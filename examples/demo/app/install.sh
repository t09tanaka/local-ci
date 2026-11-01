#!/bin/sh
# Stands in for `npm ci`: rebuilds the vendor directory from scratch.
set -e
rm -rf vendor
mkdir vendor
cp lib.sh vendor/lib.sh
