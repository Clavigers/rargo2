"""Run with python3 -B -m unittest discover -s dev -p test_sim_seed_search.py."""

import contextlib
import io
import runpy
import subprocess
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import Mock, patch

search = runpy.run_path(str(Path(__file__).with_name("sim-seed-search.py")))
Failure = search["Failure"]

FAILED_BATCH = """seed 600: no drift broken at 100 ms: client 2 got "abc"
replay: rargo-sim --seed 600
seed 602: no drift broken at 200 ms: client 10 got "xyz"
replay: rargo-sim --seed 602
passing seeds: 1 event
2 of 3 seeds failed
"""


class SeedSearchTest(unittest.TestCase):
    def test_same_invariant_and_message_shape_share_a_signature(self):
        first = Failure(600, "no drift", 'client 2 got "abc"')
        second = Failure(602, "no drift", 'client 10 got "xyz"')
        self.assertEqual(first.signature, second.signature)
        self.assertNotEqual(first.signature, Failure(602, "no drift", "client 10 timed out").signature)
        self.assertNotEqual(first.signature, Failure(602, "no deadlock", second.detail).signature)

    @patch("subprocess.run")
    def test_failed_batch_uses_reported_seeds(self, run):
        run.return_value = subprocess.CompletedProcess([], 1, FAILED_BATCH)
        failures = search["run_batch"]("rargo-sim", 600, 603)
        self.assertEqual([failure.seed for failure in failures], [600, 602])
        self.assertEqual(run.call_args.args[0], ["rargo-sim", "--seeds", "600..603"])

    @patch("subprocess.run")
    def test_incomplete_batch_is_rejected(self, run):
        run.return_value = subprocess.CompletedProcess([], 1, FAILED_BATCH.split("passing seeds")[0])
        with self.assertRaises(RuntimeError):
            search["run_batch"]("rargo-sim", 600, 603)

    @patch("subprocess.check_output")
    @patch("subprocess.run")
    def test_deduplicates_within_a_batch_and_against_existing_issues(self, run, issues):
        first = Failure(600, "no drift", "client 2 got the wrong output")
        repeats = Failure(602, "no drift", "client 10 got the wrong output")
        existing = Failure(601, "no deadlock", "nothing left to happen")
        issues.return_value = f"<!-- rargo-sim-signature: {existing.signature} -->\n"

        search["report_failures"]("owner/repo", [first, repeats, existing])

        run.assert_called_once()
        self.assertIn("bug", run.call_args.args[0])
        body = run.call_args.kwargs["input"]
        self.assertIn("no drift", body)
        self.assertIn("rargo-sim --seed 600", body)
        self.assertIn(first.signature, body)

    def test_cursor_advances_only_after_reporting_and_resumes(self):
        main = search["main"]
        with TemporaryDirectory() as state:
            argv = [
                "sim-seed-search.py", "--repo", "owner/repo", "--state-dir", state,
                "--batch-size", "3", "--batches", "1",
            ]
            with (
                patch("sys.argv", argv),
                patch.dict(main.__globals__, {}),
                contextlib.redirect_stdout(io.StringIO()),
                contextlib.redirect_stderr(io.StringIO()),
            ):
                run_batch = Mock(return_value=[])
                report = Mock(side_effect=[OSError("reporting failed"), None, None])
                main.__globals__.update(run_batch=run_batch, report_failures=report)

                self.assertEqual(main(), 1)
                cursor = Path(state) / "next-seed"
                self.assertFalse(cursor.exists())

                self.assertEqual(main(), 0)
                self.assertEqual(cursor.read_text(), "603\n")

                self.assertEqual(main(), 0)
                self.assertEqual(cursor.read_text(), "606\n")
                self.assertEqual(
                    [call.args for call in run_batch.call_args_list],
                    [("rargo-sim", 600, 603), ("rargo-sim", 600, 603),
                     ("rargo-sim", 603, 606)],
                )
