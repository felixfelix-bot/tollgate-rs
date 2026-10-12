#!/usr/bin/env bash
# Build a macOS .pkg installer for TollGate.
#
# Usage: ./packaging/macos/build-pkg.sh [--version <v>] [--target <triple>] [--no-build]
# Output: deploy/tollgate-<version>-macos-<arch>.pkg
#
# Prerequisites: Xcode command-line tools (pkgbuild ships with them).
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PACKAGING_DIR="$(cd "${SCRIPT_DIR}/.." && pwd)"
PROJECT_ROOT="$(cd "${PACKAGING_DIR}/.." && pwd)"

usage() {
    cat <<'EOF'
Usage: packaging/macos/build-pkg.sh [options]

Options:
  --version <version> Override the package version
  --target <triple>   Rust target triple (e.g. x86_64-apple-darwin)
  --no-build          Package existing binaries without running cargo
  -h, --help          Show this help
EOF
}

VERSION_OVERRIDE=""
TARGET_TRIPLE=""
NO_BUILD=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version) VERSION_OVERRIDE="${2:?missing value for --version}"; shift 2 ;;
        --target) TARGET_TRIPLE="${2:?missing value for --target}"; shift 2 ;;
        --no-build) NO_BUILD=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "Unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

VERSION="${VERSION_OVERRIDE:-$(grep '^version' "${PROJECT_ROOT}/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')}"

# From the build target rather than the build host: cross-compiling the x86_64
# package on an Apple-silicon machine would otherwise mislabel it.
if [[ -n "${TARGET_TRIPLE}" ]]; then
    case "${TARGET_TRIPLE}" in
        aarch64-*) ARCH="arm64" ;;
        x86_64-*) ARCH="x86_64" ;;
        *) echo "Unsupported target triple: ${TARGET_TRIPLE}" >&2; exit 1 ;;
    esac
    BINARY_DIR="${PROJECT_ROOT}/target/${TARGET_TRIPLE}/release"
else
    ARCH="$(uname -m)"
    BINARY_DIR="${PROJECT_ROOT}/target/release"
fi

PKG_NAME="tollgate-${VERSION}-macos-${ARCH}"
DEPLOY_DIR="${PROJECT_ROOT}/deploy"
STAGING_DIR="$(mktemp -d)"
SCRIPTS_DIR="$(mktemp -d)"
trap 'rm -rf "${STAGING_DIR}" "${SCRIPTS_DIR}"' EXIT

echo "Building TollGate v${VERSION} for macOS ${ARCH}..."

if [[ "${NO_BUILD}" -eq 0 ]]; then
    cargo_args=(build --release --manifest-path="${PROJECT_ROOT}/Cargo.toml"
                --bin tollgated --bin tolltop)
    [[ -n "${TARGET_TRIPLE}" ]] && cargo_args+=(--target "${TARGET_TRIPLE}")
    cargo "${cargo_args[@]}"
fi

for bin in tollgated tolltop; do
    [[ -f "${BINARY_DIR}/${bin}" ]] || { echo "Missing binary: ${BINARY_DIR}/${bin}" >&2; exit 1; }
done

# Staged as the installed filesystem looks.
mkdir -p "${STAGING_DIR}/usr/local/bin"
mkdir -p "${STAGING_DIR}/usr/local/etc/tollgate"
mkdir -p "${STAGING_DIR}/usr/local/var/log/tollgate"
mkdir -p "${STAGING_DIR}/usr/local/var/run"
# The wallet's directory, but never the wallet: an upgrade that replaced a
# balance with an empty one would be spending somebody's money for them.
mkdir -p "${STAGING_DIR}/usr/local/var/lib/tollgate"
mkdir -p "${STAGING_DIR}/Library/LaunchDaemons"

for bin in tollgated tolltop; do
    cp "${BINARY_DIR}/${bin}" "${STAGING_DIR}/usr/local/bin/"
    strip "${STAGING_DIR}/usr/local/bin/${bin}"
done

# Shipped as `.default` and copied into place by the postinstall only when
# there is nothing there: an upgrade must not overwrite the node's identity.
cp "${SCRIPT_DIR}/tollgate.yaml" "${STAGING_DIR}/usr/local/etc/tollgate/tollgate.yaml.default"
cp "${SCRIPT_DIR}/com.tollgate.daemon.plist" "${STAGING_DIR}/Library/LaunchDaemons/"

cat > "${SCRIPTS_DIR}/postinstall" <<'POSTINSTALL'
#!/bin/sh
set -e

LOG="/var/log/tollgate-install.log"
log() { echo "$(date '+%Y-%m-%d %H:%M:%S') $*" | tee -a "$LOG"; logger -t tollgate-install "$*"; }

log "postinstall started"
CONFDIR="/usr/local/etc/tollgate"

if [ ! -f "$CONFDIR/tollgate.yaml" ]; then
    cp "$CONFDIR/tollgate.yaml.default" "$CONFDIR/tollgate.yaml"

    # A node that generated a fresh key on every start would be a different
    # node on every start: its peers would not recognise it, and the vouchers
    # they hold would be claims on an issuer that no longer exists. So one is
    # generated here, once.
    SECRET="$(/usr/local/bin/tollgated --show-identity 2>/dev/null | awk '/^secret_key:/ { print $2 }')"
    if [ -n "$SECRET" ]; then
        sed -i '' "s|^\( *\)secret_key:.*|\1secret_key: \"$SECRET\"|" "$CONFDIR/tollgate.yaml"
        log "generated a node identity"
    else
        log "could not generate an identity; edit $CONFDIR/tollgate.yaml by hand"
    fi
    chmod 600 "$CONFDIR/tollgate.yaml"
    log "installed default config"
else
    log "kept the existing config"
fi

launchctl bootout system /Library/LaunchDaemons/com.tollgate.daemon.plist 2>/dev/null || true
launchctl bootstrap system /Library/LaunchDaemons/com.tollgate.daemon.plist 2>/dev/null || true
log "launchd service loaded"

log "postinstall complete"
exit 0
POSTINSTALL
chmod +x "${SCRIPTS_DIR}/postinstall"

cat > "${SCRIPTS_DIR}/preinstall" <<'PREINSTALL'
#!/bin/sh
# Stop the daemon before its binary is replaced.
launchctl bootout system /Library/LaunchDaemons/com.tollgate.daemon.plist 2>/dev/null || true
exit 0
PREINSTALL
chmod +x "${SCRIPTS_DIR}/preinstall"

mkdir -p "${DEPLOY_DIR}"
pkgbuild \
    --root "${STAGING_DIR}" \
    --scripts "${SCRIPTS_DIR}" \
    --identifier com.tollgate.pkg \
    --version "${VERSION}" \
    --ownership recommended \
    "${DEPLOY_DIR}/${PKG_NAME}.pkg"

echo ""
echo "Package built: deploy/${PKG_NAME}.pkg"
ls -lh "${DEPLOY_DIR}/${PKG_NAME}.pkg"
echo ""
echo "Install with: sudo installer -pkg deploy/${PKG_NAME}.pkg -target /"
echo "Watch with:   tolltop"
echo "Remove with:  sudo packaging/macos/uninstall.sh"
