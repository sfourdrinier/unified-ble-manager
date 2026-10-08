#!/usr/bin/env python3
# SPDX-License-Identifier: GPL-2.0-or-later
"""Exact patched-BlueZ Pair/lease controls with explicit native-boundary doubles.

Never installs or starts bluetoothd. A supplied isolated source tree must already
have the maintained patch applied. Extracts every production body mechanically;
the retained C template contains only controlled boundaries and assertions.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import subprocess


def production_function(source, symbol):
    pattern = re.compile(r"^(?:static\s+)?(?:[A-Za-z_]\w*\s+)*(?:\*+\s*)?" + re.escape(symbol) + r"\s*\(", re.M)
    candidates = []
    for match in pattern.finditer(source):
        opening = source.find("{", match.end())
        semicolon = source.find(";", match.end())
        if opening >= 0 and (semicolon < 0 or opening < semicolon):
            candidates.append((match.start(), opening))
    if len(candidates) != 1:
        raise ValueError(f"expected one production definition for {symbol}; found {len(candidates)}")
    start, opening = candidates[0]
    depth, index, state = 0, opening, "code"
    while index < len(source):
        char, pair = source[index], source[index:index + 2]
        if state == "line":
            if char == "\n":
                state = "code"
        elif state == "comment":
            if pair == "*/":
                state = "code"
                index += 1
        elif state in ('"', "'"):
            if char == "\\":
                index += 1
            elif char == state:
                state = "code"
        elif pair == "//":
            state = "line"
            index += 1
        elif pair == "/*":
            state = "comment"
            index += 1
        elif char in ('"', "'"):
            state = char
        elif char == "{":
            depth += 1
        elif char == "}":
            depth -= 1
            if depth == 0:
                body = source[start:index + 1]
                return body, source.count("\n", 0, start) + 1
        index += 1
    raise ValueError(f"unterminated production function {symbol}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", required=True, type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--generate-only", action="store_true")
    args = parser.parse_args()
    if not args.source.is_absolute() or not args.output.is_absolute():
        parser.error("source and output must be absolute paths")
    root = Path(__file__).resolve().parents[2]
    source = args.source.resolve()
    output = args.output.resolve()
    if output == root or root in output.parents or output == source or source in output.parents:
        parser.error("output must be outside repository and supplied source tree")
    output.mkdir(parents=True, exist_ok=True)
    lease_path, device_path = source / "src/ubm-le-lease.c", source / "src/device.c"
    lease, device = lease_path.read_text(), device_path.read_text()
    patch = (root / "vendor/bluez/ubm-le-gatt-5.87.patch").read_bytes()
    # The lease is wholly added by the maintained patch; byte-bind it before
    # extracting functions. Isolated device.c preparation is owned by the
    # existing daemon source gate, which verifies the upstream archive.
    section = patch.decode().split("+++ b/src/ubm-le-lease.c\n", 1)[1].split("--- a/", 1)[0]
    maintained_lease = "\n".join(line[1:] for line in section.splitlines() if line.startswith("+")) + "\n"
    if lease != maintained_lease:
        raise ValueError("isolated lease source differs from current maintained patch")
    template_path = Path(__file__).with_suffix(".c.in")
    template = template_path.read_text()
    names = re.findall(r"\{\{FUNCTION:(\w+)\}\}", template)
    if len(set(names)) != len(names):
        raise ValueError("duplicate production function placeholders")
    functions, provenance, prototypes = {}, [], []
    for name in names:
        relative = "src/device.c" if name in ("pair_device", "device_bonding_complete", "create_bond_req_exit") else "src/ubm-le-lease.c"
        body, line = production_function(device if relative.endswith("device.c") else lease, name)
        functions[name] = body
        prototypes.append(body[:body.index("{")].rstrip() + ";")
        provenance.append({"symbol": name, "sourceFile": relative, "firstLine": line,
                           "sha256": hashlib.sha256(body.encode()).hexdigest()})
    structures = lease[lease.index("enum link_origin {"):lease.index("static uint32_t token_namespace_serial;")]
    generated = template.replace("{{LEASE_STRUCTURES}}", structures).replace("{{PROTOTYPES}}", "\n".join(prototypes))
    for name, body in functions.items():
        generated = generated.replace("{{FUNCTION:" + name + "}}", "/* Exact production body; see provenance.json */\n" + body)
    if "{{" in generated:
        raise ValueError("unresolved template placeholder")
    c_file = output / "bluez-pair-bearer-controls.c"
    c_file.write_text(generated)
    receipt = {"scope": "exact production functions; controlled D-Bus/GLib/device/native boundaries; no physical-radio proof",
               "patchSha256": hashlib.sha256(patch).hexdigest(),
               "templateSha256": hashlib.sha256(template_path.read_bytes()).hexdigest(),
               "sourceFiles": {str(path.relative_to(source)): hashlib.sha256(path.read_bytes()).hexdigest()
                               for path in (lease_path, device_path)}, "functions": provenance,
               "generatedSha256": hashlib.sha256(c_file.read_bytes()).hexdigest(),
               "executed": False}
    receipt_file = output / "provenance.json"
    receipt_file.write_text(json.dumps(receipt, indent=2) + "\n")
    if args.generate_only:
        print(f"Generated {len(functions)} exact production functions; not compiled or executed")
        return
    executable = output / "bluez-pair-bearer-controls"
    subprocess.run(shlex.split(os.environ.get("CC", "cc")) + ["-std=gnu11", "-Werror=implicit-function-declaration",
                   str(c_file), "-o", str(executable)], check=True)
    subprocess.run([str(executable)], check=True)
    receipt["executed"] = True
    receipt_file.write_text(json.dumps(receipt, indent=2) + "\n")
    print(f"PASS {len(functions)} exact production-function Pair/bearer controls")


if __name__ == "__main__":
    main()
