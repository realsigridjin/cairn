import json
import os
import unittest

import helpers
from cairn_sessions import checkpoint as checkpoint_mod

SESSION_ID = "77777777-7777-4777-8777-777777777777"
UID = f"senpi:{SESSION_ID}"


class IncrementalAppendTest(unittest.TestCase):
    """Incremental-append handling over a synthetic append-only senpi store."""

    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)
        self.root = os.path.join(self._tmp.name, "store")
        os.makedirs(self.root)
        self.session_file = os.path.join(self.root, "session.jsonl")
        self.out = os.path.join(self._tmp.name, "canon.jsonl")
        self.ckpt_path = os.path.join(self._tmp.name, "checkpoint.json")

    def _write_session(self, n_messages):
        with open(self.session_file, "w", encoding="utf-8") as fh:
            fh.write(helpers.senpi_session_lines(n_messages, SESSION_ID))

    def _append_messages(self, start, count):
        lines = helpers.senpi_session_lines(start + count, SESSION_ID).splitlines()
        with open(self.session_file, "a", encoding="utf-8") as fh:
            for row in lines[-count:]:
                fh.write(row + "\n")

    def _run(self, tier="transcript"):
        return helpers.run_senpi_pipeline(
            self.root, tier=tier, out=self.out, ckpt_path=self.ckpt_path
        )

    def _corpus_bytes(self):
        with open(self.out, "rb") as fh:
            return fh.read()

    def test_append_only_new_chunks_change(self):
        self._write_session(10)
        report1, lines1, _ = self._run()
        self.assertEqual(report1.files_imported, 1)
        self.assertEqual(
            sorted(lines1), [f"s:{UID}:m:0-7", f"s:{UID}:m:8-15", f"s:{UID}:meta"]
        )
        offset1 = checkpoint_mod.Checkpoint.load(self.ckpt_path).entry(
            self.session_file
        )["byte_offset"]

        self._append_messages(10, 3)
        report2, lines2, _ = self._run()
        entry2 = checkpoint_mod.Checkpoint.load(self.ckpt_path).entry(
            self.session_file
        )
        # checkpoint offset advanced to the new end of file
        self.assertGreater(entry2["byte_offset"], offset1)
        self.assertEqual(entry2["byte_offset"], os.path.getsize(self.session_file))
        self.assertEqual(entry2["msg_count"], 13)
        # full window re-emitted byte-identical; tail window keeps its id and grows
        self.assertEqual(lines2[f"s:{UID}:m:0-7"], lines1[f"s:{UID}:m:0-7"])
        self.assertIn("msg 12 body", lines2[f"s:{UID}:m:8-15"])
        self.assertIn("msg 9 body", lines2[f"s:{UID}:m:8-15"])
        # meta chunk refreshed in place (same id, updated count)
        self.assertIn('"message_count":13', lines2[f"s:{UID}:meta"])
        # no duplicate or orphaned ids
        self.assertEqual(len(lines2), len(set(lines2)))
        self.assertEqual(
            sorted(lines2), [f"s:{UID}:m:0-7", f"s:{UID}:m:8-15", f"s:{UID}:meta"]
        )

    def test_window_rollover_mints_new_id(self):
        self._write_session(10)
        _, lines1, _ = self._run()
        self._append_messages(10, 8)  # -> 18 messages: windows 0-7, 8-15, 16-23
        _, lines2, _ = self._run()
        self.assertEqual(lines2[f"s:{UID}:m:0-7"], lines1[f"s:{UID}:m:0-7"])
        self.assertIn(f"s:{UID}:m:16-23", lines2)
        # window 8-15 completed with the original tail messages
        self.assertIn("msg 8 body", lines2[f"s:{UID}:m:8-15"])
        self.assertIn("msg 15 body", lines2[f"s:{UID}:m:8-15"])

    def test_unchanged_rerun_is_byte_identical(self):
        self._write_session(10)
        self._run()
        first = self._corpus_bytes()
        report2, _, _ = self._run()
        second = self._corpus_bytes()
        self.assertEqual(first, second)
        self.assertEqual(report2.files_skipped_unchanged, 1)
        self.assertEqual(report2.files_imported, 0)

    def test_crash_between_corpus_and_checkpoint_converges(self):
        self._write_session(10)
        self._run()
        first = self._corpus_bytes()
        # simulate the crash: corpus written, checkpoint lost
        os.remove(self.ckpt_path)
        report, _, _ = self._run()
        self.assertEqual(report.files_imported, 1)  # full re-parse
        self.assertEqual(self._corpus_bytes(), first)  # idempotent convergence

    def test_inflight_partial_line_held(self):
        self._write_session(10)
        self._run()
        first = self._corpus_bytes()
        with open(self.session_file, "a", encoding="utf-8") as fh:
            fh.write('{"type":"message","id":"partial","message":{"role":"user","con')
        report, lines, _ = self._run()
        self.assertEqual(report.errors, [])
        self.assertEqual(self._corpus_bytes(), first)
        # offset did not consume the partial line
        entry = checkpoint_mod.Checkpoint.load(self.ckpt_path).entry(
            self.session_file
        )
        self.assertLess(entry["byte_offset"], os.path.getsize(self.session_file))
        # complete the line: next run consumes it
        with open(self.session_file, "a", encoding="utf-8") as fh:
            fh.write('tent":[{"type":"text","text":"completed later"}]},'
                     '"timestamp":"2026-08-01T00:09:00.000Z"}\n')
        report2, lines2, _ = self._run()
        self.assertEqual(report2.errors, [])
        self.assertIn("completed later", lines2[f"s:{UID}:m:8-15"])

    def test_metadata_tier_incremental_counts(self):
        self._write_session(10)
        _, lines1, _ = self._run(tier="metadata")
        self.assertEqual(sorted(lines1), [f"s:{UID}:meta"])
        self.assertIn('"message_count":10', lines1[f"s:{UID}:meta"])
        self._append_messages(10, 3)
        _, lines2, _ = self._run(tier="metadata")
        self.assertEqual(sorted(lines2), [f"s:{UID}:meta"])
        self.assertIn('"message_count":13', lines2[f"s:{UID}:meta"])


if __name__ == "__main__":
    unittest.main()
