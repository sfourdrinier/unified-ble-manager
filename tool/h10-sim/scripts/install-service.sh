#!/usr/bin/env bash
# Installs h10-sim as an always-on systemd --user service on Linux.
#
#   tool/h10-sim/scripts/install-service.sh [--profile profiles/low-battery-legacy.json] [--token abc]
#
# What it does: builds the release binary, installs it to ~/.local/bin,
# installs the unit to ~/.config/systemd/user/h10-sim.service, enables and
# starts it. The profile is copied to ~/.config/h10-sim/profile.json only
# when absent, so re-installs never clobber a tuned profile. A --token (or
# H10SIM_TOKEN in the environment) is written to ~/.config/h10-sim/env with
# mode 600 and loaded via EnvironmentFile; the token never lands in the unit.
#
# Privilege posture (AGENTS.md: permitted, never implicit): this script does
# not escalate. Run it as the user that owns the Bluetooth session; that user
# needs D-Bus access to org.bluez (the `bluetooth` group is enough, no root).
# For start-at-boot without a login session: `loginctl enable-linger $USER`.
set -uo pipefail

SIM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$HOME/.local/bin"
CONF_DIR="$HOME/.config/h10-sim"
UNIT_DIR="$HOME/.config/systemd/user"
PROFILE_ARG="profiles/stock-h10.json"
TOKEN="${H10SIM_TOKEN:-}"

if [ "$(uname -s)" != "Linux" ]; then
  echo "install-service.sh: Linux only (systemd --user); detected $(uname -s)" >&2
  exit 2
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --profile) PROFILE_ARG="${2:?--profile requires a value}"; shift 2 ;;
    --token) TOKEN="${2:?--token requires a value}"; shift 2 ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) echo "install-service.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done

command -v cargo >/dev/null || { echo "install-service.sh: cargo not found" >&2; exit 1; }
command -v systemctl >/dev/null || { echo "install-service.sh: systemctl not found" >&2; exit 1; }

echo "building release binary…"
(cd "$SIM_DIR" && cargo build --locked --release) || exit 1

mkdir -p "$BIN_DIR" "$CONF_DIR" "$UNIT_DIR"
install -m 0755 "$SIM_DIR/target/release/h10-sim" "$BIN_DIR/h10-sim"

if [ ! -f "$CONF_DIR/profile.json" ]; then
  cp "$SIM_DIR/$PROFILE_ARG" "$CONF_DIR/profile.json"
  echo "installed default profile ($PROFILE_ARG) — edit $CONF_DIR/profile.json freely"
else
  echo "keeping existing $CONF_DIR/profile.json"
fi

if [ -n "$TOKEN" ]; then
  printf 'H10SIM_TOKEN=%s\n' "$TOKEN" > "$CONF_DIR/env"
  chmod 600 "$CONF_DIR/env"
  echo "wrote token to $CONF_DIR/env (mode 600)"
fi

cp "$SIM_DIR/systemd/h10-sim.service" "$UNIT_DIR/h10-sim.service"
systemctl --user daemon-reload
systemctl --user enable --now h10-sim.service
systemctl --user status h10-sim.service --no-pager --lines 5
