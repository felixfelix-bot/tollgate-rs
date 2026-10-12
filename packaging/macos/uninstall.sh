#!/bin/sh
# Remove a TollGate installation from macOS.
#
# Leaves the config and the wallet alone unless --purge is given: one holds the
# node's identity, which every voucher a peer holds is a claim on, and the other
# holds bearer tokens that exist nowhere else.
set -e

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

if [ "$(id -u)" -ne 0 ]; then
    echo "Run with sudo." >&2
    exit 1
fi

launchctl bootout system /Library/LaunchDaemons/com.tollgate.daemon.plist 2>/dev/null || true
rm -f /Library/LaunchDaemons/com.tollgate.daemon.plist
rm -f /usr/local/bin/tollgated /usr/local/bin/tolltop
rm -rf /usr/local/var/log/tollgate
rm -f /usr/local/var/run/tollgate.sock
pkgutil --forget com.tollgate.pkg 2>/dev/null || true

if [ "$PURGE" -eq 1 ]; then
    rm -rf /usr/local/etc/tollgate /usr/local/var/lib/tollgate
    echo "Removed, including the node's identity and whatever it was holding."
else
    echo "Removed. Kept:"
    echo "  /usr/local/etc/tollgate      this node's identity"
    echo "  /usr/local/var/lib/tollgate  its wallet, which is money"
    echo "Pass --purge to delete those too."
fi
