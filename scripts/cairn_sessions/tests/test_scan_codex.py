import unittest

import helpers
from cairn_sessions import scan_codex

CODEX_FIXTURE = helpers.FIXTURES / "codex" / "sessions"


def _scan(root, tier, tmp):
    ckpt = helpers.fresh_checkpoint(tmp)
    report = helpers.make_report("codex", str(root))
    parsed = scan_codex.scan(
        str(root),
        tier=tier,
        checkpoint=ckpt,
        prev_index=helpers.empty_prev(),
        report=report,
    )
    return parsed, report


class CodexParseTest(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)

    def test_main_rollout(self):
        parsed, report = _scan(CODEX_FIXTURE, "transcript", self._tmp.name)
        sessions = {pf.session_uid: pf.session for pf in parsed if pf.session}
        main = sessions["codex:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"]
        self.assertEqual(main.source, "codex")
        self.assertEqual(main.cwd, "/Users/test/codexproj")
        self.assertEqual(main.provider, "codexlb")
        self.assertEqual(main.model, "gpt-5.6-sol")  # from turn_context
        self.assertEqual(main.depth, 0)
        self.assertIsNone(main.parent_id)
        # user + assistant + tool output; developer + function_call dropped
        roles = [m.role for m in main.messages]
        self.assertEqual(roles, ["user", "assistant", "tool"])
        all_text = "\n".join(m.text for m in main.messages)
        self.assertNotIn("developer text must never be imported", all_text)
        self.assertNotIn("secret tool args must never be imported", all_text)
        self.assertIn("ok 12 passed", all_text)
        # github token in tool output redacted
        self.assertNotIn("ghp_abcdefghij1234567890abcd", all_text)
        self.assertIn("[REDACTED:github_token]", all_text)
        # usage from token_count total_token_usage
        self.assertIsNotNone(main.usage)
        self.assertEqual(main.usage.input, 1000)
        self.assertEqual(main.usage.cache_read, 200)
        self.assertEqual(main.usage.total, 1300)
        self.assertEqual(main.first_prompt, "fix the flaky importer test")

    def test_child_rollout_lineage(self):
        parsed, _ = _scan(CODEX_FIXTURE, "metadata", self._tmp.name)
        sessions = {pf.session_uid: pf.session for pf in parsed if pf.session}
        child = sessions["codex:bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb"]
        self.assertEqual(child.parent_id, "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        self.assertEqual(child.root_session_id,
                         "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa")
        self.assertEqual(child.depth, 1)

    def test_malformed_midfile_line_isolated(self):
        parsed, report = _scan(CODEX_FIXTURE, "transcript", self._tmp.name)
        bad = [e for e in report.errors if "cccccccc" in e.path]
        self.assertEqual(len(bad), 1)
        self.assertIn("malformed JSONL line", bad[0].reason)
        uids = {pf.session_uid for pf in parsed}
        self.assertIn("codex:aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", uids)
        self.assertNotIn("codex:cccccccc-cccc-4ccc-8ccc-cccccccccccc", uids)

    def test_denylisted_auth_never_opened(self):
        root = helpers.FIXTURES / "codex"
        with helpers.OpenSpy() as spy:
            _scan(root, "transcript", self._tmp.name)
        self.assertNotIn("auth.json", spy.basenames())


if __name__ == "__main__":
    unittest.main()
