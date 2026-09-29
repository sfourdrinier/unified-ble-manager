#!/usr/bin/env bash
# One canonical private-bus regression gate for hosted CI and clean preflight.
# Each suite gets its own bus; none connects to the system Bluetooth service.
set -euo pipefail

run() {
  UBM_BLUEZ_PRIVATE_BUS_TEST=1 dbus-run-session -- cargo test --locked -p ubm-desktop "$@" -- --ignored --test-threads=1
}

run --test bluez_private_bus
run --lib private_bus_
run --test bluez_bearer_scope
UBM_BLUEZ_PRIVATE_BUS_TEST=1 dbus-run-session -- cargo test --locked -p btleplug --lib le_gatt_tests -- --ignored --test-threads=1
node scripts/ci/test-bluez-daemon-extension.js
