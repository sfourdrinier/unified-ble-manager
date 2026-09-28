#!/bin/sh
# SPDX-License-Identifier: GPL-2.0-or-later
# Only build outputs and a private test D-Bus session. No install/radio access.
set -eu
if [ "$#" -ne 1 ]; then
  echo 'usage: build-test-isolated.sh /absolute/prepared/bluez-5.87' >&2
  exit 2
fi
case "$1" in /*) ;; *) echo 'absolute source path required' >&2; exit 2 ;; esac
cd -- "$1"
test -f src/ubm-gatt-state.h
test -f unit/test-ubm-device.c
for required in cc make pkg-config dbus-run-session; do
  command -v "$required" >/dev/null
done
mkdir -p -- ubm-build-tmp
TMPDIR="$PWD/ubm-build-tmp"
export TMPDIR
./configure --prefix="$PWD/ubm-unused-install" --sysconfdir="$PWD/ubm-unused-etc" \
  --localstatedir="$PWD/ubm-unused-var" --with-udevdir="$PWD/ubm-unused-udev" \
  --with-systemdsystemunitdir="$PWD/ubm-unused-systemd" \
  --with-systemduserunitdir="$PWD/ubm-unused-systemd-user" \
  --disable-dependency-tracking --disable-client --disable-tools \
  --disable-monitor --disable-cups --disable-obex --disable-manpages
make -j1 src/builtin.h src/bluetoothd unit/test-gatt
./unit/test-gatt
cc -std=c11 -Wall -Wextra -Werror -I. unit/test-ubm-gatt-state.c \
  -o unit/test-ubm-gatt-state
./unit/test-ubm-gatt-state
for test_name in gatt-projection refresh device bonded-notify; do
  # pkg-config provides ordinary compiler/linker argument lists, not file names.
  # shellcheck disable=SC2046
  cc -std=gnu11 -DHAVE_CONFIG_H -DUBM_NOTIFY_TRACKED -Werror=implicit-function-declaration \
    -ffunction-sections -fdata-sections -I. -Ilib \
    $(pkg-config --cflags glib-2.0 dbus-1) "unit/test-ubm-$test_name.c" \
    -Wl,--gc-sections gdbus/.libs/libgdbus-internal.a \
    src/.libs/libshared-glib.a lib/.libs/libbluetooth-internal.a \
    $(pkg-config --libs glib-2.0 dbus-1) -o "unit/test-ubm-$test_name"
done
./unit/test-ubm-gatt-projection
./unit/test-ubm-refresh
dbus-run-session -- ./unit/test-ubm-device
./unit/test-ubm-bonded-notify
for row in pending-count pending-disable removed transition unbonded-peer dormant-peer \
  registration-refused pending-registration-refused client-registration-refused \
  pending-queue-refused client-queue-refused io-refused; do
  ./unit/test-ubm-bonded-notify "$row"
done
sha256sum src/bluetoothd src/device.c src/gatt-client.c src/shared/gatt-client.c
