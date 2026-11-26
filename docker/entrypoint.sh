#!/bin/sh
set -eu

export DDG_STATE_DIR="${DDG_STATE_DIR:-/home/ddg/.local/state/ddg}"
export DDG_RUN_DIR="${DDG_RUN_DIR:-$DDG_STATE_DIR}"
export DDG_PLUGINS_DIR="${DDG_PLUGINS_DIR:-/etc/ddg/plugins}"

mkdir -p "$DDG_STATE_DIR" "$DDG_RUN_DIR"
chmod 700 "$DDG_STATE_DIR" "$DDG_RUN_DIR"

if [ "${DDG_ENABLE_SSHD:-0}" = "1" ]; then
    SSH_DIR="/etc/ddg/ssh"
    AUTH_SRC="$SSH_DIR/authorized_keys"
    if [ ! -s "$AUTH_SRC" ]; then
        echo "ERROR: DDG_ENABLE_SSHD=1 requires a non-empty $AUTH_SRC" >&2
        exit 1
    fi
    mkdir -p "$SSH_DIR" /home/ddg/.ssh
    chmod 700 /home/ddg/.ssh
    cp "$AUTH_SRC" /home/ddg/.ssh/authorized_keys
    chmod 600 /home/ddg/.ssh/authorized_keys

    HOST_KEY="$SSH_DIR/ssh_host_ed25519_key"
    if [ ! -f "$HOST_KEY" ]; then
        ssh-keygen -q -t ed25519 -N "" -f "$HOST_KEY"
        chmod 600 "$HOST_KEY"
    fi
    /usr/sbin/sshd -f /etc/ddg/sshd_config -E "$DDG_STATE_DIR/sshd.log"
fi

# Start ddg-terminal-server core in background and wait for terminal.sock
ddg-terminal-server core >>"$DDG_STATE_DIR/terminal-server.log" 2>&1 &
TERM_PID=$!

i=0
while [ ! -S "$DDG_RUN_DIR/terminal.sock" ]; do
    if ! kill -0 "$TERM_PID" 2>/dev/null; then
        echo "ERROR: ddg-terminal-server core exited unexpectedly" >&2
        cat "$DDG_STATE_DIR/terminal-server.log" >&2 || true
        exit 1
    fi
    i=$((i + 1))
    if [ "$i" -ge 100 ]; then
        echo "ERROR: timed out waiting for terminal.sock" >&2
        exit 1
    fi
    sleep 0.05
done

# Run ddg-agents-host in foreground under tini (PID 1).
exec ddg-agents-host serve
