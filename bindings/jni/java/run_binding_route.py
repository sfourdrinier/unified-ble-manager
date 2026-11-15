#!/usr/bin/env python3
"""Offline JVM test of the Android JNI facade and binding-scoped routes."""
import os
from pathlib import Path
import re
import subprocess
import tempfile

root = Path(__file__).resolve().parents[3]
source = (root / 'android/src/main/java/com/ubm/core/MobileCoreBridge.java').read_text()
pattern = r'public\s+static\s+native\s+int\s+(native(?:Complete|Ingest)\w+)\(([^)]*)\);'
callbacks = [(name, params) for name, params in re.findall(pattern, source, re.MULTILINE) if not name.endswith('ForBinding')]
rust = (root / 'bindings/jni/src/mobile.rs').read_text()
native_names = set(re.findall(r'Java_com_ubm_core_MobileCoreBridge_(native(?:Complete|Ingest)\w+ForBinding)', rust))
missing = {name + 'ForBinding' for name, _ in callbacks} - native_names
unexpected = native_names - {name + 'ForBinding' for name, _ in callbacks}
if missing or unexpected:
    print(f'Java and JNI callback surfaces differ: missing={sorted(missing)}, unexpected={sorted(unexpected)}', flush=True)
    if os.environ.get('UBM_JNI_PARTIAL_ROUTE_TEST') != '1':
        raise AssertionError('Java and JNI callback surfaces differ')
    callbacks = [(name, params) for name, params in callbacks if name + 'ForBinding' in native_names]
for name, _ in callbacks:
    if not re.search(r'\b' + name + r'ForBinding\s*\(', source):
        raise AssertionError(f'Android facade is missing {name}ForBinding')
env = os.environ.copy()
env['RUSTC_WRAPPER'] = ''
env.setdefault('CARGO_TARGET_DIR', '/tmp/ubm-522-jni-route-target')
subprocess.run(['cargo', 'rustc', '--offline', '--locked', '-p', 'ubm5_jni_echo', '--lib', '--', '--cfg', 'ubm_jni_roundtrip'], cwd=root, env=env, check=True)
with tempfile.TemporaryDirectory(prefix='ubm-522-jvm-') as directory:
    directory = Path(directory)
    facade = root / 'android/src/main/java/com/ubm/core/MobileCoreBridge.java'
    harness = root / 'bindings/jni/java/com/ubm/core'
    subprocess.run(['javac', '-d', str(directory), str(facade), str(harness / 'TestMobile.java'), str(harness / 'TestBindingRoute.java')], check=True)
    for test in ['TestBindingRoute', 'TestMobile']:
        subprocess.run(['java', '-Djava.library.path=' + env['CARGO_TARGET_DIR'] + '/debug', '-cp', str(directory), 'com.ubm.core.' + test], check=True, timeout=30)
