import os
import unittest
from unittest import mock

import helpers  # noqa: F401  (sys.path setup)
from cairn_sessions import redact


class PatternScrubTest(unittest.TestCase):
    """Table-driven privacy rows: every pattern class fires and replaces."""

    ROWS = [
        ("openrouter_key",
         "key is sk-or-v1-abcdef1234567890abcdef1234567890abcdef12 ok",
         "sk-or-v1-abcdef1234567890abcdef1234567890abcdef12"),
        ("anthropic_key", "key is sk-ant-abcdef1234567890abcdef ok",
         "sk-ant-abcdef1234567890abcdef"),
        ("aws_key", "aws AKIAIOSFODNN7EXAMPLE here", "AKIAIOSFODNN7EXAMPLE"),
        ("github_token", "token ghp_abcdefghij1234567890abcd end",
         "ghp_abcdefghij1234567890abcd"),
        ("github_token", "token github_pat_11AAAAAA222222bbbbbb3333 end",
         "github_pat_11AAAAAA222222bbbbbb3333"),
        ("slack_token", "slack xoxb-123456789012-abcdefghijkl end",
         "xoxb-123456789012-abcdefghijkl"),
        (
            "jwt",
            "saw eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0."
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c in logs",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0."
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c",
        ),
        ("bearer", "Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789",
         "Bearer abcdefghijklmnopqrstuvwxyz0123456789"),
        ("credential", 'api_key = "supersecretvalue123456"',
         "supersecretvalue123456"),
        ("credential", "password:hunter2hunter2", "hunter2hunter2"),
        (
            "private_key",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjEAAAAABG5vbmU\n"
            "-----END OPENSSH PRIVATE KEY-----",
            "b3BlbnNzaC1rZXktdjEAAAAABG5vbmU",
        ),
    ]

    def test_every_pattern_class_scrubs(self):
        for cls, text, secret in self.ROWS:
            with self.subTest(cls=cls):
                out, hits = redact.redact_text(text)
                self.assertIn(cls, hits)
                self.assertIn(f"[REDACTED:{cls}]", out)
                # the raw secret itself must be gone
                self.assertNotIn(secret, out)

    def test_clean_text_untouched(self):
        text = "an ordinary sentence about chunking and retrieval"
        out, hits = redact.redact_text(text)
        self.assertEqual(out, text)
        self.assertEqual(hits, [])

    def test_pure_function(self):
        text = "sk-or-v1-abcdef1234567890abcdef1234567890abcdef12 twice sk-or-v1-zz"
        first, _ = redact.redact_text(text)
        second, _ = redact.redact_text(text)
        self.assertEqual(first, second)
        self.assertNotIn("sk-or-", first)

    def test_private_key_block_fully_removed(self):
        text = (
            "before\n-----BEGIN RSA PRIVATE KEY-----\nABC\nDEF\n"
            "-----END RSA PRIVATE KEY-----\nafter"
        )
        out, hits = redact.redact_text(text)
        self.assertIn("private_key", hits)
        self.assertNotIn("ABC", out)
        self.assertIn("before", out)
        self.assertIn("after", out)


class EnvScrubTest(unittest.TestCase):
    def test_env_values_scrubbed(self):
        env = {
            "CAIRN_SERVER_TOKEN": "tok-live-secret-abcdef",
            "OPENROUTER_API_KEY": "sk-or-v1-eeeeeeeeeeeeeeeeeeee",
        }
        out, hits = redact.redact_text("used tok-live-secret-abcdef here", env=env)
        self.assertIn("env", hits)
        self.assertNotIn("tok-live-secret-abcdef", out)
        self.assertIn("[REDACTED:env]", out)

    def test_short_env_values_ignored(self):
        env = {"CAIRN_SERVER_TOKEN": "short"}
        out, hits = redact.redact_text("short stays", env=env)
        self.assertEqual(out, "short stays")
        self.assertEqual(hits, [])

    def test_os_environ_used_by_default(self):
        with mock.patch.dict(os.environ, {"CAIRN_SERVER_TOKEN": "env-secret-123456"}):
            out, hits = redact.redact_text("has env-secret-123456 inside")
        self.assertIn("env", hits)
        self.assertNotIn("env-secret-123456", out)


class CapTest(unittest.TestCase):
    def test_first_prompt_cap(self):
        text = "x" * 600
        out = redact.redact_first_prompt(text)
        self.assertLessEqual(len(out), redact.FIRST_PROMPT_CAP + len("\n[truncated]"))
        self.assertTrue(out.endswith("[truncated]"))

    def test_message_cap(self):
        text = "y" * 5000
        out = redact.redact_message_text(text)
        self.assertLessEqual(len(out), redact.MESSAGE_TEXT_CAP + len("\n[truncated]"))

    def test_under_cap_untouched(self):
        self.assertEqual(redact.redact_first_prompt("short"), "short")
        self.assertEqual(redact.redact_message_text("short"), "short")


if __name__ == "__main__":
    unittest.main()
