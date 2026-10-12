# .ngit — Nostr CI (ngit-ci)

ngit-ci reads workflows from `.ngit/act/workflows/` **only** — files under
`.github/workflows/` are detected but never executed. GitHub Actions is
untouched and keeps running; the two systems run side by side.

## What runs, and why

`act/workflows/rust-test.yml` mirrors the `check` and `no_std` jobs of
`.github/workflows/ci.yml`, with the same commands in the same order, from the
workspace root:

| Step | Command |
| --- | --- |
| protoc | `apt-get install protobuf-compiler` |
| Format | `cargo fmt --all --check` |
| Clippy | `cargo clippy --workspace --all-targets -- -D warnings` |
| Build | `cargo build --workspace --all-targets` |
| Test | `cargo test --workspace` |
| no_std | `cargo build -p tollgate-protocol -p tollgate-core --target thumbv7em-none-eabihf` |

The toolchain is the one the repo pins: `rust-toolchain.toml` → **1.94.1**
(components `rustfmt`, `clippy`), installed by `dtolnay/rust-toolchain` exactly
as the GitHub workflow does. The separately-installed `thumbv7em-none-eabihf`
target is what guards the esp32-ready `no_std` constraint on `tollgate-core`
and `tollgate-protocol`.

Caching uses `actions/cache@v4` against the coordinator's own cache server
(`~/.cargo/git`, `~/.cargo/registry`, `target`) rather than a third-party Rust
cache action; the step is `continue-on-error: true` so a coordinator with
caching disabled still runs the job.

`protobuf-compiler` is installed before the build, as in the GitHub `check`
job: cdk's signatory reaches gRPC through prost, whose build script shells out
to `protoc`.

## Triggers

Push to **`master`** (this repository's default branch) and pull requests.
`schedule` is not supported by ngit-ci and is not used.

**This deployment runs the `request-required` policy.** Ordinary push and PR
runs do not start until a maintainer publishes a standing **Service Request
(kind 9843)** naming the coordinator and this repository; until then the
coordinator logs `Skipping push trigger until an authorized Service Request is
observed`. Manual triggers (`ngit ci trigger`, or gitworkshop's retry button)
are one-shot and bypass the gate.

```bash
nak event --sec <maintainer-nsec> -k 9843 -c "" \
  -t "a=30617:<maintainer-hex>:tollgate-rs" \
  -t "p=<coordinator-hex>" \
  wss://relay.ngit.dev wss://gitnostr.com
```

## Reading results

- `ngit ci status <commit|pr>` — job and workflow state for a commit or PR.
  (Older `ngit` builds lack the `ci` subcommand; gitworkshop.dev shows the same
  results against the commit or PR, and `nak` reads them directly.)
- Published kinds: **39842** workflow progress, **9841** job result (carries
  the job's log tail), **9842** workflow result/conclusion. Each names the
  commit, the workflow path, and the SHA-256 of the workflow file's content, and
  the 9842 event's `q` tag quotes the Service Request for a gated run.

```bash
# results from the coordinator that ran the job
nak req -k 9842 -a "$COORD_HEX" -l 5  wss://relay.ngit.dev   # conclusions
nak req -k 9841 -a "$COORD_HEX" -l 20 wss://relay.ngit.dev   # per-job + log tail
```

## Not run here (deliberately)

- **Docker integration suites** (`image` + `integration` jobs in
  `.github/workflows/ci.yml`: `testing/{peering,purchase,refusal,rollover,allowance,forwarding}/test.sh`,
  which build `tollgate-test:latest` and run compose topologies). act's job
  container has no Docker daemon, so these cannot run here. They remain GitHub
  only — ngit-ci's docs are explicit that `docker` fails in the job container.
- **macOS and OpenWrt packaging** (`package-macos.yml`, `package-openwrt.yml`).
  macOS labels cannot be served by ngit-ci, and the OpenWrt job needs
  `cargo-zigbuild` + zig for a tag-triggered release build rather than a test.
- **Anything requiring a live network peer or mint** beyond the unit tests —
  the crate tests are self-contained `#[test]`/`#[tokio::test]` cases.
- **Cross-architecture fan-in.** There is one file, on `ubuntu-latest`. A single
  file whose jobs span architectures is claimed only where one coordinator
  supports every label; use one file per architecture if that is ever needed.
