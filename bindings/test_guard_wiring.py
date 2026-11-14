#!/usr/bin/env python3
"""Regression tests for bindings/guard_wiring.py."""

import importlib.util
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("guard_wiring.py")
SPEC = importlib.util.spec_from_file_location("guard_wiring", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
GUARD = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(GUARD)


class StripTestGatedTests(unittest.TestCase):
    def test_strips_a_cfg_test_struct_field(self):
        text = """struct DispatchCentral {
    retained: u64,
    #[cfg(test)]
    ordered_recording_start_panic: AtomicU64,
    counters: DispatchCounters,
}
"""

        kept = GUARD.strip_test_gated("fixture.rs", text)

        self.assertEqual(
            [line for _, line in kept],
            [
                "struct DispatchCentral {",
                "    retained: u64,",
                "    counters: DispatchCounters,",
                "}",
            ],
        )

    def test_strips_a_cfg_test_module_with_a_path_attribute(self):
        text = """#[cfg(test)]
#[path = "platform_metadata_transport_522_tests.rs"]
mod platform_metadata_transport_522_tests;
fn production() {}
"""

        kept = GUARD.strip_test_gated("fixture.rs", text)

        self.assertEqual([line for _, line in kept], ["fn production() {}"])

    def test_excludes_only_explicitly_proven_test_modules(self):
        test_only = GUARD.test_only_rs_files(GUARD.ROOT)
        self.assertEqual(
            {path.relative_to(GUARD.ROOT).as_posix() for path in test_only},
            set(GUARD.TEST_ONLY_MODULES),
        )
        self.assertTrue(all(path not in GUARD.RS_FILES for path in test_only))

    def test_unlisted_tests_filename_is_still_scanned(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "bindings/napi/src/production_tests.rs"
            source.parent.mkdir(parents=True)
            source.write_text("fn production() { fallible().unwrap(); }\n")

            candidates = GUARD.production_rs_files(root, test_only_modules={})

            self.assertEqual(candidates, [source])


if __name__ == "__main__":
    unittest.main()
