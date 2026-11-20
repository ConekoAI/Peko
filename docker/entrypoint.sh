#!/bin/sh
# Container entrypoint.
#
#   (no arguments)    run the engine in the foreground
#   anything else     run it as a `peko` command and exit
#                     (`docker run --rm peko version`)
#
# Only the bare form starts an engine. `engine status` and the like are
# commands: a second engine on the same /data would take the first one's
# place.
set -eu

if [ "$#" -gt 0 ]; then
    if [ "$1" = "peko" ]; then
        shift
    fi
    exec peko "$@"
fi

if [ -z "${PEKO_MASTER_PASSPHRASE:-}" ] && [ -z "${PEKO_MASTER_PASSPHRASE_FILE:-}" ]; then
    cat >&2 <<'EOF'
peko: no passphrase for the vault.

A container has no OS keychain, so the keys this runtime holds (model API
keys, its own identity) are encrypted with a passphrase you give it. Set
one of:

  -e PEKO_MASTER_PASSPHRASE=<passphrase>
  -e PEKO_MASTER_PASSPHRASE_FILE=/run/secrets/<name>   (a mounted secret)

Use the same one every time: it unlocks what is stored in /data.
EOF
    exit 78
fi

if [ ! -w "${PEKO_HOME:-/data}" ]; then
    echo "peko: ${PEKO_HOME:-/data} is not writable by uid $(id -u). Mount a volume the container's user (10001) can write." >&2
    exit 73
fi

# The passphrase from a secret file, for the engine itself.
if [ -z "${PEKO_MASTER_PASSPHRASE:-}" ]; then
    if [ ! -r "$PEKO_MASTER_PASSPHRASE_FILE" ]; then
        echo "peko: cannot read PEKO_MASTER_PASSPHRASE_FILE ($PEKO_MASTER_PASSPHRASE_FILE)" >&2
        exit 78
    fi
    PEKO_MASTER_PASSPHRASE="$(cat "$PEKO_MASTER_PASSPHRASE_FILE")"
    export PEKO_MASTER_PASSPHRASE
fi

# The engine itself, not `peko engine start`: it is the process that
# handles SIGTERM, so `docker stop` shuts it down cleanly.
exec /opt/peko/bin/peko-daemon
