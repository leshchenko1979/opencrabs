#!/bin/bash
# Self-healing SSH tunnel wrapper - auto-restarts when connection drops

HOST="root@vpn"
REMOTE_PORT=4444
LOCAL_PORT=22
SSH_KEY="$HOME/.ssh/id_ed25519"
SSH_OPTIONS="-o ServerAliveInterval=60 -o ServerAliveCountMax=3 -o StrictHostKeyChecking=no -o ExitOnForwardFailure=yes"

while true; do
    ssh -R "*:${REMOTE_PORT}:localhost:${LOCAL_PORT}" -N -i "$SSH_KEY" $SSH_OPTIONS $HOST
    echo "$(date): SSH tunnel died with exit $?, restarting in 5 seconds..." >&2
    sleep 5
done