#!/usr/bin/env bash
# Build a TollGate .ipk package for OpenWrt without the OpenWrt SDK.
#
# Uses cargo-zigbuild to cross-compile and assembles the .ipk directly: an .ipk
# is a gzipped tar of three members, so no SDK is required to make one.
#
# Usage:
#   ./packaging/openwrt-ipk/build-ipk.sh [--arch <name>] [--bin-dir <dir>]
#
# Architectures (--arch):
#   aarch64   GL.iNet MT3000/MT6000, RPi 3/4/5, most modern routers  [default]
#   x86_64    x86 routers and VMs
#   mipsel    Older MIPS routers (TP-Link, Netgear, GL.iNet AR750)
#   mips      MIPS big-endian routers (ath79)
#   arm       32-bit ARM routers (Cortex-A7)
#
# Output: deploy/tollgate_<version>_<openwrt-arch>.ipk
#
# Prerequisites:
#   cargo install cargo-zigbuild
#   rustup target add <rust-triple>   (added automatically if missing)
set -euo pipefail

ARCH="aarch64"
BIN_DIR=""   # prebuilt binaries, for a CI job that cross-compiled once already

while [[ $# -gt 0 ]]; do
    case "$1" in
        --arch) ARCH="$2"; shift 2 ;;
        --arch=*) ARCH="${1#*=}"; shift ;;
        --bin-dir) BIN_DIR="$2"; shift 2 ;;
        --bin-dir=*) BIN_DIR="${1#*=}"; shift ;;
        -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
        *) echo "Unknown argument: $1" >&2; exit 1 ;;
    esac
done

# RUST_TARGET  — passed to cargo --target
# OPENWRT_ARCH — goes in the control file and the filename
case "$ARCH" in
    aarch64) RUST_TARGET="aarch64-unknown-linux-musl"; OPENWRT_ARCH="aarch64_cortex-a53" ;;
    x86_64)  RUST_TARGET="x86_64-unknown-linux-musl";  OPENWRT_ARCH="x86_64" ;;
    mipsel)  RUST_TARGET="mipsel-unknown-linux-musl";  OPENWRT_ARCH="mipsel_24kc" ;;
    mips)    RUST_TARGET="mips-unknown-linux-musl";    OPENWRT_ARCH="mips_24kc" ;;
    arm)     RUST_TARGET="arm-unknown-linux-musleabihf"; OPENWRT_ARCH="arm_cortex-a7" ;;
    *)
        echo "Unknown arch: $ARCH" >&2
        echo "Valid: aarch64, x86_64, mipsel, mips, arm" >&2
        exit 1
        ;;
esac

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
FILES_DIR="$SCRIPT_DIR/files"
DEPLOY_DIR="$PROJECT_ROOT/deploy"

PKG_NAME="tollgate"
PKG_VERSION="${PKG_VERSION:-$(grep '^version' "$PROJECT_ROOT/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')}"

echo "==> Building $PKG_NAME $PKG_VERSION for $OPENWRT_ARCH ($RUST_TARGET)"

# ---------------------------------------------------------------------------
# 1. Binaries
# ---------------------------------------------------------------------------

if [ -n "$BIN_DIR" ]; then
    RELEASE_DIR="$BIN_DIR"
    echo "==> Using prebuilt binaries from $RELEASE_DIR"
else
    if ! command -v cargo-zigbuild &>/dev/null; then
        echo "Error: cargo-zigbuild not found." >&2
        echo "  Install: cargo install cargo-zigbuild" >&2
        exit 1
    fi
    if ! rustup target list --installed | grep -q "^$RUST_TARGET$"; then
        echo "==> Adding Rust target $RUST_TARGET..."
        rustup target add "$RUST_TARGET"
    fi

    echo "==> Compiling..."
    (cd "$PROJECT_ROOT" && cargo zigbuild --release --target "$RUST_TARGET" \
        --bin tollgated --bin tolltop)
    RELEASE_DIR="$PROJECT_ROOT/target/$RUST_TARGET/release"
fi

for bin in tollgated tolltop; do
    [ -f "$RELEASE_DIR/$bin" ] || { echo "Missing binary: $RELEASE_DIR/$bin" >&2; exit 1; }
done

# ---------------------------------------------------------------------------
# 2. Assemble
# ---------------------------------------------------------------------------

WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT
CONTROL_DIR="$WORK_DIR/control"
DATA_DIR="$WORK_DIR/data"
mkdir -p "$CONTROL_DIR" "$DATA_DIR"

install -d "$DATA_DIR/usr/bin"
install -m 0755 "$RELEASE_DIR/tollgated" "$DATA_DIR/usr/bin/tollgated"
install -m 0755 "$RELEASE_DIR/tolltop"   "$DATA_DIR/usr/bin/tolltop"
# Stripped after install so a --bin-dir of unstripped binaries still works and
# the originals are left alone.
"${LLVM_STRIP:-strip}" "$DATA_DIR/usr/bin/tollgated" "$DATA_DIR/usr/bin/tolltop" 2>/dev/null || true

install -d "$DATA_DIR/etc/init.d"
install -m 0755 "$FILES_DIR/etc/init.d/tollgate" "$DATA_DIR/etc/init.d/tollgate"

# 0600: the config holds the node's secret key once first-boot setup has
# written one into it.
install -d "$DATA_DIR/etc/tollgate"
install -m 0600 "$FILES_DIR/etc/tollgate/tollgate.yaml" "$DATA_DIR/etc/tollgate/tollgate.yaml"

install -d "$DATA_DIR/etc/uci-defaults"
install -m 0755 "$FILES_DIR/etc/uci-defaults/90-tollgate-setup" \
    "$DATA_DIR/etc/uci-defaults/90-tollgate-setup"

install -d "$DATA_DIR/lib/upgrade/keep.d"
install -m 0644 "$FILES_DIR/lib/upgrade/keep.d/tollgate" "$DATA_DIR/lib/upgrade/keep.d/tollgate"

PKG_SIZE=$(du -sk "$DATA_DIR" | cut -f1)

# Dependencies are what the nftables adapter actually shells out to and what
# the kernel needs to honour it: nft for the gate and the counters, tc for the
# shaper, sch_htb for the class the shaper installs, and conntrack because the
# forward chain matches on established connections.
cat > "$CONTROL_DIR/control" <<EOF
Package: $PKG_NAME
Version: $PKG_VERSION
Architecture: $OPENWRT_ARCH
Maintainer: TollGate
Section: net
Priority: optional
Depends: nftables, tc-full, kmod-sched-core, kmod-nf-conntrack
Description: TollGate node — sell network transit for ecash
 Meters and sells this router's forwarding capacity, hop by hop, paid for in
 byte-denominated Cashu vouchers over Spilman payment channels. Gates and
 shapes the kernel forwarding path with nftables and tc, runs the mint its
 peers pay against, and buys transit from its own upstream on the same terms.
Installed-Size: $PKG_SIZE
EOF

# opkg leaves a conffile alone on upgrade, which is what keeps the identity
# and whatever the operator has priced.
cat > "$CONTROL_DIR/conffiles" <<EOF
/etc/tollgate/tollgate.yaml
EOF

cat > "$CONTROL_DIR/postinst" <<'EOF'
#!/bin/sh
# First-boot setup: identity, mint URL, firewall. Deletes itself when done.
if [ -x /etc/uci-defaults/90-tollgate-setup ]; then
    /etc/uci-defaults/90-tollgate-setup && rm -f /etc/uci-defaults/90-tollgate-setup
fi

/etc/init.d/tollgate enable
/etc/init.d/tollgate start
exit 0
EOF
chmod 0755 "$CONTROL_DIR/postinst"

cat > "$CONTROL_DIR/prerm" <<'EOF'
#!/bin/sh
/etc/init.d/tollgate stop    2>/dev/null || true
/etc/init.d/tollgate disable 2>/dev/null || true
exit 0
EOF
chmod 0755 "$CONTROL_DIR/prerm"

# ---------------------------------------------------------------------------
# 3. Pack
# ---------------------------------------------------------------------------
# An .ipk is a gzipped tar of three members — debian-binary, control.tar.gz and
# data.tar.gz — not the ar archive a .deb uses.

IPK_WORK="$WORK_DIR/ipk"
mkdir -p "$IPK_WORK"
echo "2.0" > "$IPK_WORK/debian-binary"

# BSD tar's default PAX format is one busybox tar cannot read, so the format is
# always stated. Homebrew's GNU tar is `gtar` on macOS.
if command -v gtar &>/dev/null; then
    TAR_CMD="gtar"; TAR_FLAGS="--format=gnu --numeric-owner"
elif tar --version 2>/dev/null | grep -q 'GNU tar'; then
    TAR_CMD="tar";  TAR_FLAGS="--format=gnu --numeric-owner"
else
    TAR_CMD="tar";  TAR_FLAGS="--format=ustar"
fi

ipk_tar() {
    local out="$1" src="$2"; shift 2
    local mtime=""
    [ -n "${SOURCE_DATE_EPOCH:-}" ] && mtime="--mtime=@$SOURCE_DATE_EPOCH"
    # COPYFILE_DISABLE keeps macOS resource forks out of the archive.
    COPYFILE_DISABLE=1 "$TAR_CMD" $TAR_FLAGS $mtime -czf "$out" -C "$src" "$@"
}

ipk_tar "$IPK_WORK/control.tar.gz" "$CONTROL_DIR" .
ipk_tar "$IPK_WORK/data.tar.gz"    "$DATA_DIR"    .

mkdir -p "$DEPLOY_DIR"
PKG_FILENAME="${PKG_NAME}_${PKG_VERSION}_${OPENWRT_ARCH}.ipk"
ipk_tar "$DEPLOY_DIR/$PKG_FILENAME" "$IPK_WORK" ./debian-binary ./control.tar.gz ./data.tar.gz

echo ""
echo "==> Done: deploy/$PKG_FILENAME ($(du -sh "$DEPLOY_DIR/$PKG_FILENAME" | cut -f1))"
echo ""
echo "Install on a router:"
echo "    scp -O deploy/$PKG_FILENAME root@192.168.1.1:/tmp/"
echo "    ssh root@192.168.1.1 opkg install /tmp/$PKG_FILENAME"
