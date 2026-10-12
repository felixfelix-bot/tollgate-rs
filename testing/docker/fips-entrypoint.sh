#!/bin/sh
# Bring the mesh up, then run whatever the container was asked to run.
#
# The FIPS daemon has to be first and has to be ready: tollgated refuses to
# start if the control socket does not answer, and anything speaking over the
# mesh needs fips0 to exist. So this waits for both rather than racing them,
# and every service in the topology gets the same treatment — a container that
# only serves a file still needs the interface its address lives on.
set -eu

fips --config /etc/fips/fips.yaml &
FIPS_PID=$!

# Both conditions matter and they arrive in this order: the interface is
# created while the daemon is still setting up, and the socket is the last
# thing to appear.
i=0
while [ "$i" -lt 60 ]; do
    if [ -e /sys/class/net/fips0 ] && [ -S /run/fips/control.sock ]; then
        break
    fi
    # A daemon that has already exited will never satisfy either condition.
    if ! kill -0 "$FIPS_PID" 2>/dev/null; then
        echo "fipsd exited before the mesh came up" >&2
        exit 1
    fi
    i=$((i + 1))
    sleep 0.5
done

if [ ! -S /run/fips/control.sock ]; then
    echo "fipsd did not open its control socket within 30s" >&2
    exit 1
fi

echo "mesh up: $(ip -6 -br addr show fips0)"

exec "$@"
