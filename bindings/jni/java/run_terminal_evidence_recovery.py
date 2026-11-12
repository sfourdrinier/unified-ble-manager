#!/usr/bin/env python3
"""Run the real JNI terminal-evidence recovery regression on a JVM."""
import os
from pathlib import Path
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[3]
env = os.environ.copy()
env["RUSTC_WRAPPER"] = ""
env.setdefault("CARGO_TARGET_DIR", "/tmp/ubm-terminal-evidence-jni-target")
subprocess.run([
    "cargo", "rustc", "--offline", "--locked", "-p", "ubm5_jni_echo",
    "--features", "jni-roundtrip", "--lib", "--", "--cfg", "ubm_jni_roundtrip",
], cwd=ROOT, env=env, check=True)
with tempfile.TemporaryDirectory(prefix="ubm-terminal-evidence-jvm-") as directory:
    classes = Path(directory)
    facade = ROOT / "android/src/main/java/com/ubm/core/MobileCoreBridge.java"
    harness = ROOT / "bindings/jni/java/com/ubm/core"
    subprocess.run([
        "javac", "-d", str(classes), str(facade), str(harness / "TestMobile.java"),
        str(harness / "TestTerminalEvidenceRecovery.java"),
    ], check=True)
    subprocess.run([
        "java", "-Djava.library.path=" + env["CARGO_TARGET_DIR"] + "/debug",
        "-cp", str(classes), "com.ubm.core.TestTerminalEvidenceRecovery",
    ], check=True, timeout=30)
