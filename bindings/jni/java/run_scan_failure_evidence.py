#!/usr/bin/env python3
"""Run the unadvertised typed scan JNI entry against a JVM and a scripted radio."""
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[3]
FLOOR = 12 * 1024**3

def guarded(command, **kwargs):
    if shutil.disk_usage(ROOT).free < FLOOR:
        raise SystemExit("disk guard: less than 12 GiB free")
    process = subprocess.Popen(command, start_new_session=True, **kwargs)
    deadline = time.monotonic() + 120
    try:
        while process.poll() is None:
            if shutil.disk_usage(ROOT).free < FLOOR:
                raise RuntimeError("disk guard: stopped below 12 GiB")
            if time.monotonic() >= deadline:
                raise RuntimeError("test command timed out")
            time.sleep(1)
    except BaseException:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait()
        raise
    if process.returncode:
        raise SystemExit(process.returncode)

env = os.environ.copy()
env["RUSTC_WRAPPER"] = ""
env.setdefault("CARGO_TARGET_DIR", "/tmp/ubm-522-jni-route-target")
guarded(["cargo", "rustc", "--offline", "--locked", "-p", "ubm5_jni_echo", "--features", "jni-roundtrip", "--lib", "--", "--cfg", "ubm_jni_roundtrip"], cwd=ROOT, env=env)
with tempfile.TemporaryDirectory(prefix="ubm-scan-jni-") as directory:
    directory = Path(directory)
    facade = ROOT / "android/src/main/java/com/ubm/core/MobileCoreBridge.java"
    harness = ROOT / "bindings/jni/java/com/ubm/core"
    guarded(["javac", "-d", str(directory), str(facade), str(harness / "TestMobile.java"), str(harness / "TestBindingRoute.java"), str(harness / "TestScanFailureEvidence.java")])
    guarded(["java", "-Djava.library.path=" + env["CARGO_TARGET_DIR"] + "/debug", "-cp", str(directory), "com.ubm.core.TestScanFailureEvidence"])
