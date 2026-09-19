#!/bin/bash
# examples-shared/driver/hosts.sh — idempotent lifecycle of the macOS desktop
# test-driver hosts (tauri, electron, node).
#
#   examples-shared/driver/hosts.sh up <tauri|electron|node>
#   examples-shared/driver/hosts.sh down <tauri|electron|node|all>
#   examples-shared/driver/hosts.sh status
#
# Why Terminal: macOS grants Bluetooth to the app that launches a process (its
# responsible app). An automation tool may lack that permission, so `up` asks
# one dedicated Terminal window (titled ubm-driver-hosts, reused every time) to
# run this script's `spawn` step. `spawn` starts the host fully detached and
# records its PID, so repeated `up` calls never pile up windows or processes.
#
# Idempotence: one PID file per host under $STATE. `up` is a no-op while the
# recorded PID is alive and is that host's process; `down` stops exactly it.
set -euo pipefail
trap 'echo "hosts.sh: failed at line $LINENO: $BASH_COMMAND" >&2' ERR

SELF="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
STATE="${UBM_DRIVER_HOSTS_STATE:-${TMPDIR:-/tmp}/ubm-driver-hosts}"
NODE_BIN_DIR="${UBM_NODE_BIN_DIR:-$HOME/.nvm/versions/node/v22.23.1/bin}"
mkdir -p "$STATE"

host_pattern() {
  case "$1" in
    tauri) echo "example-tauri/src-tauri/target/debug/unified-ble-manager-tauri-example" ;;
    electron) echo "example-electron/driver/main.cjs" ;;
    node) echo "example-node/driver.ts serve-host" ;;
    *) echo "unknown host: $1 (tauri|electron|node)" >&2; exit 2 ;;
  esac
}

pid_file() { echo "$STATE/$1.pid"; }
log_file() { echo "$STATE/$1.log"; }

running_pid() {
  local file pid
  file="$(pid_file "$1")"
  [ -f "$file" ] || return 1
  pid="$(cat "$file")"
  if kill -0 "$pid" 2>/dev/null && ps -o command= -p "$pid" | grep -qF "$(host_pattern "$1")"; then
    echo "$pid"
    return 0
  fi
  rm -f "$file"
  return 1
}

stray_pids() {
  # Processes of this host not recorded in the PID file (e.g. started by hand).
  { pgrep -f "$(host_pattern "$1")" || true; } | while read -r pid; do
    [ "$pid" = "$$" ] && continue
    ps -o command= -p "$pid" | grep -q "hosts.sh" && continue
    echo "$pid"
  done
}

spawn() {
  local host="$1" log
  log="$(log_file "$host")"
  export PATH="$NODE_BIN_DIR:$PATH"
  cd "$ROOT"
  case "$host" in
    tauri)
      UBM_TAURI_START_PAGE=driver.html nohup "$ROOT/$(host_pattern tauri)" > "$log" 2>&1 < /dev/null &
      ;;
    electron)
      pnpm exec vite build --config example-electron/driver/vite.config.mts > "$log" 2>&1
      nohup pnpm exec electron example-electron/driver/main.cjs --backend "${UBM_ELECTRON_BACKEND:-corebluetooth}" >> "$log" 2>&1 < /dev/null &
      ;;
    node)
      nohup node example-node/driver.ts serve-host --backend "${UBM_NODE_BACKEND:-corebluetooth}" > "$log" 2>&1 < /dev/null &
      ;;
  esac
  echo $! > "$(pid_file "$host")"
  disown || true
}

# One dedicated Terminal window, reused for every launch: Terminal becomes the
# responsible app (Bluetooth permission), hosts start detached, and repeated
# calls never open more windows.
HOSTS_WINDOW_TITLE="ubm-driver-hosts"
run_in_hosts_window() {
  local command="$1"
  osascript - "$command" "$HOSTS_WINDOW_TITLE" <<'OSA' >/dev/null
on run argv
  set theCommand to item 1 of argv
  set theTitle to item 2 of argv
  tell application "Terminal"
    repeat with w in windows
      try
        if custom title of tab 1 of w is theTitle then
          do script theCommand in tab 1 of w
          return
        end if
      end try
    end repeat
    set newTab to do script theCommand
    set custom title of newTab to theTitle
  end tell
end run
OSA
}

up() {
  local host="$1" pid
  if pid="$(running_pid "$host")"; then
    echo "$host already running (pid $pid)"
    return 0
  fi
  local strays
  strays="$(stray_pids "$host" | tr '\n' ' ')"
  if [ -n "${strays// /}" ]; then
    echo "$host has unmanaged processes ($strays); run '$0 down $host' first" >&2
    return 1
  fi
  # F9: the desktop N-API prebuild is gitignored — refresh it (a no-op when
  # fresh) before electron/node launch. Tauri compiles its plugin inside its
  # own app build, so only electron/node refresh here. A failed refresh
  # aborts the launch; UBM_NATIVE_REFRESH=off switches to check-only.
  if [ "$host" = electron ] || [ "$host" = node ]; then
    node "$ROOT/scripts/native/ensure-native.js" desktop || return 1
  fi
  if [ "${UBM_HOSTS_DIRECT:-}" = 1 ]; then
    spawn "$host"
  else
    run_in_hosts_window "\"$SELF\" spawn $host"
  fi
  for _ in $(seq 1 60); do
    if pid="$(running_pid "$host")"; then
      echo "$host started (pid $pid, log $(log_file "$host"))"
      return 0
    fi
    sleep 0.5
  done
  echo "$host did not start; see $(log_file "$host")" >&2
  return 1
}

down() {
  local host="$1" pid
  if [ "$host" = all ]; then
    for h in tauri electron node; do down "$h"; done
    return 0
  fi
  if pid="$(running_pid "$host")"; then
    kill "$pid" 2>/dev/null || true
    pkill -P "$pid" 2>/dev/null || true
  fi
  for stray in $(stray_pids "$host"); do kill "$stray" 2>/dev/null || true; done
  rm -f "$(pid_file "$host")"
  echo "$host stopped"
}

status() {
  local pid
  for h in tauri electron node; do
    if pid="$(running_pid "$h")"; then
      echo "$h: running (pid $pid)"
    elif [ -n "$(stray_pids "$h")" ]; then
      echo "$h: UNMANAGED processes $(stray_pids "$h" | tr '\n' ' ')"
    else
      echo "$h: stopped"
    fi
  done
}

case "${1:-}" in
  up) up "${2:?host}" ;;
  down) down "${2:?host|all}" ;;
  status) status ;;
  spawn) spawn "${2:?host}" ;;
  *) echo "usage: $0 up|down <tauri|electron|node> | down all | status" >&2; exit 2 ;;
esac
