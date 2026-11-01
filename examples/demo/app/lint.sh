#!/bin/sh
set -e
. ./vendor/lib.sh
for f in *.sh; do sh -n "$f"; done
echo "lint ok"
