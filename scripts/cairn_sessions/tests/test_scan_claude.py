import unittest

import helpers
from cairn_sessions import scan_claude

CLAUDE_FIXTURE = helpers.FIXTURES / "claude"
MAIN_TRANSCRIPT = CLAUDE_FIXTURE / "projects" / "-Users-test-proj" / (
    "dddddddd-dddd-4ddd-8ddd-dddddddddddd.jsonl"
)


def _scan(root, tier, tmp):
    ckpt = helpers.fresh_checkpoint(tmp)
    report = helpers.make_report("claude", str(root))
    parsed = scan_claude.scan(
        str(root),
        tier=tier,
        checkpoint=ckpt,
        prev_index=helpers.empty_prev(),
        report=report,
    )
    return parsed, report


class ClaudeIndexTierTest(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)

    def test_metadata_tier_uses_index_without_opening_transcript(self):
        with helpers.OpenSpy() as spy:
            parsed, report = _scan(CLAUDE_FIXTURE, "metadata", self._tmp.name)
        self.assertNotIn(str(MAIN_TRANSCRIPT), spy.opened)
        self.assertIn("sessions-index.json", spy.basenames())
        sessions = {pf.session_uid: pf.session for pf in parsed if pf.session}
        main = sessions["claude-code:dddddddd-dddd-4ddd-8ddd-dddddddddddd"]
        self.assertEqual(main.title, "Importer design review")
        self.assertEqual(main.first_prompt, "review the session importer design")
        self.assertEqual(main.message_count, 4)
        self.assertEqual(main.git_branch, "main")
        self.assertEqual(main.repo_path, "/Users/test/proj")
        self.assertEqual(main.cwd, "/Users/test/proj")
        from cairn_sessions.model import iso_to_ms

        self.assertEqual(main.created_at_ms, iso_to_ms("2026-08-01T04:00:00.000Z"))
        self.assertEqual(main.updated_at_ms, iso_to_ms("2026-08-01T04:00:30.000Z"))
        self.assertEqual(main.messages, [])

    def test_denylisted_files_never_opened(self):
        with helpers.OpenSpy() as spy:
            _scan(CLAUDE_FIXTURE, "metadata", self._tmp.name)
        self.assertNotIn(".claude.json", spy.basenames())
        self.assertNotIn("settings.json", spy.basenames())


class ClaudeTranscriptTierTest(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)

    def test_transcript_parse(self):
        parsed, _ = _scan(CLAUDE_FIXTURE, "transcript", self._tmp.name)
        sessions = {pf.session_uid: pf.session for pf in parsed if pf.session}
        main = sessions["claude-code:dddddddd-dddd-4ddd-8ddd-dddddddddddd"]
        roles = [m.role for m in main.messages]
        self.assertEqual(roles, ["user", "assistant", "tool", "assistant"])
        all_text = "\n".join(m.text for m in main.messages)
        self.assertNotIn("hidden reasoning", all_text)  # thinking dropped
        self.assertNotIn("xoxb-123456789012-abcdefghijkl", all_text)
        self.assertIn("[REDACTED:slack_token]", all_text)
        self.assertEqual(main.model, "claude-opus-4-1")
        self.assertIsNotNone(main.usage)
        self.assertEqual(main.usage.input, 1700)  # 800 + 900
        self.assertEqual(main.usage.cache_read, 90)  # 40 + 50
        # index enrichment still applies on transcript tier
        self.assertEqual(main.title, "Importer design review")
        self.assertEqual(main.git_branch, "main")

    def test_subagent_identity(self):
        parsed, _ = _scan(CLAUDE_FIXTURE, "transcript", self._tmp.name)
        sessions = {pf.session_uid: pf.session for pf in parsed if pf.session}
        sub = sessions["claude-code:agent-abc123def456"]
        parent = sessions["claude-code:dddddddd-dddd-4ddd-8ddd-dddddddddddd"]
        # child keeps its own identity; parent pointer uses the parent's id
        self.assertEqual(sub.native_id, "agent-abc123def456")
        self.assertNotEqual(sub.session_uid, parent.session_uid)
        self.assertEqual(sub.parent_id, parent.native_id)
        self.assertEqual(sub.depth, 1)
        self.assertEqual(sub.root_session_id, parent.root_session_id)
        self.assertEqual(parent.subagent_count, 1)

    def test_malformed_nonindexed_file_isolated(self):
        parsed, report = _scan(CLAUDE_FIXTURE, "transcript", self._tmp.name)
        bad = [e for e in report.errors if "eeeeeeee" in e.path]
        self.assertEqual(len(bad), 1)
        uids = {pf.session_uid for pf in parsed}
        self.assertIn("claude-code:dddddddd-dddd-4ddd-8ddd-dddddddddddd", uids)
        self.assertNotIn("claude-code:eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee", uids)


if __name__ == "__main__":
    unittest.main()
