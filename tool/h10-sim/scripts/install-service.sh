#!/usr/bin/env bash
# Installs h10-sim as an always-on systemd --user service on Linux.
#
#   tool/h10-sim/scripts/install-service.sh [--profile profiles/low-battery-legacy.json] [--token abc]
#       [--linux-advertising bluez|mgmt-legacy] [--system-unit]
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
#
# --linux-advertising mgmt-legacy (README, Linux) needs CAP_NET_ADMIN, which
# only the owner grants, with sudo, by hand:
#   * user unit (default): H10SIM_LINUX_ADVERTISING=mgmt-legacy goes to the
#     env file; the script prints `sudo setcap cap_net_admin+ep` for the
#     installed binary and does not start the unit until getcap shows it
#     (every install replaces the binary, which drops the capability);
#   * --system-unit: renders systemd/h10-sim-mgmt-legacy.service.in (ambient
#     CAP_NET_ADMIN for that service only, running as this user) into
#     ~/.config/h10-sim/ and prints the sudo commands that install it.
set -uo pipefail

SIM_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN_DIR="$HOME/.local/bin"
CONF_DIR="$HOME/.config/h10-sim"
UNIT_DIR="$HOME/.config/systemd/user"
PROFILE_ARG="profiles/stock-h10.json"
TOKEN="${H10SIM_TOKEN:-}"
ADVERTISING="bluez"
SYSTEM_UNIT=0

if [ "$(uname -s)" != "Linux" ]; then
  echo "install-service.sh: Linux only (systemd --user); detected $(uname -s)" >&2
  exit 2
fi

while [ $# -gt 0 ]; do
  case "$1" in
    --profile) PROFILE_ARG="${2:?--profile requires a value}"; shift 2 ;;
    --token) TOKEN="${2:?--token requires a value}"; shift 2 ;;
    --linux-advertising) ADVERTISING="${2:?--linux-advertising requires a value}"; shift 2 ;;
    --system-unit) SYSTEM_UNIT=1; shift ;;
    -h|--help) sed -n '2,29p' "$0"; exit 0 ;;
    *) echo "install-service.sh: unknown argument $1" >&2; exit 2 ;;
  esac
done

case "$ADVERTISING" in
  bluez|mgmt-legacy) ;;
  *) echo "install-service.sh: --linux-advertising must be bluez or mgmt-legacy" >&2; exit 2 ;;
esac
if [ "$SYSTEM_UNIT" = 1 ] && [ "$ADVERTISING" != "mgmt-legacy" ]; then
  echo "install-service.sh: --system-unit is only for --linux-advertising mgmt-legacy" >&2
  exit 2
fi

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

# The env file keeps an existing token when no new one is given.
EXISTING_TOKEN=""
if [ -z "$TOKEN" ] && [ -f "$CONF_DIR/env" ]; then
  EXISTING_TOKEN="$(sed -n 's/^H10SIM_TOKEN=//p' "$CONF_DIR/env")"
fi
TOKEN="${TOKEN:-$EXISTING_TOKEN}"
(
  umask 077
  {
    if [ -n "$TOKEN" ]; then printf 'H10SIM_TOKEN=%s\n' "$TOKEN"; fi
    printf 'H10SIM_LINUX_ADVERTISING=%s\n' "$ADVERTISING"
  } > "$CONF_DIR/env"
)
chmod 600 "$CONF_DIR/env"
echo "wrote $CONF_DIR/env (mode 600): advertising $ADVERTISING${TOKEN:+, token set}"

if [ "$SYSTEM_UNIT" = 1 ]; then
  RENDERED="$CONF_DIR/h10-sim-mgmt-legacy.service"
  sed -e "s|@USER@|$(id -un)|g" -e "s|@HOME@|$HOME|g" \
    "$SIM_DIR/systemd/h10-sim-mgmt-legacy.service.in" > "$RENDERED"
  systemd-analyze verify "$RENDERED" || exit 1
  echo
  echo "rendered $RENDERED. Install it yourself (this script never escalates):"
  echo "  systemctl --user disable --now h10-sim.service   # if the user unit is enabled"
  echo "  sudo install -m 0644 $RENDERED /etc/systemd/system/h10-sim-mgmt-legacy.service"
  echo "  sudo systemctl daemon-reload"
  echo "  sudo systemctl enable --now h10-sim-mgmt-legacy.service"
  echo "Remove: sudo systemctl disable --now h10-sim-mgmt-legacy.service &&"
  echo "        sudo rm /etc/systemd/system/h10-sim-mgmt-legacy.service && sudo systemctl daemon-reload"
  exit 0
fi

cp "$SIM_DIR/systemd/h10-sim.service" "$UNIT_DIR/h10-sim.service"
systemctl --user daemon-reload

if [ "$ADVERTISING" = "mgmt-legacy" ] && ! getcap "$BIN_DIR/h10-sim" | grep -q cap_net_admin; then
  echo
  echo "mgmt-legacy needs CAP_NET_ADMIN on $BIN_DIR/h10-sim, which this install just replaced."
  echo "Grant it yourself (this script never escalates), then start the unit:"
  echo "  sudo setcap cap_net_admin+ep $BIN_DIR/h10-sim"
  echo "  systemctl --user enable --now h10-sim.service"
  echo "Remove it again with: sudo setcap -r $BIN_DIR/h10-sim"
  exit 3
fi

systemctl --user enable --now h10-sim.service
systemctl --user status h10-sim.service --no-pager --lines 5
