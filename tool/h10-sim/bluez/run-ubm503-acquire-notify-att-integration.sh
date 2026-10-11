#!/bin/sh
# tool/h10-sim/bluez/run-ubm503-acquire-notify-att-integration.sh
set -eu

host=${UBM503_BLUEZ_HOST:-lx5090wifi}
source_dir=${UBM503_BLUEZ_SOURCE:-/tmp/ubm503-private-bluez-1791686980-96939/clean/bluez-5.87}
root=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/ubm503-att-integration.XXXXXX")
remote_work="/tmp/ubm503-att-integration.$$"
remote="${host}:${remote_work}"
trap 'rm -rf "$work"; ssh "$host" "rm -rf '$remote_work'" >/dev/null 2>&1 || true' EXIT HUP INT TERM

ssh "$host" "test -f '$source_dir/src/gatt-database.c'"
test "$(sha256sum "$root/ubm503-acquire-notify-lifetime.patch" | awk '{print $1}')" = "93f4720aae613435609a9453c959f39884adc6e03ac4697849371942678f9b2f"
ssh "$host" "test -f '$source_dir/../../ubm503-active-bluez-source.tar.xz'"
ssh "$host" "test \"\$(sha256sum '$source_dir/../../ubm503-active-bluez-source.tar.xz' | cut -d' ' -f1)\" = 0bb324f4528fcb25231482d92749ceafbaf240a4698902bd572c3525cc816619"
ssh "$host" "test \"\$(sha256sum '$source_dir/config.h' | cut -d' ' -f1)\" = 90f5a73e0799cb2d7c6a42b6bee9255c9a046b4663adb197511ac70b3b59bd1c"
ssh "$host" "test \"\$(sha256sum '$source_dir/src/gatt-database.c' | cut -d' ' -f1)\" = b3a767636f4759ed45a55fe603a27447702a61eb92595cafc78c456a71e80959"
ssh "$host" "mkdir -p '$remote_work/src'"
scp -q "$root/ubm503-acquire-notify-att-integration.c" "$remote/"
scp -q "$root/ubm503-acquire-notify-lifetime.patch" "$remote/"
ssh "$host" "cp '$source_dir/src/gatt-database.c' '$remote_work/src/gatt-database.c'; cp '$source_dir/src/gatt-database.c' '$remote_work/src/gatt-database.baseline.c'; cp '$remote_work/src/gatt-database.baseline.c' '$remote_work/src/gatt-database.c'; patch --batch --fuzz=0 --reverse --reject-file='$remote_work/rejects' -p1 -d '$remote_work' < '$remote_work/ubm503-acquire-notify-lifetime.patch'; test ! -s '$remote_work/rejects'; mv '$remote_work/src/gatt-database.c' '$remote_work/src/gatt-database.baseline.c'; cp '$source_dir/src/gatt-database.c' '$remote_work/src/gatt-database.c'"

ssh "$host" "python3 - '$remote_work/src/gatt-database.baseline.c' '$remote_work/src/gatt-database.c' '$remote_work/ubm503-acquire-notify-att-integration-extracted.inc' <<'PY'
import re
import sys
from pathlib import Path

baseline = Path(sys.argv[1]).read_text()
patched = Path(sys.argv[2]).read_text()
out = Path(sys.argv[3])

def function(source, name, replacement=None):
    match = re.search(r'(?m)^static\\s+[^\\n]+\\b' + re.escape(name) + r'\\s*\\([^)]*\\)\\n\\{', source)
    if not match:
        raise SystemExit(f'missing exact source function: {name}')
    brace = source.find('{', match.start())
    depth = 0
    quote = None
    escaped = False
    comment = None
    for index in range(brace, len(source)):
        char = source[index]
        next_char = source[index + 1] if index + 1 < len(source) else ''
        if comment == 'line':
            if char == '\\n':
                comment = None
            continue
        if comment == 'block':
            if char == '*' and next_char == '/':
                comment = None
            continue
        if not quote and char == '/' and next_char == '/':
            comment = 'line'
            continue
        if not quote and char == '/' and next_char == '*':
            comment = 'block'
            continue
        if quote:
            if escaped:
                escaped = False
            elif char == '\\\\':
                escaped = True
            elif char == quote:
                quote = None
            continue
        if char == chr(34) or char == chr(39) or char == chr(92):
            quote = char
        elif char == '{':
            depth += 1
        elif char == '}':
            depth -= 1
            if depth == 0:
                text = source[match.start():index + 1]
                if replacement:
                    text = text.replace(name, replacement, 1)
                return text
    raise SystemExit(f'unterminated exact source function: {name}')

def struct_definition(source, name):
    match = re.search(r'(?m)^struct\\s+' + re.escape(name) + r'\\s*\\{', source)
    if not match:
        raise SystemExit(f'missing exact source struct: {name}')
    brace = source.find('{', match.start())
    depth = 0
    for index in range(brace, len(source)):
        if source[index] == '{':
            depth += 1
        elif source[index] == '}':
            depth -= 1
            if depth == 0:
                return source[match.start():index + 1] + ';'
    raise SystemExit(f'unterminated exact source struct: {name}')

names = ['client_io_ref_if_live', 'client_io_release', 'client_io_free',
         'indication_guard_release', 'indication_guard_destroy',
         'indication_guard_new', 'indication_guard_finish',
         'sock_io_write', 'sock_io_conf', 'send_notification_to_device',
         'sock_io_read']
chunks = [struct_definition(patched, 'client_io'),
          struct_definition(patched, 'indication_guard'),
          struct_definition(patched, 'btd_gatt_database'),
          struct_definition(patched, 'gatt_app'),
          struct_definition(patched, 'external_service'),
          struct_definition(patched, 'external_chrc'),
          struct_definition(patched, 'notify'),
          struct_definition(patched, 'device_state'),
          struct_definition(patched, 'ccc_state')]
chunks += [function(patched, name) for name in names if name != 'sock_io_conf']
chunks.append(function(patched, 'sock_io_conf', 'patched_sock_io_conf_impl'))
chunks.append(function(baseline, 'sock_io_conf', 'baseline_sock_io_conf'))
out.write_text('\\n'.join(chunks) + '\\n')
PY"

ssh "$host" "cd '$source_dir'; make -B -j2 -o configure -o Makefile.in -o config.status src/shared/libshared_glib_la-att.lo src/shared/libshared_glib_la-gatt-server.lo src/shared/libshared_glib_la-gatt-db.lo src/shared/libshared_glib_la-io-glib.lo src/shared/libshared_glib_la-queue.lo src/shared/libshared_glib_la-util.lo src/shared/libshared_glib_la-crypto.lo src/shared/libshared_glib_la-timeout-glib.lo && cc -DHAVE_CONFIG_H -I. -Ilib -c lib/bluetooth/uuid.c -o lib/bluetooth/.libs/uuid.o && sha256sum src/shared/.libs/libshared_glib_la-att.o src/shared/.libs/libshared_glib_la-gatt-server.o src/shared/.libs/libshared_glib_la-gatt-db.o src/shared/.libs/libshared_glib_la-io-glib.o src/shared/.libs/libshared_glib_la-queue.o src/shared/.libs/libshared_glib_la-util.o src/shared/.libs/libshared_glib_la-crypto.o src/shared/.libs/libshared_glib_la-timeout-glib.o lib/bluetooth/.libs/uuid.o"

ssh "$host" "cd '$source_dir'; cc=\${CC:-cc}; \"\$cc\" -std=c11 -Wall -Wextra -Wno-unused-parameter -Werror -O1 -g -fsanitize=address -fno-omit-frame-pointer -I. -Ilib -Isrc -I\"\$(pwd)\" \"$remote_work/ubm503-acquire-notify-att-integration.c\" -o \"$remote_work/ubm503-att-integration\" \$(pkg-config --cflags --libs glib-2.0 gio-2.0) src/shared/.libs/libshared_glib_la-att.o src/shared/.libs/libshared_glib_la-gatt-server.o src/shared/.libs/libshared_glib_la-gatt-db.o src/shared/.libs/libshared_glib_la-io-glib.o src/shared/.libs/libshared_glib_la-queue.o src/shared/.libs/libshared_glib_la-util.o src/shared/.libs/libshared_glib_la-crypto.o src/shared/.libs/libshared_glib_la-timeout-glib.o lib/bluetooth/.libs/uuid.o -lcrypto -ldl"

set +e
baseline_output=$(ssh "$host" "ASAN_OPTIONS=detect_leaks=1 '$remote_work/ubm503-att-integration' --baseline" 2>&1)
baseline_status=$?
set -e
printf '%s\n' "$baseline_output"
test "$baseline_status" -eq 1
printf '%s\n' "$baseline_output" | grep -F 'baseline-red: actual ATT confirmation injected after IO destroy'
printf '%s\n' "$baseline_output" | grep -F "GLib-CRITICAL" | grep -F "g_io_add_watch_full: assertion 'channel != NULL' failed"
printf '%s\n' "$baseline_output" | grep -F 'FAIL: baseline-red: actual ATT late confirmation used destroyed IO'

ssh "$host" "ASAN_OPTIONS=detect_leaks=1 '$remote_work/ubm503-att-integration'"
