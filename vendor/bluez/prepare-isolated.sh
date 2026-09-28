#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-or-later
# Source-only preparation. Never installs or starts a daemon.
set -eu
if [ "$#" -ne 2 ]; then
  echo 'usage: prepare-isolated.sh /absolute/bluez-5.87.tar.xz /absolute/new-directory' >&2
  exit 2
fi
archive=$1
destination=$2
case "$archive:$destination" in /*:/*) ;; *) echo 'absolute paths required' >&2; exit 2 ;; esac
if [ -e "$destination" ]; then
  echo 'destination must not exist; existing source is never overwritten' >&2
  exit 2
fi
expected=26bdcf2cebd7310c6f598850606b037ef0c515fe6608ebc54d22c50c4c32b35f
actual=$(sha256sum "$archive")
if [ "${actual%% *}" != "$expected" ]; then
  echo 'refusing archive: exact official BlueZ 5.87 SHA-256 does not match' >&2
  exit 2
fi
asset_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mkdir -- "$destination"
tar -xf "$archive" -C "$destination"
cd "$destination/bluez-5.87"
patch --batch --fuzz=0 -p1 < "$asset_dir/ubm-le-gatt-5.87.patch"
printf 'Prepared isolated source: %s/bluez-5.87\n' "$destination"
