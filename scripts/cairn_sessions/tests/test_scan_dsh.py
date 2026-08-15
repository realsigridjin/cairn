import os
import shutil
import unittest

import helpers
from cairn_sessions import checkpoint as checkpoint_mod
from cairn_sessions import scan_dsh, scanutil


def _scan(root, tier, tmp, **kwargs):
    ckpt = helpers.fresh_checkpoint(tmp)
    report = helpers.make_report("dsh", str(root))
    parsed = scan_dsh.scan(
        str(root),
        tier=tier,
        checkpoint=ckpt,
        prev_index=helpers.empty_prev(),
        report=report,
        **kwargs,
    )
    return parsed, report, ckpt


@unittest.skipUnless(helpers.zstd_available(), "no zstd backend on this host")
class DshTranscriptTest(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)

    def test_stdlib_decode_and_surface_projection(self):
        root = helpers.build_dsh_store(self._tmp.name)
        parsed, report, _ = _scan(root, "transcript", self._tmp.name,
                                  zstd_backend="stdlib")
        self.assertEqual(report.errors, [])
        session = parsed[0].session
        self.assertEqual(
            session.session_uid, "dsh:session-99999999-9999-4999-8999-999999999999"
        )
        self.assertEqual(session.store_format, "jsonl.zstd")
        self.assertEqual(session.cwd, "/Users/test/dshproj")
        self.assertEqual(session.repo_path, "/Users/test/dshproj")
        self.assertEqual(session.agent_role, "standard")
        self.assertEqual(session.depth, 0)
        self.assertEqual(session.created_at_ms, 1786630000000)
        self.assertEqual(session.updated_at_ms, 1786630003000)
        roles = [m.role for m in session.messages]
        self.assertEqual(roles, ["user", "assistant", "tool", "assistant"])
        all_text = "\n".join(m.text for m in session.messages)
        self.assertNotIn("hidden dsh reasoning", all_text)
        self.assertNotIn("sk-or-v1-abcdef", all_text)
        self.assertIn("[REDACTED:openrouter_key]", all_text)
        # projcache-provided fields
        self.assertEqual(session.title, "Repo orientation")
        self.assertIsNotNone(session.usage)
        self.assertEqual(session.usage.total, 1690)

    def test_cli_fallback_backend(self):
        exe = shutil.which("zstd")
        if not exe:
            self.skipTest("zstd CLI not installed")
        root = helpers.build_dsh_store(self._tmp.name)
        parsed, report, _ = _scan(root, "transcript", self._tmp.name,
                                  zstd_backend="cli", zstd_bin=exe)
        self.assertEqual(report.errors, [])
        self.assertEqual(len(parsed), 1)

    def test_bad_frame_is_file_error_and_checkpoint_not_advanced(self):
        bad = (helpers.FIXTURES / "dsh" / "raw" / "bad-frame.zstd").read_bytes()
        root = helpers.build_dsh_store(self._tmp.name, raw_bytes=bad,
                                       with_projcache=False)
        parsed, report, ckpt = _scan(root, "transcript", self._tmp.name)
        self.assertEqual(parsed, [])
        self.assertEqual(len(report.errors), 1)
        self.assertIn("zstd", report.errors[0].reason)
        self.assertEqual(ckpt.data["stores"], {})

    def test_unknown_format_version_hard_stops_store(self):
        root = helpers.build_dsh_store(
            self._tmp.name, session_file="session-v99.jsonl", with_projcache=False
        )
        with self.assertRaises(scanutil.StoreHardError):
            _scan(root, "transcript", self._tmp.name)


class DshMetadataTierTest(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)

    def test_complete_projcache_avoids_decompress(self):
        root = helpers.build_dsh_store(self._tmp.name)
        original = scan_dsh.decompress_zstd

        def boom(*args, **kwargs):
            raise AssertionError("decompress must not be called in metadata tier")

        scan_dsh.decompress_zstd = boom
        try:
            parsed, report, _ = _scan(root, "metadata", self._tmp.name)
        finally:
            scan_dsh.decompress_zstd = original
        self.assertEqual(report.errors, [])
        session = parsed[0].session
        self.assertEqual(session.message_count, 4)
        self.assertEqual(session.title, "Repo orientation")
        self.assertEqual(session.first_prompt, "what does this repo do?")
        self.assertEqual(session.usage.total, 1690)
        self.assertEqual(session.cwd, "/Users/test/dshproj")
        self.assertEqual(session.repo_path, "/Users/test/dshproj")
        self.assertEqual(session.messages, [])

    @unittest.skipUnless(helpers.zstd_available(), "no zstd backend on this host")
    def test_missing_projcache_falls_back_to_header(self):
        root = helpers.build_dsh_store(self._tmp.name, with_projcache=False)
        parsed, report, _ = _scan(root, "metadata", self._tmp.name)
        self.assertEqual(report.errors, [])
        session = parsed[0].session
        self.assertEqual(session.cwd, "/Users/test/dshproj")
        self.assertEqual(session.message_count, 4)
        self.assertIsNone(session.title)
        self.assertEqual(session.messages, [])

    def test_denylisted_files_never_opened(self):
        root = helpers.build_dsh_store(self._tmp.name)
        # bait next to the session dir
        with open(os.path.join(root, ".credentials.yaml"), "w") as fh:
            fh.write("key: fake\n")
        with helpers.OpenSpy() as spy:
            _scan(root, "metadata", self._tmp.name)
        self.assertNotIn(".credentials.yaml", spy.basenames())


if __name__ == "__main__":
    unittest.main()
