//! A live view of what a TollGate node is doing.
//!
//! Reads the node's control socket and redraws. Three tabs, because the node
//! does three separable things: it carries traffic for peers, it sells the
//! vouchers that pay for it, and it holds what it is paid.
//!
//! The peers tab is deliberately organised around the thing the protocol makes
//! hard to see: **two independent payment streams per peer**. What a peer
//! bought from us and what we bought from it are different channels, different
//! mints, different windows, bought at different moments — so they get separate
//! columns rather than one netted number, because netting them would invent a
//! relationship the protocol does not have. A row is a summary; Enter opens
//! everything the node knows about that peering.
//!
//! The pricing tab is where an operator changes what the node charges. The
//! price is the one setting worth moving while a node runs — an uplink becomes
//! scarce, or stops being — and moving it touches no session, grant or channel:
//! it decides what the *next* buyer of vouchers pays, and nothing already sold.
//!
//! The wallet tab is where the money is, and the only place the display asks
//! anything of the outside world: topping up produces an invoice somebody has
//! to pay, so it is shown as a QR as well as a string. The thing paying it is
//! usually a phone.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Wrap};
use tollgate_net::control::{self, PeerSnapshot, Request, Response, Snapshot};
use tollgate_net::market::Accepted;
use tollgate_net::wallet::{Holding, Kind, TopUp};

#[derive(Parser, Debug)]
#[command(name = "tolltop", about = "Watch a TollGate node")]
struct Args {
    /// The node's control socket. Found automatically if not given.
    #[arg(short, long)]
    socket: Option<PathBuf>,

    /// How often to refresh, in milliseconds.
    #[arg(long, default_value_t = 500)]
    interval: u64,
}

/// Which tab is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Peers,
    Pricing,
    Wallet,
}

impl Tab {
    const ALL: [Tab; 3] = [Tab::Peers, Tab::Pricing, Tab::Wallet];

    fn label(self) -> &'static str {
        match self {
            Tab::Peers => "peers",
            Tab::Pricing => "pricing",
            Tab::Wallet => "wallet",
        }
    }

    fn next(self) -> Self {
        match self {
            Tab::Peers => Tab::Pricing,
            Tab::Pricing => Tab::Wallet,
            Tab::Wallet => Tab::Peers,
        }
    }

    fn previous(self) -> Self {
        match self {
            Tab::Peers => Tab::Wallet,
            Tab::Pricing => Tab::Peers,
            Tab::Wallet => Tab::Pricing,
        }
    }
}

/// Everything the display is currently doing, as opposed to what the node is.
struct App {
    socket: PathBuf,
    snapshot: Snapshot,
    /// Why the last refresh failed, if it did. The previous snapshot stays on
    /// screen: a node that is restarting should not erase what it was doing.
    error: Option<String>,
    /// Something the node refused, kept until the operator does something else.
    /// It cannot share `error`, which is recomputed every refresh and would
    /// wipe this before it was read.
    notice: Option<String>,
    tab: Tab,
    /// Which peer the cursor is on, and whether its detail is open.
    peers: TableState,
    detail: bool,
    /// Which accepted issuer the cursor is on, over on the pricing tab.
    issuers: TableState,
    /// And which of the money holdings, over on the wallet tab. Only the money
    /// takes a cursor: it is the half that can be topped up.
    holdings: TableState,
    /// `Some` while a number is being typed — a price, or an amount to top up
    /// by. Editing is modal because a keystroke that means "quit" in one mode
    /// and "9" in the other has to be told which it is.
    editing: Option<String>,
    /// An invoice waiting to be paid, held on screen until it is or the
    /// operator dismisses it. The node collects what it bought by itself.
    invoice: Option<TopUp>,
}

impl App {
    fn selected_issuer(&self) -> Option<&Accepted> {
        self.issuers
            .selected()
            .and_then(|i| self.snapshot.accepts.get(i))
    }

    fn selected(&self) -> Option<&PeerSnapshot> {
        self.peers
            .selected()
            .and_then(|i| self.snapshot.peers.get(i))
    }

    /// Keep the cursor on a row that exists.
    ///
    /// Peers come and go while the display is open, and a selection pointing
    /// past the end would show nothing while looking like it should show
    /// something.
    fn clamp_selection(&mut self) {
        let count = self.snapshot.peers.len();
        match (count, self.peers.selected()) {
            (0, _) => {
                self.peers.select(None);
                self.detail = false;
            }
            (_, None) => self.peers.select(Some(0)),
            (n, Some(i)) if i >= n => self.peers.select(Some(n - 1)),
            _ => {}
        }
    }

    /// Keep the pricing and wallet cursors on rows that exist, for the same
    /// reason: an issuer stops being accepted, a balance is spent to nothing.
    fn clamp_issuers(&mut self) {
        let (issuers, money) = (self.snapshot.accepts.len(), self.money().len());
        clamp(&mut self.issuers, issuers);
        clamp(&mut self.holdings, money);
    }

    /// Every mint this node could hold money at, held or not.
    ///
    /// A balance is not the same question as a place to top up. A node that has
    /// spent everything still buys at the mint it buys at, and one that takes
    /// four issuers' paper can be topped up at any of them — showing only what
    /// is held would mean the one moment an operator needs to add money is the
    /// one moment there is nothing to put the cursor on.
    ///
    /// So: what is held, plus the configured money mint, plus every accepted
    /// issuer that deals in money rather than in capacity. Held first, because
    /// those are the rows with something in them.
    fn money(&self) -> Vec<Holding> {
        let mut rows: Vec<Holding> = self
            .snapshot
            .holdings
            .iter()
            .filter(|h| h.kind == Kind::Money)
            .cloned()
            .collect();

        let mut add = |mint: &str, unit: &str| {
            let known = rows.iter().any(|h| {
                h.mint.trim_end_matches('/') == mint.trim_end_matches('/') && h.unit == unit
            });
            if !known {
                rows.push(Holding {
                    mint: mint.to_string(),
                    unit: unit.to_string(),
                    amount: 0,
                    kind: Kind::Money,
                });
            }
        };

        if let Some(money) = &self.snapshot.money_mint {
            add(&money.mint, &money.unit);
        }
        for accepted in &self.snapshot.accepts {
            // What this node takes as payment is denominated either in money or
            // in the resource it sells; only the first is somewhere to buy.
            if accepted.unit != self.snapshot.unit {
                add(&accepted.mint, &accepted.unit);
            }
        }
        rows
    }

    /// The money issuer under the cursor, if there is one to top up.
    fn selected_money(&self) -> Option<Holding> {
        self.holdings
            .selected()
            .and_then(|i| self.money().get(i).cloned())
    }

    fn move_issuer(&mut self, delta: isize) {
        move_cursor(&mut self.issuers, self.snapshot.accepts.len(), delta);
    }

    fn move_holding(&mut self, delta: isize) {
        let count = self.money().len();
        move_cursor(&mut self.holdings, count, delta);
    }

    fn move_selection(&mut self, delta: isize) {
        move_cursor(&mut self.peers, self.snapshot.peers.len(), delta);
    }
}

/// Point a cursor at a row that exists, or at nothing when there are none.
fn clamp(state: &mut TableState, count: usize) {
    match (count, state.selected()) {
        (0, _) => state.select(None),
        (_, None) => state.select(Some(0)),
        (n, Some(i)) if i >= n => state.select(Some(n - 1)),
        _ => {}
    }
}

/// Move a cursor, stopping at either end rather than wrapping.
///
/// Wrapping in a list this short reads as the cursor jumping rather than as
/// reaching the end.
fn move_cursor(state: &mut TableState, count: usize, delta: isize) {
    if count == 0 {
        return;
    }
    let current = state.selected().unwrap_or(0) as isize;
    state.select(Some((current + delta).clamp(0, count as isize - 1) as usize));
}

fn main() -> Result<()> {
    let args = Args::parse();
    // Looked for rather than assumed: a node run by a service manager puts its
    // socket in /run, one run by a person puts it under XDG_RUNTIME_DIR, and a
    // tool that only knew about /tmp would report "no such file" about a node
    // that is running perfectly well.
    let socket = match args.socket {
        Some(path) => path,
        None => control::find_socket()?,
    };
    let interval = Duration::from_millis(args.interval.max(50));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    let mut terminal = ratatui::init();
    // A full repaint of a known-blank screen before the first draw. Entering
    // the alternate screen does not clear it, and the first draw only emits
    // cells that differ from an assumed-blank buffer — so on a terminal that
    // hands back an alternate buffer with something already in it (tmux, and
    // most things over ssh) the old contents show through the gaps.
    let _ = terminal.clear();

    let mut app = App {
        socket,
        snapshot: Snapshot::default(),
        error: None,
        notice: None,
        tab: Tab::Peers,
        peers: TableState::default(),
        detail: false,
        issuers: TableState::default(),
        holdings: TableState::default(),
        editing: None,
        invoice: None,
    };

    loop {
        match runtime.block_on(control::fetch(&app.socket)) {
            Ok(fresh) => {
                app.snapshot = fresh;
                app.error = None;
            }
            Err(e) => app.error = Some(format!("{e:#}")),
        }
        app.clamp_selection();
        app.clamp_issuers();

        terminal.draw(|frame| draw(frame, &mut app))?;

        if event::poll(interval)?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            // Editing takes every key it can use before anything else looks at
            // one: while a price is half-typed, `q` is not "quit".
            if let Some(text) = app.editing.as_mut() {
                match key.code {
                    KeyCode::Esc => app.editing = None,
                    KeyCode::Enter => {
                        let typed = text.trim().parse::<u64>();
                        match (app.tab, typed) {
                            (Tab::Pricing, Ok(bytes_per_unit)) => {
                                app.notice = match app.selected_issuer().cloned() {
                                    Some(issuer) => runtime
                                        .block_on(set_price(&app.socket, &issuer, bytes_per_unit))
                                        .err()
                                        .map(|e| format!("{e:#}")),
                                    None => None,
                                };
                            }
                            (Tab::Wallet, Ok(amount)) => {
                                // Top up the issuer under the cursor, or the
                                // node's own money mint when nothing is held
                                // yet and there is no cursor to be under.
                                let at = app.selected_money();
                                match runtime.block_on(top_up(&app.socket, amount, at.as_ref())) {
                                    Ok(invoice) => {
                                        app.invoice = Some(invoice);
                                        app.notice = None;
                                    }
                                    Err(e) => app.notice = Some(format!("{e:#}")),
                                }
                            }
                            // Nothing typed is a change of mind, not a zero:
                            // refusing an issuer is worth typing a `0` for.
                            _ => app.notice = None,
                        }
                        app.editing = None;
                    }
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Char(c) if c.is_ascii_digit() => text.push(c),
                    _ => {}
                }
                continue;
            }

            match key.code {
                KeyCode::Char('q') => break,
                KeyCode::Tab | KeyCode::Right => {
                    app.tab = app.tab.next();
                    app.notice = None;
                }
                KeyCode::BackTab | KeyCode::Left => {
                    app.tab = app.tab.previous();
                    app.notice = None;
                }
                // Esc backs out of whatever is open, and quits from the top.
                KeyCode::Esc if app.invoice.is_some() => app.invoice = None,
                KeyCode::Esc if app.detail => app.detail = false,
                KeyCode::Esc => break,

                KeyCode::Up | KeyCode::Char('k') if app.tab == Tab::Peers => app.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') if app.tab == Tab::Peers => {
                    app.move_selection(1)
                }
                KeyCode::Enter if app.tab == Tab::Peers => {
                    app.detail = !app.detail && app.selected().is_some();
                }

                KeyCode::Up | KeyCode::Char('k') if app.tab == Tab::Wallet => app.move_holding(-1),
                KeyCode::Down | KeyCode::Char('j') if app.tab == Tab::Wallet => app.move_holding(1),

                KeyCode::Up | KeyCode::Char('k') if app.tab == Tab::Pricing => app.move_issuer(-1),
                KeyCode::Down | KeyCode::Char('j') if app.tab == Tab::Pricing => app.move_issuer(1),

                // On the pricing tab, editing is the only thing to do, so
                // Enter starts it as well as `e`.
                KeyCode::Enter | KeyCode::Char('e') if app.tab == Tab::Pricing => {
                    if let Some(issuer) = app.selected_issuer() {
                        app.editing = Some(issuer.bytes_per_unit.to_string());
                        app.notice = None;
                    }
                }

                // Topping up is the only thing to do on the wallet tab, so
                // Enter starts it as well as `t`.
                KeyCode::Enter | KeyCode::Char('t') if app.tab == Tab::Wallet => {
                    app.editing = Some(String::new());
                    app.invoice = None;
                    app.notice = None;
                }

                // Collect an invoice that was paid while nothing was watching
                // for it — a node restarted mid-wait, or a mint that
                // rate-limited the status polls until the waiter gave up.
                KeyCode::Char('c') if app.tab == Tab::Wallet => {
                    let at = app.selected_money();
                    app.notice = match runtime.block_on(claim(&app.socket, at.as_ref())) {
                        Ok(0) => Some("nothing paid for and unclaimed".into()),
                        Ok(claimed) => Some(format!("claimed {claimed}")),
                        Err(e) => Some(format!("{e:#}")),
                    };
                    app.invoice = None;
                }
                _ => {}
            }
        }
    }

    ratatui::restore();
    Ok(())
}

/// Ask the node to buy money, and hand back the invoice that pays for it.
///
/// `at` is the holding under the cursor. Without one — a wallet that holds
/// nothing yet — the node uses its own money mint, so that an operator topping
/// up for the first time does not have to know a URL to do it.
async fn top_up(socket: &std::path::Path, amount: u64, at: Option<&Holding>) -> Result<TopUp> {
    let request = Request::TopUp {
        amount,
        mint: at.map(|h| h.mint.clone()),
        unit: at.map(|h| h.unit.clone()).unwrap_or_else(|| "sat".into()),
    };
    match control::send(socket, &request).await? {
        Response::Ok { data } => Ok(serde_json::from_value(data)?),
        Response::Error { message } => Err(anyhow::anyhow!(message)),
    }
}

/// Ask the node to collect anything paid for and not yet claimed.
async fn claim(socket: &std::path::Path, at: Option<&Holding>) -> Result<u64> {
    let request = Request::Claim {
        mint: at.map(|h| h.mint.clone()),
        unit: at.map(|h| h.unit.clone()).unwrap_or_else(|| "sat".into()),
    };
    match control::send(socket, &request).await? {
        Response::Ok { data } => Ok(data
            .get("claimed")
            .and_then(|c| c.as_u64())
            .unwrap_or_default()),
        Response::Error { message } => Err(anyhow::anyhow!(message)),
    }
}

/// Tell the node what one issuer's paper buys, and fail loudly if it will not.
async fn set_price(socket: &std::path::Path, issuer: &Accepted, bytes_per_unit: u64) -> Result<()> {
    let request = Request::SetPrice {
        mint: issuer.mint.clone(),
        unit: issuer.unit.clone(),
        bytes_per_unit,
    };
    match control::send(socket, &request).await? {
        Response::Ok { .. } => Ok(()),
        Response::Error { message } => Err(anyhow::anyhow!(message)),
    }
}

fn draw(frame: &mut Frame, app: &mut App) {
    let [tabs, body, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(frame.area());

    frame.render_widget(tab_bar(app), tabs);
    match app.tab {
        Tab::Peers => peers_tab(frame, app, body),
        Tab::Pricing => pricing_tab(frame, app, body),
        Tab::Wallet => wallet_tab(frame, app, body),
    }
    frame.render_widget(status(app), footer);
}

/// The tabs, and nothing else. What the node *is* belongs in the status bar,
/// where it stays visible whichever tab is open.
fn tab_bar(app: &App) -> Paragraph<'_> {
    let dim = Style::default().fg(Color::DarkGray);
    let mut spans = Vec::new();
    for (i, tab) in Tab::ALL.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled(" | ", dim));
        }
        spans.push(Span::styled(
            tab.label(),
            if *tab == app.tab {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            },
        ));
    }

    Paragraph::new(Line::from(spans))
        .block(Block::default().borders(Borders::ALL).title(" tolltop "))
}

/// The peers table, with the detail beside it rather than instead of it.
///
/// Split rather than swapped: the row a detail belongs to is context for
/// reading it, and losing the table to open one peering means losing sight of
/// how it compares to the others.
fn peers_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.detail {
        let [left, right] =
            Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)])
                .areas(area);
        // Half the width cannot hold eleven columns, so the table drops to
        // the five that say whether this peering is working. The rest of them
        // are in the panel beside it anyway.
        peer_table(frame, app, left, true);
        frame.render_widget(peer_detail(app), right);
    } else {
        peer_table(frame, app, area, false);
    }
}

fn peer_table(frame: &mut Frame, app: &mut App, area: Rect, compact: bool) {
    let titles: &[&str] = if compact {
        &["peer", "access", "sold", "left", "bought"]
    } else {
        &[
            "peer", "access", // what they bought from us
            "sold", "left", "in", "up", // what we bought from them
            "bought", "want", "m", "out", // and what has actually moved
            "taken",
        ]
    };
    let widths: &[Constraint] = if compact {
        &[
            Constraint::Length(10), // peer
            Constraint::Length(11), // access
            Constraint::Length(12), // sold
            Constraint::Length(7),  // left
            Constraint::Length(12), // bought
        ]
    } else {
        &[
            Constraint::Length(10), // peer
            Constraint::Length(11), // access
            Constraint::Length(12), // sold
            Constraint::Length(7),  // left
            Constraint::Length(14), // in (channel)
            Constraint::Length(12), // up
            Constraint::Length(12), // bought
            Constraint::Length(12), // want
            Constraint::Length(3),  // m
            Constraint::Length(14), // out (channel)
            Constraint::Length(12), // taken (a total)
        ]
    };

    let rows: Vec<Row> = if compact {
        app.snapshot.peers.iter().map(compact_peer_row).collect()
    } else {
        app.snapshot.peers.iter().map(peer_row).collect()
    };

    let title = if compact {
        " peers ".to_string()
    } else {
        " peers — left of the divide is what they bought from us, right is what we bought from them "
            .to_string()
    };

    let table = Table::new(rows, widths.to_vec())
        .header(header_row(titles))
        .row_highlight_style(SELECTED)
        .highlight_symbol(CURSOR)
        .block(Block::default().borders(Borders::ALL).title(title));

    frame.render_stateful_widget(table, area, &mut app.peers);
}

/// How the row under the cursor is drawn.
///
/// Reversed rather than given a background colour of its own. A coloured bar is
/// what a header looks like, and one sitting under another was unreadable as a
/// cursor — worse when there was a single row, where a permanently highlighted
/// line reads as decoration rather than as a thing that moves.
const SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);

/// And the mark in front of it, so a cursor is a cursor even where the terminal
/// renders reversed video badly.
const CURSOR: &str = "▶ ";

/// A column header: a label, not a selection.
fn header_row<'a>(titles: &'a [&'a str]) -> Row<'a> {
    Row::new(titles.iter().map(|t| Cell::from(*t))).style(
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD),
    )
}

/// The five columns that say whether a peering is working, for when the detail
/// panel has taken the rest of the width.
fn compact_peer_row(peer: &PeerSnapshot) -> Row<'_> {
    Row::new(vec![
        Cell::from(short(&peer.pubkey)),
        Cell::from(Span::styled(
            peer.access.clone(),
            access_style(&peer.access),
        )),
        Cell::from(rate(peer.shaped_rate)),
        Cell::from(grant_left(peer)),
        Cell::from(rate(peer.bought_rate)),
    ])
}

/// How long the grant in force has left.
///
/// A grant that has lapsed leaves the peer on the allowance, which is a normal
/// resting state rather than a fault — so it is dimmed, not red.
fn grant_left(peer: &PeerSnapshot) -> Span<'static> {
    if peer.grant_expires_in_ms == 0 {
        Span::styled("lapsed", Style::default().fg(Color::DarkGray))
    } else {
        Span::raw(format!("{:.1}s", peer.grant_expires_in_ms as f64 / 1000.0))
    }
}

fn peer_row(peer: &PeerSnapshot) -> Row<'_> {
    Row::new(vec![
        Cell::from(short(&peer.pubkey)),
        // Colour carries the one thing worth noticing at a glance: whether
        // traffic is flowing for this peer at all.
        Cell::from(Span::styled(
            peer.access.clone(),
            access_style(&peer.access),
        )),
        Cell::from(rate(peer.shaped_rate)),
        Cell::from(grant_left(peer)),
        Cell::from(channels(&peer.incoming_channels)),
        Cell::from(rate(peer.upload_rate)),
        Cell::from(rate(peer.bought_rate)),
        Cell::from(rate(peer.demand)),
        Cell::from(peer.received_multiplier.to_string()),
        Cell::from(
            peer.outgoing_channel
                .as_ref()
                .map(|c| {
                    let mut text = channel(c);
                    if peer.rollover_ready {
                        // A replacement is funded and waiting for this one to
                        // fill — worth showing, since it is the thing that
                        // keeps buying from stalling at a channel boundary.
                        text.push('+');
                    }
                    text
                })
                .unwrap_or_else(|| "—".into()),
        ),
        // A total, not a rate. Nothing measures a receive rate the way
        // `upload_rate` measures the sending one, and formatting a cumulative
        // counter with a `/s` on the end made it read as one — a peering that
        // had taken 178 KiB looked like it was taking 178 KiB every second.
        Cell::from(units(peer.received)),
    ])
}

/// One peering, in full.
///
/// The table has to fit eleven columns across a terminal, so it shows rates and
/// elides the rest. This is where the rest goes: the whole key, the phase, the
/// cumulative totals, and every channel rather than the first one.
fn peer_detail(app: &App) -> Paragraph<'_> {
    let Some(peer) = app.selected() else {
        return Paragraph::new("no peer selected")
            .block(Block::default().borders(Borders::ALL).title(" peer "));
    };

    let dim = Style::default().fg(Color::DarkGray);
    let field = |name: &'static str, value: String| {
        Line::from(vec![
            Span::styled(format!("{name:<20}"), dim),
            Span::raw(value),
        ])
    };

    let mut lines = vec![
        Line::from(vec![
            Span::styled("peer ", dim),
            Span::styled(
                peer.pubkey.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(format!("{:<20}", "access"), dim),
            Span::styled(peer.access.clone(), access_style(&peer.access)),
            Span::styled("   phase ", dim),
            Span::raw(peer.phase.clone()),
        ]),
        Line::from(""),
        // --- what they bought from us -------------------------------------
        Line::from(Span::styled(
            "what this peer bought from us",
            Style::default().fg(Color::Cyan),
        )),
        field("shaped rate", rate(peer.shaped_rate)),
        field(
            "grant expires in",
            if peer.grant_expires_in_ms == 0 {
                "lapsed — on the allowance".into()
            } else {
                format!("{:.1}s", peer.grant_expires_in_ms as f64 / 1000.0)
            },
        ),
        field("authorized", units(peer.authorized)),
        field("consumed", units(peer.consumed)),
        field("delivered to them", units(peer.delivered)),
        field("they are pushing", rate(peer.upload_rate)),
    ];

    if peer.incoming_channels.is_empty() {
        lines.push(field("channels on", "none".into()));
    } else {
        lines.push(Line::from(Span::styled(
            format!("{:<20}", "channels on"),
            dim,
        )));
        for c in &peer.incoming_channels {
            lines.push(Line::from(format!("  {}", channel_detail(c))));
        }
    }

    // --- what we bought from them -----------------------------------------
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "what we bought from this peer",
        Style::default().fg(Color::Cyan),
    )));
    lines.push(field("bought rate", rate(peer.bought_rate)));
    lines.push(field("demand we observe", rate(peer.demand)));
    lines.push(field("received from them", units(peer.received)));
    lines.push(field(
        "their surcharge",
        format!("{}x on what we push", peer.received_multiplier),
    ));
    lines.push(field(
        "channel we pay on",
        match &peer.outgoing_channel {
            Some(c) => channel_detail(c),
            None => "none".into(),
        },
    ));
    lines.push(field(
        "replacement funded",
        if peer.rollover_ready {
            "yes — waiting for the one in use to fill".into()
        } else {
            "no".into()
        },
    ));

    Paragraph::new(lines).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" peer {} ", short(&peer.pubkey))),
    )
}

/// What this node takes as payment, and what a unit of it buys.
///
/// A list rather than a number, because the price of capacity is not one price:
/// a sat from a mint you expect to honour its tokens is worth more than a sat
/// from one you do not, and pricing issuers is how an operator says so.
fn pricing_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    if app.snapshot.accepts.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled(
                "This node takes no paper as payment.",
                Style::default().fg(Color::Red),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "Nobody can buy its vouchers here, which is a working setting for a",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "node whose capacity is sold somewhere else. Add issuers under",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "`market.accept` to take payment.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" pricing "));
        frame.render_widget(empty, area);
        return;
    }

    let rows: Vec<Row> = app
        .snapshot
        .accepts
        .iter()
        .map(|a| {
            Row::new(vec![
                Cell::from(a.mint.clone()),
                Cell::from(a.unit.clone()),
                Cell::from(a.bytes_per_unit.to_string()),
                Cell::from(costs(1_000_000_000, a)),
                Cell::from(costs(1_000_000, a)),
            ])
        })
        .collect();

    let table = Table::new(
        rows,
        [
            Constraint::Min(24),    // issuer
            Constraint::Length(6),  // unit
            Constraint::Length(15), // bytes per unit
            Constraint::Length(14), // a gigabyte
            Constraint::Length(14), // a megabyte
        ],
    )
    .header(header_row(&[
        "issuer",
        "unit",
        "bytes per unit",
        "a gigabyte",
        "a megabyte",
    ]))
    .row_highlight_style(SELECTED)
    .highlight_symbol(CURSOR)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" pricing — what this node takes, and what it gives for it "),
    );

    frame.render_stateful_widget(table, area, &mut app.issuers);
}

/// What this node is holding.
///
/// Money first, then other people's vouchers, because they are not the same
/// kind of thing: money is what this node can pay with, and a peer's vouchers
/// are what it has been paid — a claim on that peer's capacity that nobody but
/// that peer will honour.
fn wallet_tab(frame: &mut Frame, app: &mut App, area: Rect) {
    // An invoice takes the whole tab while it is unpaid: it is the one thing on
    // screen the operator has to act on, and a QR wants the room.
    if let Some(invoice) = &app.invoice {
        invoice_panel(frame, invoice, area);
        return;
    }

    // Every mint this node could hold money at, whether or not it holds any —
    // an empty row is where a top-up goes.
    let money = app.money();
    let transit: Vec<Holding> = app
        .snapshot
        .holdings
        .iter()
        .filter(|h| h.kind == Kind::PrepaidTransit)
        .cloned()
        .collect();

    if money.is_empty() && transit.is_empty() {
        let empty = Paragraph::new(vec![
            Line::from(Span::styled(
                "This node holds nothing, and has nowhere to buy.",
                Style::default().fg(Color::Yellow),
            )),
            Line::from(""),
            Line::from(Span::styled(
                "It can still sell — it is paid in money it can hold — but it cannot buy",
                Style::default().fg(Color::DarkGray),
            )),
            Line::from(Span::styled(
                "transit from an upstream until `wallet.mint` names a mint to buy at.",
                Style::default().fg(Color::DarkGray),
            )),
        ])
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(" wallet "));
        frame.render_widget(empty, area);
        return;
    }

    // Two tables rather than one with a column saying which is which. Both are
    // spending power; the difference is who will take it. Money buys from
    // whoever accepts its mint, and a byte-denominated holding is capacity
    // already bought from one upstream — good there and nowhere else.
    //
    // Only the money table takes the cursor, because only it is actionable: `t`
    // tops up the issuer under it, and there is no topping up an upstream's
    // capacity except by buying transit, which the node does for itself.
    let [top, bottom] = Layout::vertical([
        Constraint::Length(money.len().max(1) as u16 + 3),
        Constraint::Min(3),
    ])
    .areas(area);

    frame.render_stateful_widget(
        holdings_table(
            &money,
            " money — buys from any peer that accepts the mint ",
            Color::Green,
        )
        .row_highlight_style(SELECTED)
        .highlight_symbol(CURSOR),
        top,
        &mut app.holdings,
    );
    frame.render_widget(
        holdings_table(
            &transit,
            " prepaid transit — bought from an upstream, spendable only there ",
            Color::Cyan,
        ),
        bottom,
    );
}

fn holdings_table<'a>(holdings: &[Holding], title: &'a str, colour: Color) -> Table<'a> {
    let rows: Vec<Row> = if holdings.is_empty() {
        vec![Row::new(vec![Cell::from(Span::styled(
            "—",
            Style::default().fg(Color::DarkGray),
        ))])]
    } else {
        holdings
            .iter()
            .map(|h| {
                // A mint with nothing in it is a place to put money rather than
                // money, so it is dimmed instead of coloured: the cursor can
                // still land on it, which is the point.
                let amount = if h.amount == 0 {
                    Span::styled("—", Style::default().fg(Color::DarkGray))
                } else {
                    Span::styled(
                        held(h),
                        Style::default().fg(colour).add_modifier(Modifier::BOLD),
                    )
                };
                Row::new(vec![
                    Cell::from(h.mint.clone()),
                    Cell::from(h.unit.clone()),
                    Cell::from(amount),
                ])
            })
            .collect()
    };

    Table::new(
        rows,
        [
            Constraint::Min(24),    // issuer
            Constraint::Length(6),  // unit
            Constraint::Length(16), // held
        ],
    )
    .header(header_row(&["issuer", "unit", "held"]))
    .block(Block::default().borders(Borders::ALL).title(title))
}

/// A holding, in the units its own kind is read in.
///
/// Money is counted: 900 sat is 900 sat, and rounding it to "0.9k" hides the
/// thing being counted. Capacity is measured, so it reads as bytes do
/// everywhere else in this display.
fn held(holding: &Holding) -> String {
    match holding.kind {
        Kind::Money => format!("{} {}", holding.amount, holding.unit),
        Kind::PrepaidTransit => units(holding.amount),
    }
}

/// The invoice for a top-up, and nothing else on screen.
///
/// A QR beside the string, because the thing paying is a phone. The string
/// stays: a QR is no use over ssh into a terminal being read through a scroll
/// buffer, and it is what an operator copies into a wallet on the same machine.
fn invoice_panel(frame: &mut Frame, invoice: &TopUp, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" top up — esc to dismiss ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().fg(Color::DarkGray);
    let header = Paragraph::new(vec![
        Line::from(vec![
            Span::styled("buying ", dim),
            Span::styled(
                format!("{} {}", invoice.amount, invoice.unit),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(" at ", dim),
            Span::raw(invoice.mint.clone()),
        ]),
        Line::from(Span::styled(
            "Scan or paste. The node collects what it buys by itself, and the \
             balance appears when it lands — there is nothing to confirm.",
            dim,
        )),
    ])
    .wrap(Wrap { trim: false });

    let qr = qr_lines(&invoice.request);
    // Height first: a QR that does not fit whole is not a QR, so if the pane is
    // too short the string gets the room instead.
    let qr_height = qr.len() as u16;
    let qr_width = qr.iter().map(|l| l.chars().count()).max().unwrap_or(0) as u16;
    let fits = qr_height + 3 <= inner.height && qr_width + 20 <= inner.width;

    let [head, body] = Layout::vertical([Constraint::Length(3), Constraint::Min(1)]).areas(inner);
    frame.render_widget(header, head);

    if !fits {
        let mut lines = vec![Line::from(Span::styled(
            invoice.request.clone(),
            Style::default().fg(Color::Yellow),
        ))];
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("(a {qr_width}×{qr_height} QR needs a taller window)"),
            dim,
        )));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
        return;
    }

    let [left, right] =
        Layout::horizontal([Constraint::Length(qr_width + 2), Constraint::Min(20)]).areas(body);

    // Black on white, whatever the terminal's own colours are: a scanner needs
    // dark modules on a light field, and a QR drawn in the terminal's
    // foreground colour on its background is as likely to be the negative.
    frame.render_widget(
        Paragraph::new(
            qr.into_iter()
                .map(|line| Line::from(Span::raw(line)))
                .collect::<Vec<_>>(),
        )
        .style(Style::default().fg(Color::Black).bg(Color::White)),
        left,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(
            invoice.request.clone(),
            Style::default().fg(Color::Yellow),
        ))
        .wrap(Wrap { trim: false }),
        right,
    );
}

/// A bolt11 invoice as terminal-sized QR rows.
///
/// Uppercased first. A bech32 invoice is case-insensitive, and uppercase is
/// what lets the encoder use alphanumeric mode instead of binary — roughly a
/// third fewer modules, which is the difference between a QR that fits an
/// ordinary window and one that does not. Two module rows per line of text,
/// for the same reason.
fn qr_lines(invoice: &str) -> Vec<String> {
    use qrcode::QrCode;
    use qrcode::render::unicode;

    let Ok(code) = QrCode::new(invoice.to_uppercase().as_bytes()) else {
        return Vec::new();
    };
    code.render::<unicode::Dense1x2>()
        .quiet_zone(true)
        .build()
        .lines()
        .map(str::to_string)
        .collect()
}

/// What a quantity costs in one issuer's paper.
fn costs(bytes: u64, issuer: &Accepted) -> String {
    if issuer.bytes_per_unit == 0 {
        return "—".into();
    }
    let units = bytes as f64 / issuer.bytes_per_unit as f64;
    if units >= 1.0 {
        format!("{units:.0} {}", issuer.unit)
    } else {
        format!("{units:.3} {}", issuer.unit)
    }
}

fn status(app: &App) -> Paragraph<'static> {
    let dim = Style::default().fg(Color::DarkGray);

    // Editing wins over everything: the operator is mid-keystroke, and what
    // they need to see is what they have typed so far.
    if let Some(text) = &app.editing {
        // What a number means depends on which tab asked for it.
        let (prompt, help) = match app.tab {
            Tab::Wallet => ("top up by (sat): ", "   enter to buy, esc to cancel"),
            _ => (
                "bytes per unit: ",
                "   enter to set, esc to cancel, 0 to stop taking this issuer",
            ),
        };
        return Paragraph::new(Line::from(vec![
            Span::styled(prompt, Style::default().fg(Color::Yellow)),
            Span::styled(
                format!("{text}_"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::styled(help, dim),
        ]))
        .block(Block::default().borders(Borders::ALL));
    }

    if let Some(message) = app.notice.as_ref().or(app.error.as_ref()) {
        return Paragraph::new(Line::from(Span::styled(
            format!("{}: {message}", app.socket.display()),
            Style::default().fg(Color::Red),
        )))
        .block(Block::default().borders(Borders::ALL));
    }

    let hints = match (app.tab, app.detail, app.invoice.is_some()) {
        (Tab::Peers, false, _) => "tab switches   ↑↓ selects   enter opens   q quits",
        (Tab::Peers, true, _) => "esc closes   ↑↓ selects   tab switches   q quits",
        (Tab::Pricing, _, _) => "↑↓ selects   e changes the price   tab switches   q quits",
        (Tab::Wallet, _, true) => "esc dismisses   c claims a paid invoice   q quits",
        (Tab::Wallet, _, false) => "t tops up   c claims a paid invoice   tab switches   q quits",
    };

    // Which node this is, and what it charges, stay visible on every tab: the
    // tab bar above says what is being looked at, not what is being looked at
    // *on*.
    Paragraph::new(Line::from(vec![
        Span::styled(
            short(&app.snapshot.pubkey),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled("  selling ", dim),
        Span::raw(app.snapshot.unit.clone()),
        Span::styled("  takes ", dim),
        Span::raw(match app.snapshot.accepts.len() {
            0 => "nothing".to_string(),
            1 => "1 issuer".to_string(),
            n => format!("{n} issuers"),
        }),
        Span::styled("  up ", dim),
        Span::raw(duration(app.snapshot.uptime_ms)),
        Span::styled("  ", dim),
        Span::styled(
            format!("{} peer(s)", app.snapshot.peers.len()),
            Style::default().fg(Color::Green),
        ),
        Span::styled(format!("   {hints}"), dim),
    ]))
    .block(Block::default().borders(Borders::ALL))
}

fn access_style(access: &str) -> Style {
    match access {
        "active" => Style::default().fg(Color::Green),
        "free" => Style::default().fg(Color::Cyan),
        "suspended" => Style::default().fg(Color::Red),
        _ => Style::default().fg(Color::DarkGray),
    }
}

/// How full a channel is. The number that matters is how close it is to needing
/// a rollover, not its absolute size.
fn channel(c: &control::ChannelSnapshot) -> String {
    let pct = if c.capacity == 0 {
        0
    } else {
        (c.signed as u128 * 100 / c.capacity as u128) as u64
    };
    format!("{} {pct}%", c.id)
}

fn channel_detail(c: &control::ChannelSnapshot) -> String {
    format!(
        "{} — {} of {} signed",
        channel(c),
        units(c.signed),
        units(c.capacity)
    )
}

fn channels(list: &[control::ChannelSnapshot]) -> String {
    match list {
        [] => "—".into(),
        [one] => channel(one),
        // Several at once is a rollover in flight, or a payer spending from
        // more than one accepted mint.
        many => format!("{} +{}", channel(&many[0]), many.len() - 1),
    }
}

/// Binary units, because the unit sold is the byte and grants decompose into
/// power-of-two proofs.
fn rate(bytes_per_second: u64) -> String {
    match bytes_per_second {
        0 => "—".into(),
        n => format!("{}/s", units(n)),
    }
}

fn units(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let v = bytes as f64;
    if v >= GIB {
        format!("{:.2} GiB", v / GIB)
    } else if v >= MIB {
        format!("{:.2} MiB", v / MIB)
    } else if v >= KIB {
        format!("{:.1} KiB", v / KIB)
    } else {
        format!("{bytes} B")
    }
}

fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn short(hex_key: &str) -> String {
    hex_key.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real bolt11 invoice, from the fake mint the topologies run.
    const INVOICE: &str = "lnbc2500n1p48t9kydqqpp5atkdtq5tetqkr3acy4cxhgzc0pn9lw7pyahumh70g43qy9qt2r9qsp59g4z52329g4z52329g4z52329g4z52329g4z52329g4z52329g4q9qrsgq";

    #[test]
    fn an_invoice_becomes_a_square_qr() {
        let lines = qr_lines(INVOICE);
        assert!(!lines.is_empty(), "an invoice should encode");

        // Two module rows per line of text, so the drawing is half as tall as
        // it is wide — give or take the odd row.
        let width = lines.iter().map(|l| l.chars().count()).max().unwrap();
        let height = lines.len();
        assert!(
            height * 2 >= width - 2 && height * 2 <= width + 2,
            "{width}×{height} is not a square QR"
        );
        assert!(
            width < 80,
            "{width} columns will not fit an ordinary window"
        );
    }

    #[test]
    fn uppercasing_the_invoice_makes_the_qr_smaller() {
        // Bech32 is case-insensitive, and uppercase is what lets the encoder
        // use alphanumeric mode instead of binary. It is the difference between
        // a QR that fits a window and one that does not.
        use qrcode::QrCode;
        let binary = QrCode::new(INVOICE.as_bytes()).expect("encode").width();
        let alphanumeric = QrCode::new(INVOICE.to_uppercase().as_bytes())
            .expect("encode")
            .width();
        assert!(
            alphanumeric < binary,
            "uppercase {alphanumeric} should beat mixed case {binary}"
        );
    }

    #[test]
    fn something_that_will_not_encode_is_not_a_panic() {
        // 4 kB is past what any QR version holds. A display that fell over
        // because a mint wrote a long invoice would be worse than one showing
        // the string alone.
        assert!(qr_lines(&"x".repeat(4096)).is_empty());
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use tollgate_net::wallet::Holding;

    fn app_with_two_issuers() -> App {
        let mut app = App {
            socket: PathBuf::from("/tmp/x.sock"),
            snapshot: Snapshot {
                pubkey: "02aa".into(),
                unit: "byte".into(),
                accepts: vec![
                    Accepted {
                        mint: "https://one.example".into(),
                        unit: "sat".into(),
                        bytes_per_unit: 1_000_000,
                    },
                    Accepted {
                        mint: "https://two.example".into(),
                        unit: "sat".into(),
                        bytes_per_unit: 500_000,
                    },
                ],
                holdings: vec![
                    Holding {
                        mint: "https://one.example".into(),
                        unit: "sat".into(),
                        amount: 900,
                        kind: Kind::Money,
                    },
                    // Bought from an upstream and not yet spent — spending
                    // power, but only at the node that issued it.
                    Holding {
                        mint: "https://upstream.example".into(),
                        unit: "byte".into(),
                        amount: 4096,
                        kind: Kind::PrepaidTransit,
                    },
                ],
                ..Snapshot::default()
            },
            error: None,
            notice: None,
            tab: Tab::Pricing,
            peers: TableState::default(),
            detail: false,
            issuers: TableState::default(),
            holdings: TableState::default(),
            editing: None,
            invoice: None,
        };
        app.clamp_issuers();
        app
    }

    fn render(app: &mut App) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).expect("terminal");
        terminal.draw(|frame| draw(frame, app)).expect("draw");
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect()
    }

    /// The rows the cursor is on, by what they contain.
    fn cursored(screen: &[String]) -> Vec<String> {
        screen
            .iter()
            .filter(|line| line.contains(CURSOR.trim()))
            .map(|line| line.trim().to_string())
            .collect()
    }

    #[test]
    fn the_cursor_is_on_a_row_rather_than_on_the_header() {
        // The bug this replaces: the header was a solid grey bar and the
        // selected row was another one just under it, so the header read as the
        // selection and nothing looked like a cursor at all.
        let mut app = app_with_two_issuers();
        let screen = render(&mut app);

        let marked = cursored(&screen);
        assert_eq!(marked.len(), 1, "exactly one row carries the cursor");
        assert!(marked[0].contains("one.example"), "{:?}", marked);

        let header = screen
            .iter()
            .find(|l| l.contains("bytes per unit"))
            .expect("a header");
        assert!(
            !header.contains(CURSOR.trim()),
            "the header is a label, not a selection: {header:?}"
        );
    }

    #[test]
    fn the_cursor_moves_between_issuers_and_stops_at_the_ends() {
        let mut app = app_with_two_issuers();

        app.move_issuer(1);
        assert!(cursored(&render(&mut app))[0].contains("two.example"));

        // Past the end stays at the end rather than wrapping, which in a list
        // this short reads as the cursor jumping.
        app.move_issuer(1);
        assert!(cursored(&render(&mut app))[0].contains("two.example"));

        app.move_issuer(-5);
        assert!(cursored(&render(&mut app))[0].contains("one.example"));
    }

    #[test]
    fn every_mint_money_could_be_bought_at_gets_a_row() {
        // The one moment an operator needs to add money is the moment there is
        // none — so a mint with an empty balance still has to be on screen and
        // still has to take the cursor, or there is nothing to press `t` on.
        let mut app = app_with_two_issuers();
        app.tab = Tab::Wallet;
        app.snapshot.money_mint = Some(Accepted {
            mint: "https://money.example".into(),
            unit: "sat".into(),
            bytes_per_unit: 0,
        });
        app.clamp_issuers();

        let screen = render(&mut app).join("\n");
        assert!(screen.contains("one.example"), "the held one: {screen}");
        assert!(screen.contains("money.example"), "the empty one: {screen}");

        // And the cursor reaches it.
        app.move_holding(1);
        let marked = cursored(&render(&mut app));
        assert_eq!(marked.len(), 1, "{marked:?}");
        assert!(
            marked[0].contains("money.example"),
            "the cursor should reach a mint with nothing in it: {marked:?}"
        );
        assert_eq!(
            app.selected_money().map(|h| h.amount),
            Some(0),
            "and topping up there is what puts the first sat in it"
        );
    }

    #[test]
    fn an_issuer_priced_in_the_resource_is_not_somewhere_to_buy_money() {
        // `market.accept` holds both kinds: mints whose paper is money, and
        // mints whose paper is capacity. Only the first is a place to top up.
        let mut app = app_with_two_issuers();
        app.snapshot.accepts.push(Accepted {
            mint: "https://upstream.example".into(),
            unit: "byte".into(),
            bytes_per_unit: 1,
        });

        let money = app.money();
        assert!(
            !money.iter().any(|h| h.mint.contains("upstream.example")),
            "{money:?}"
        );
    }

    #[test]
    fn the_wallet_cursor_is_on_the_money_and_not_on_prepaid_transit() {
        // Only the money is actionable: `t` tops up the issuer under the
        // cursor, and an upstream's capacity is topped up by buying transit,
        // which the node does for itself.
        let mut app = app_with_two_issuers();
        app.tab = Tab::Wallet;
        let screen = render(&mut app);

        let marked = cursored(&screen);
        assert_eq!(marked.len(), 1, "{marked:?}");
        assert!(marked[0].contains("one.example"), "{:?}", marked);
        assert_eq!(
            app.selected_money().map(|h| h.unit),
            Some("sat".to_string())
        );

        let transit = screen
            .iter()
            .find(|l| l.contains("upstream.example"))
            .expect("the prepaid-transit row");
        assert!(!transit.contains(CURSOR.trim()), "{transit:?}");
    }

    #[test]
    fn the_two_halves_say_who_will_take_them() {
        // The labels are the point. Both halves are spending power; what
        // differs is whether it is general or good at exactly one node — and
        // neither of them is "what peers have paid us", because a peer pays in
        // *this* node's paper, which redeeming cancels rather than banks.
        let mut app = app_with_two_issuers();
        app.tab = Tab::Wallet;
        let screen = render(&mut app).join("\n");

        assert!(screen.contains("buys from any peer"), "{screen}");
        assert!(screen.contains("spendable only there"), "{screen}");
    }
}
