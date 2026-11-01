#!/bin/sh
set -e
. ./vendor/lib.sh
case "$1" in
  1) [ "$(add 1 2)" = 3 ] ;;
  2) [ "$(add 2 2)" = 4 ] ;;
esac
echo "shard $1 ok" | tee report.txt
