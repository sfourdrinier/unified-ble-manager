#!/bin/sh
# tool/h10-sim/bluez/run-ubm503-acquire-notify-lifetime-regression.sh
set -eu
baseline=${1:-/tmp/ubm503-active-bluez-gatt-database.c}
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/ubm503-lifetime.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
test -f "$baseline"
test "$(shasum -a 256 "$baseline" | awk '{print $1}')" = "91b0e032a6ba46fc34c47d6d30f60c4ca39c5db8c752e8603c194bf73409bc09"
mkdir -p "$work/src"
cp "$baseline" "$work/src/gatt-database.c"
patch --batch --fuzz=0 --reject-file="$work/rejects" -p1 -d "$work" < "$root/ubm503-acquire-notify-lifetime.patch"
test ! -s "$work/rejects"
patched="$work/src/gatt-database.c"
python3 - "$baseline" "$patched" "$work/ubm503-acquire-notify-lifetime-extracted.inc" <<'PY'
import re
import sys
from pathlib import Path
baseline = Path(sys.argv[1]).read_text()
patched = Path(sys.argv[2]).read_text()
out = Path(sys.argv[3])
def function(source, name, replacement=None):
    match = re.search(r"(?m)^static\s+[^\n]+\b" + re.escape(name) + r"\s*\([^;]*\)\n\{", source)
    if not match:
        raise SystemExit(f"missing exact source function: {name}")
    start = match.start()
    brace = source.find("{", match.start())
    depth = 0
    quoted = None
    escaped = False
    for index in range(brace, len(source)):
        char = source[index]
        if quoted:
            if escaped:
                escaped = False
            elif char == "\\\\":
                escaped = True
            elif char == quoted:
                quoted = None
            continue
        if char in "\\\"'":
            quoted = char
        elif char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                text = source[start:index + 1]
                if replacement:
                    text = text.replace(name, replacement, 1)
                return text
    raise SystemExit(f"unterminated exact source function: {name}")
def struct_definition(source, name):
    match = re.search(r"(?m)^struct\s+" + re.escape(name) + r"\s*\{", source)
    if not match:
        raise SystemExit(f"missing exact source struct: {name}")
    start = match.start()
    brace = source.find("{", match.start())
    depth = 0
    for index in range(brace, len(source)):
        if source[index] == "{": depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[start:index + 1] + ";"
    raise SystemExit(f"unterminated exact source struct: {name}")
names = ["client_io_ref_lock", "client_io_ref_unlock", "client_io_ref_if_live", "client_io_release", "client_io_free", "indication_guard_lock", "indication_guard_unlock", "indication_guard_release", "indication_guard_destroy", "indication_guard_new", "indication_guard_finish", "sock_io_write", "sock_io_conf"]
chunks = [struct_definition(patched, "client_io"), struct_definition(patched, "indication_guard")]
chunks += [function(patched, name) for name in names if name in patched]
chunks.append(function(baseline, "sock_io_conf", "baseline_sock_io_conf"))
out.write_text(chr(10).join(chunks) + chr(10))
PY
cc=${CC:-cc}
"$cc" -std=c11 -Wall -Wextra -Werror -Wno-unused-parameter -O0 -g -I"$work" -I"$root" "$root/ubm503-acquire-notify-lifetime-regression.c" -o "$work/regression"
set +e
"$work/regression" --baseline >"$work/baseline.log" 2>&1
baseline_status=$?
set -e
grep -F 'baseline-red: late confirmation touched destroyed IO' "$work/baseline.log"
test "$baseline_status" -eq 1
cat "$work/baseline.log"
"$work/regression"
