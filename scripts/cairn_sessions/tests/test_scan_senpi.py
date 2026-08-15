import os
import unittest

import helpers
from cairn_sessions import scan_senpi, scanutil

SENPI_FIXTURE = helpers.FIXTURES / "senpi"
ROOT_SESSION = SENPI_FIXTURE / "sessions" / "--Users-test-myproj--" / (
    "2026-08-01T00-00-00-000Z_11111111-1111-4111-8111-111111111111.jsonl"
)
MALFORMED = SENPI_FIXTURE / "sessions" / "--Users-test-myproj--" / (
    "2026-08-01T01-00-00-000Z_33333333-3333-4333-8333-333333333333.jsonl"
)
TASKS_ROOT = SENPI_FIXTURE / "senpi-task"


def _scan(root, tier, tmp, tasks_roots=()):
    ckpt = helpers.fresh_checkpoint(tmp)
    report = helpers.make_report("senpi", str(root))
    parsed = scan_senpi.scan(
        str(root),
        tier=tier,
        checkpoint=ckpt,
        prev_index=helpers.empty_prev(),
        report=report,
        tasks_roots=tasks_roots,
    )
    return parsed, report


class SenpiParseTest(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)

    def test_root_session_transcript_tier(self):
        parsed, report = _scan(SENPI_FIXTURE, "transcript", self._tmp.name,
                               tasks_roots=[str(TASKS_ROOT)])
        sessions = {pf.session_uid: pf.session for pf in parsed if pf.session}
        root = sessions["senpi:11111111-1111-4111-8111-111111111111"]
        self.assertEqual(root.source, "senpi")
        self.assertEqual(root.cwd, "/Users/test/myproj")
        self.assertEqual(root.provider, "codex-lb")
        self.assertEqual(root.model, "gpt-5.6-sol")
        self.assertEqual(root.message_count, 4)
        self.assertEqual(root.depth, 0)
        self.assertIsNone(root.parent_id)
        self.assertEqual(root.root_session_id, root.native_id)
        self.assertEqual(root.store_format, "jsonl")
        # usage: last assistant message wins
        self.assertIsNotNone(root.usage)
        self.assertEqual(root.usage.total, 1756)
        self.assertAlmostEqual(root.usage.cost, 0.0051)
        # surface projection
        roles = [m.role for m in root.messages]
        self.assertEqual(roles, ["user", "assistant", "tool", "assistant"])
        self.assertEqual([m.seq for m in root.messages], [0, 1, 2, 3])
        # thinking blocks are structural drops
        all_text = "\n".join(m.text for m in root.messages)
        self.assertNotIn("planning the answer", all_text)
        self.assertNotIn("encblob", all_text)
        # secrets are redacted
        self.assertNotIn("sk-or-v1-abcdef", all_text)
        self.assertNotIn("AKIAIOSFODNN7EXAMPLE", all_text)
        self.assertNotIn("eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9", all_text)
        self.assertIn("[REDACTED:", all_text)
        # first prompt captured and capped
        self.assertEqual(root.first_prompt, "how do I wire the cairn ingest pipeline?")
        # timestamps are content-derived
        from cairn_sessions.model import iso_to_ms

        self.assertEqual(root.created_at_ms, iso_to_ms("2026-08-01T00:00:00.000Z"))
        self.assertEqual(root.updated_at_ms, 1786233607000)

    def test_metadata_tier_has_no_messages(self):
        parsed, _ = _scan(SENPI_FIXTURE, "metadata", self._tmp.name)
        sessions = [pf.session for pf in parsed if pf.session]
        self.assertTrue(sessions)
        for session in sessions:
            self.assertEqual(session.messages, [])
            self.assertGreaterEqual(session.message_count, 1)
            self.assertTrue(session.first_prompt)

    def test_malformed_file_isolated(self):
        parsed, report = _scan(SENPI_FIXTURE, "transcript", self._tmp.name)
        bad = [e for e in report.errors if "33333333" in e.path]
        self.assertEqual(len(bad), 1)
        self.assertIn("truncated", bad[0].reason)
        # other sessions in the same store still imported
        uids = {pf.session_uid for pf in parsed}
        self.assertIn("senpi:11111111-1111-4111-8111-111111111111", uids)
        self.assertNotIn("senpi:33333333-3333-4333-8333-333333333333", uids)

    def test_denylisted_files_never_opened(self):
        with helpers.OpenSpy() as spy:
            _scan(SENPI_FIXTURE, "transcript", self._tmp.name)
        self.assertNotIn("auth.json", spy.basenames())
        self.assertNotIn(".env", spy.basenames())

    def test_lineage_from_tasks_root(self):
        parsed, _ = _scan(SENPI_FIXTURE, "metadata", self._tmp.name,
                          tasks_roots=[str(TASKS_ROOT)])
        sessions = {pf.session_uid: pf.session for pf in parsed if pf.session}
        child = sessions["senpi:22222222-2222-4222-8222-222222222222"]
        self.assertEqual(child.parent_id, "11111111-1111-4111-8111-111111111111")
        self.assertEqual(child.root_session_id,
                         "11111111-1111-4111-8111-111111111111")
        self.assertEqual(child.depth, 1)
        root = sessions["senpi:11111111-1111-4111-8111-111111111111"]
        self.assertEqual(root.subagent_count, 1)

    def test_pi_source_detection(self):
        pi_fixture = helpers.FIXTURES / "pi"
        parsed, report = _scan(pi_fixture, "transcript", self._tmp.name)
        uids = {pf.session_uid for pf in parsed if pf.session}
        self.assertIn("pi:44444444-4444-4444-8444-444444444444", uids)
        with helpers.OpenSpy() as spy:
            _scan(pi_fixture, "transcript", self._tmp.name)
        self.assertNotIn("auth.json", spy.basenames())


if __name__ == "__main__":
    unittest.main()
