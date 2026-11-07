"""Focused regression tests for remote_tests.py's failure accounting."""

from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import time
import unittest
from pathlib import Path


MODULE_PATH = Path(__file__).with_name("remote_tests.py")
SPEC = importlib.util.spec_from_file_location("remote_tests", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
remote_tests = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = remote_tests
SPEC.loader.exec_module(remote_tests)


class RemoteRunnerAccountingTests(unittest.TestCase):
    def test_filter_typo_has_no_runnable_match(self) -> None:
        names = [
            "hung_hwnd_rejoins_after_suspend_past_early_recovery_window",
            "unrelated_test",
        ]
        self.assertEqual(
            remote_tests.matching_test_names(
                names, set(), "hung_hwnd_rejoins_after_suspend_past_early_recoveri"
            ),
            [],
        )

    def test_known_late_resume_filter_selects_a_runnable_test(self) -> None:
        names = [
            "hung_hwnd_rejoins_after_suspend_past_early_recovery_window",
            "unrelated_test",
        ]
        self.assertEqual(
            remote_tests.matching_test_names(
                names,
                set(),
                "hung_hwnd_rejoins_after_suspend_past_early_recovery_window",
            ),
            ["hung_hwnd_rejoins_after_suspend_past_early_recovery_window"],
        )

    def test_ignored_filter_match_is_not_counted_as_executed(self) -> None:
        names = ["late_resume", "late_resume_ignored"]
        self.assertEqual(
            remote_tests.matching_test_names(
                names, {"late_resume_ignored"}, "late_resume"
            ),
            ["late_resume"],
        )

    def test_rust_test_listing_parser_ignores_summary_and_benchmarks(self) -> None:
        self.assertEqual(
            remote_tests.parse_listed_test_names(
                "alpha::first: test\nbeta::second: test\n"
                "benchmark: bench\n2 tests, 1 benchmark\n"
            ),
            ["alpha::first", "beta::second"],
        )

    def test_test_case_summary_counts_pass_fail_and_ignored(self) -> None:
        self.assertEqual(
            remote_tests.parse_test_summary(
                "test result: FAILED. 4 passed; 1 failed; 2 ignored; "
                "0 measured; 10 filtered out; finished in 0.20s"
            ),
            {"passed": 4, "failed": 1, "ignored": 2},
        )

    def test_remote_marker_requires_successful_ssh_controller(self) -> None:
        self.assertEqual(remote_tests.resolve_remote_exit_code(0, ["0"]), 0)
        self.assertEqual(remote_tests.resolve_remote_exit_code(0, ["17"]), 17)
        self.assertEqual(remote_tests.resolve_remote_exit_code(42, ["0"]), 42)

    def test_missing_malformed_duplicate_and_out_of_range_markers_fail(self) -> None:
        for markers in ([], ["oops"], ["0", "0"], ["256"]):
            with self.subTest(markers=markers):
                self.assertNotEqual(
                    remote_tests.resolve_remote_exit_code(0, markers), 0
                )

    def test_stream_timeout_is_enforced_during_continuous_output(self) -> None:
        class QuietLogger:
            def write(self, *_args: object, **_kwargs: object) -> None:
                pass

        with tempfile.TemporaryDirectory() as temp_dir:
            started = time.monotonic()
            with self.assertRaisesRegex(remote_tests.RunnerError, "timed out"):
                remote_tests.stream_local_process(
                    [
                        sys.executable,
                        "-c",
                        "import time\n"
                        "while True:\n"
                        " print('heartbeat', flush=True)\n"
                        " time.sleep(0.001)\n",
                    ],
                    env=os.environ.copy(),
                    timeout=0.35,
                    label="continuous-output-probe",
                    logger=QuietLogger(),  # type: ignore[arg-type]
                    mirror=Path(temp_dir) / "probe.log",
                )
            self.assertLess(time.monotonic() - started, 5)

    @unittest.skipUnless(os.name == "nt", "Windows process-tree termination")
    def test_stream_timeout_terminates_descendant_processes(self) -> None:
        class QuietLogger:
            def write(self, *_args: object, **_kwargs: object) -> None:
                pass

        with tempfile.TemporaryDirectory() as temp_dir:
            marker = Path(temp_dir) / "descendant-survived.txt"
            child_script = "\n".join(
                [
                    "import pathlib, sys, time",
                    "time.sleep(1.5)",
                    "pathlib.Path(sys.argv[1]).write_text('survived')",
                ]
            )
            parent_script = "\n".join(
                [
                    "import subprocess, sys, time",
                    "subprocess.Popen(",
                    f"    [sys.executable, '-c', {child_script!r}, sys.argv[1]],",
                    "    creationflags=0x08000000,",
                    ")",
                    "while True:",
                    "    print('heartbeat', flush=True)",
                    "    time.sleep(0.001)",
                ]
            )
            with self.assertRaisesRegex(remote_tests.RunnerError, "timed out"):
                remote_tests.stream_local_process(
                    [sys.executable, "-c", parent_script, str(marker)],
                    env=os.environ.copy(),
                    timeout=0.35,
                    label="process-tree-timeout-probe",
                    logger=QuietLogger(),  # type: ignore[arg-type]
                    mirror=Path(temp_dir) / "tree-probe.log",
                )
            time.sleep(1.8)
            self.assertFalse(marker.exists(), "timed-out descendant was not killed")


if __name__ == "__main__":
    unittest.main(verbosity=2)
