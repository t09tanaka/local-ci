#!/bin/sh
# Talks to the redis service through the URL the workflow sets.
set -e
port="${CACHE_URL##*:}"
printf 'PING\r\n' | nc -w 2 127.0.0.1 "$port" | grep -q PONG
echo "cache reachable on $port"
