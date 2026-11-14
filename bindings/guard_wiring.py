#!/usr/bin/env python3
"""Wiring-slice production guards (review follow-up L1 + L2).

L1: no unwrap/expect/panic on fallible production paths. Scans
    bindings/*/src/*.rs + crates/ubm-core/src/*.rs with #[cfg(test)]-gated
    items stripped (test modules may use expect/unwrap freely).
L2: no stale EchoCore/echo_core names in safety docs or Java sources (the
    deleted echo-only stand-in); the historical `echo_core.rs` filename is
    allowed. Rust sources are covered for EchoCore too.
Fails loudly (exit 1) with file:line details. contracts/** untouched.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent  # repo root: bindings/<script>

RS_FORBIDDEN = [
    (re.compile(r"\.unwrap\("), ".unwrap("),
    (re.compile(r"\.expect\("), ".expect("),
    (re.compile(r"\.expect_err\("), ".expect_err("),
    (re.compile(r"(?<![A-Za-z0-9_])panic!"), "panic!"),
    (re.compile(r"\btodo!"), "todo!"),
    (re.compile(r"\bunimplemented!"), "unimplemented!"),
    (re.compile(r"EchoCore"), "EchoCore"),
    (re.compile(r"echo_core(?!\.rs)"), "echo_core"),
]
DOC_FORBIDDEN = [
    (re.compile(r"EchoCore"), "EchoCore"),
    (re.compile(r"echo_core(?!\.rs)"), "echo_core"),
]


def fail(msg):
    print(f"guard: FAIL {msg}", file=sys.stderr)
    sys.exit(1)


# Only these standalone sources are test-only. Keeping the parent declaration
# alongside each child prevents a filename convention from silently exempting
# a future production `*_tests.rs` module from the production guard.
TEST_ONLY_MODULES = {
    "bindings/napi/src/platform_metadata_transport_522_tests.rs": (
        "bindings/napi/src/dispatch.rs",
        '#[cfg(test)]\n#[path = "platform_metadata_transport_522_tests.rs"]\n'
        "mod platform_metadata_transport_522_tests;",
    ),
    "bindings/uniffi/src/ordered_admission_tests.rs": (
        "bindings/uniffi/src/mobile.rs",
        '#[cfg(test)]\n#[path = "ordered_admission_tests.rs"]\n'
        "mod ordered_admission_tests;",
    ),
    "crates/ubm-core/src/ordered_notifications_tests.rs": (
        "crates/ubm-core/src/ordered_notifications.rs",
        '#[cfg(test)]\n#[path = "ordered_notifications_tests.rs"]\nmod tests;',
    ),
}


def test_only_rs_files(root, test_only_modules=TEST_ONLY_MODULES):
    """Return explicitly proven test-only standalone Rust sources."""
    paths = set()
    for child_rel, (parent_rel, declaration) in test_only_modules.items():
        child = root / child_rel
        parent = root / parent_rel
        if not child.is_file():
            fail(f"test-only source is missing: {child_rel}")
        if not parent.is_file() or declaration not in parent.read_text():
            fail(
                f"test-only source lacks its exact #[cfg(test)] parent declaration: "
                f"{child_rel} via {parent_rel}"
            )
        paths.add(child)
    return paths


def production_rs_files(root, test_only_modules=TEST_ONLY_MODULES):
    """Return every candidate except standalone modules proven test-only."""
    test_only = test_only_rs_files(root, test_only_modules)
    candidates = sorted(root.glob("bindings/*/src/*.rs")) + sorted(
        root.glob("crates/ubm-core/src/*.rs")
    )
    return [path for path in candidates if path not in test_only]


RS_FILES = production_rs_files(ROOT)
DOC_FILES = sorted(ROOT.glob("bindings/*/LIFETIME_RULES.md")) + sorted(
    ROOT.glob("bindings/jni/java/**/*.java")
)


def scan_item(lines, j):
    """Return the index one past the #[cfg(test)]-gated item starting at j.

    The item ends at the first `;` at brace depth 0 (use/static/plain
    items) or at the close of its balanced `{...}` block (fn/mod/impl/
    struct items). String, char, line-comment and (nested) block-comment
    contents never affect brace depth. Fails closed on unknown shapes.

    Test-gated struct and enum members are handled by ``strip_test_gated``:
    their terminating comma is not an item terminator and must not make this
    item scanner consume the enclosing type.
    """
    n = len(lines)
    depth = 0
    seen_brace = False
    state = "code"  # code | string | char | line | block
    block_depth = 0
    j0 = j
    while j < n:
        line = lines[j]
        k = 0
        while k < len(line):
            c = line[k]
            nxt = line[k + 1] if k + 1 < len(line) else ""
            if state == "code":
                if c == "/" and nxt == "/":
                    state = "line"
                    break
                if c == "/" and nxt == "*":
                    state = "block"
                    block_depth = 1
                    k += 2
                    continue
                if c == '"':
                    state = "string"
                elif c == "'":
                    # Char literal only for 'x' / escaped forms; otherwise
                    # a lifetime tick, which carries no code structure.
                    if nxt == "\\":
                        state = "char"
                    elif k + 2 < len(line) and line[k + 2] == "'":
                        k += 3
                        continue
                elif c == "{":
                    depth += 1
                    seen_brace = True
                elif c == "}":
                    depth -= 1
                    if depth < 0:
                        fail(f"unbalanced brace while stripping near line {j0 + 1}")
                elif c == ";" and depth == 0 and not seen_brace:
                    return j + 1
            elif state == "string":
                if c == "\\":
                    k += 2
                    continue
                if c == '"':
                    state = "code"
            elif state == "char":
                if c == "\\":
                    k += 2
                    continue
                if c == "'":
                    state = "code"
            elif state == "block":
                if c == "/" and nxt == "*":
                    block_depth += 1
                    k += 2
                    continue
                if c == "*" and nxt == "/":
                    block_depth -= 1
                    if block_depth == 0:
                        state = "code"
                    k += 2
                    continue
            k += 1
        if state == "line":
            state = "code"
        if seen_brace and depth == 0 and state in ("code", "line"):
            return j + 1
        j += 1
    fail(f"unterminated #[cfg(test)] item starting at line {j0 + 1}")


def strip_test_gated(path, text):
    """Return [(lineno, line)] of production lines (test-gated items out)."""
    lines = text.splitlines()
    out = []
    i = 0
    while i < len(lines):
        if lines[i].strip() == "#[cfg(test)]":
            item = i + 1
            # Rust permits attributes such as #[path = "..."] between the
            # test gate and its item. Recognize only complete one-line
            # attributes here; a multi-line or malformed attribute remains a
            # hard error rather than letting this lexical guard guess.
            while item < len(lines) and lines[item].strip().startswith("#["):
                if not lines[item].rstrip().endswith("]"):
                    fail(
                        f"{path}: unsupported multi-line attribute after #[cfg(test)] "
                        f"at line {item + 1}"
                    )
                item += 1
            if item >= len(lines):
                fail(f"{path}: unsupported shape after #[cfg(test)] at line {i + 1}")
            # A cfg-gated member is a complete, rustfmt-normalized physical
            # line ending in `,`. It is not a Rust item: passing it to
            # scan_item would scan past the enclosing struct/enum and can
            # report an unrelated unmatched closing brace. Deliberately keep
            # multi-line members unsupported so unknown grammar still fails
            # closed instead of accidentally hiding production code.
            if lines[item].rstrip().endswith(","):
                i = item + 1
            else:
                i = scan_item(lines, item)
        else:
            out.append((i + 1, lines[i]))
            i += 1
    return out


def main():
    if not RS_FILES:
        fail("no Rust files matched; guard misconfigured")
    violations = []
    prod_lines = 0
    for path in RS_FILES:
        rel = path.relative_to(ROOT)
        kept = strip_test_gated(str(rel), path.read_text())
        prod_lines += len(kept)
        for lineno, line in kept:
            for rx, _ in RS_FORBIDDEN:
                if rx.search(line):
                    violations.append(f"{rel}:{lineno}: {line.strip()}")
                    break
    for path in DOC_FILES:
        rel = path.relative_to(ROOT)
        for lineno, line in enumerate(path.read_text().splitlines(), 1):
            for rx, _ in DOC_FORBIDDEN:
                if rx.search(line):
                    violations.append(f"{rel}:{lineno}: {line.strip()}")
                    break
    if violations:
        print("guard: FORBIDDEN production matches (L1/L2):", file=sys.stderr)
        for v in violations:
            print(f"  {v}", file=sys.stderr)
        sys.exit(1)
    print(
        f"guards: OK ({len(RS_FILES)} rust files, {prod_lines} production lines, "
        f"{len(DOC_FILES)} doc files)"
    )


if __name__ == "__main__":
    main()
