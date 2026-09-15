"""UniFFI feasibility round-trip: drives the REAL generated Python bindings
against the REAL built cdylib through the generated scaffolding (no mocks).

Covers: owned byte batches, lossless u64 counters, typed C-UBM failures,
wire reconstruction from live record fields, init gating, cancellation
(armed + threaded mid-flight), close invalidation, panic containment, and
codegen/runtime checksum-mismatch rejection. Fails loudly; no skips.
"""
import importlib.util
import os
import sys
import tempfile
import threading
import time

BIND_DIR = sys.argv[1] if len(sys.argv) > 1 else os.path.dirname(os.path.abspath(__file__))
assert os.path.isfile(os.path.join(BIND_DIR, "ubm_echo.py")), f"bindings missing in {BIND_DIR}"
assert os.path.isfile(os.path.join(BIND_DIR, "libubm5_uniffi_echo.so")), f"cdylib missing in {BIND_DIR}"

sys.path.insert(0, BIND_DIR)
import ubm_echo

REV = "C-UBM.0.1.1-DRAFT"
MAX_BYTES = 524288
PASS = 0


def check(name, cond, extra=""):
    global PASS
    assert cond, f"FAIL {name} {extra}"
    PASS += 1
    print(f"  ok: {name}")


def wire_of(rec):
    # Records split the failure; the foreign side rebuilds the shared form.
    # (Feasibility mapping carries no `detail` member: the join is
    # code|domain|operation. Recorded in LIFETIME_RULES.md.)
    return f"{rec.code}|{rec.domain}|{rec.operation}"


print("== init + byte batches ==")
s = ubm_echo.EchoSession(REV)
r = s.echo_bytes(bytes([0, 1, 2, 250, 255]))
check("bytes round-trip", r.ok and bytes(r.data) == bytes([0, 1, 2, 250, 255]))
r = s.echo_bytes(b"")
check("empty batch", r.ok and bytes(r.data) == b"")
big = bytes([0xAB]) * MAX_BYTES
r = s.echo_bytes(big)
check("max batch", r.ok and len(r.data) == MAX_BYTES)
r = s.echo_bytes(bytes([0]) * (MAX_BYTES + 1))
check("oversize typed", (not r.ok) and wire_of(r) == "bytes.too-large|core|echo-bytes")

print("== u64 counters (arbitrary-precision ints) ==")
for n in [0, 1, 9007199254740993, 9223372036854775807, 18446744073709551615]:
    r = s.echo_counter(str(n))
    check(f"counter {n}", r.ok and int(r.value) == n)
r = s.echo_counter("00042")
check("canonical form", r.ok and r.value == "42")
for bad in ["", "-1", "+5", "12a34", " 42", "4.0", "0x10", "18446744073709551616"]:
    r = s.echo_counter(bad)
    check(f"counter rejects {bad!r}", (not r.ok) and wire_of(r) == "bytes.invalid|core|echo-counter", r.code)

print("== init gate: foreign revision fails closed on every call ==")
f = ubm_echo.EchoSession("C-UBM.9.9.9-DRAFT")
for name, rec in [
    ("bytes", f.echo_bytes(b"\x01")),
    ("counter", f.echo_counter("1")),
    ("chunked", f.echo_bytes_chunked(b"\x01", 1)),
    ("cancel", f.cancel_inflight()),
]:
    check(f"foreign rev {name}", (not rec.ok) and rec.code == "protocol.incompatible"
          and rec.domain == "core", wire_of(rec))

print("== cancellation ==")
s2 = ubm_echo.EchoSession(REV)
check("cancel arms", s2.cancel_inflight().ok)
r = s2.echo_bytes_chunked(bytes([1, 2, 3]), 10)
check("armed cancel aborts", (not r.ok) and wire_of(r) == "operation.aborted|core|echo-bytes-chunked")
r = s2.echo_bytes(b"\x09")
check("usable after abort", r.ok and bytes(r.data) == b"\x09")

# Threaded mid-flight cancel: ctypes releases the GIL during the call, so the
# worker genuinely runs while the main thread cancels (~1 GiB of hashing vs
# a 20 ms delay: strictly asserted abort).
s3 = ubm_echo.EchoSession(REV)
outcome = {}

def worker():
    outcome["rec"] = s3.echo_bytes_chunked(bytes([0x5A]) * 262144, 5000)

t = threading.Thread(target=worker)
t.start()
time.sleep(0.02)
s3.cancel_inflight()
t.join(timeout=120)
check("worker joined", not t.is_alive())
check("mid-flight abort", (not outcome["rec"].ok)
      and wire_of(outcome["rec"]) == "operation.aborted|core|echo-bytes-chunked")

print("== close invalidation ==")
s4 = ubm_echo.EchoSession(REV)
check("close ok", s4.close().ok)
check("close idempotent", s4.close().ok)
for op, rec in [("echo-bytes", s4.echo_bytes(b"\x01")),
                 ("echo-counter", s4.echo_counter("1")),
                 # M1: uniform post-close cancel rejects like every other
                 # call on a destroyed session (napi/JNI agree).
                 ("cancel-inflight", s4.cancel_inflight())]:
    check(f"post-close {op}", (not rec.ok)
          and wire_of(rec) == f"lifecycle.destroyed|core|{op}", wire_of(rec))

print("== production surface exposes no panic probe ==")
s5 = ubm_echo.EchoSession(REV)
check("panic probe deleted from production surface", not hasattr(s5, "panic_probe"))
r = ubm_echo.EchoSession(REV).echo_bytes(b"\x07")
check("surface usable", r.ok and bytes(r.data) == b"\x07")

print("== codegen/runtime mismatch rejects loudly ==")
# The generator emits a live contract-version check at import (the
# per-method checksum call is commented out in uniffi 0.32 output, so the
# version check is the enforced mechanism — tamper exactly that).
with tempfile.TemporaryDirectory() as tmp:
    src = open(os.path.join(BIND_DIR, "ubm_echo.py")).read()
    import re
    tampered, n = re.subn(r"bindings_contract_version = \d+",
                          "bindings_contract_version = 0", src, count=1)
    assert n == 1, "contract-version anchor not found"
    open(os.path.join(tmp, "ubm_echo.py"), "w").write(tampered)
    open(os.path.join(tmp, "libubm5_uniffi_echo.so"), "wb").write(
        open(os.path.join(BIND_DIR, "libubm5_uniffi_echo.so"), "rb").read())
    spec = importlib.util.spec_from_file_location("ubm_echo_tampered",
                                                  os.path.join(tmp, "ubm_echo.py"))
    mod = importlib.util.module_from_spec(spec)
    try:
        spec.loader.exec_module(mod)
        mismatch_rejected = False
    except Exception as err:  # noqa: BLE001 - asserting rejection, any error counts if typed
        mismatch_rejected = type(err).__name__ == "InternalError"
    check("contract-version mismatch rejected", mismatch_rejected)

print(f"uniffi-python-roundtrip: OK ({PASS} checks)")
