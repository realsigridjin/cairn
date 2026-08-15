import io
import json
import os
import shutil
import unittest
from contextlib import redirect_stderr, redirect_stdout

import helpers
from cairn_sessions import scanutil

import session_import

FIX = helpers.FIXTURES


def _copytree(src, dst):
    shutil.copytree(src, dst)
    return dst


class CliTestBase(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)
        self.out = os.path.join(self._tmp.name, "canon.jsonl")
        self.ckpt = os.path.join(self._tmp.name, "checkpoint.json")

    def argv(self, *extra):
        return [
            "--senpi-root", str(FIX / "senpi"),
            "--pi-root", str(FIX / "pi"),
            "--codex-root", str(FIX / "codex"),
            "--claude-root", str(FIX / "claude"),
            "--dsh-root", os.path.join(self._tmp.name, "no-dsh-store"),
            "--tasks-root", str(FIX / "senpi" / "senpi-task"),
            "--out", self.out,
            "--checkpoint", self.ckpt,
            *extra,
        ]

    def run_cli(self, *extra):
        buf = io.StringIO()
        with redirect_stdout(buf):
            code = session_import.main(self.argv(*extra))
        return code, buf.getvalue()


class CliPartialRunTest(CliTestBase):
    def test_partial_run_reports_errors_and_imports_the_rest(self):
        code, output = self.run_cli("--tier", "transcript")
        self.assertEqual(code, 2)  # malformed fixtures in senpi/codex/claude
        self.assertTrue(os.path.exists(self.out))
        with open(self.out, encoding="utf-8") as fh:
            chunks = [json.loads(line) for line in fh if line.strip()]
        sources = {c["metadata"]["source"] for c in chunks}
        self.assertEqual(sources, {"senpi", "pi", "codex", "claude-code"})
        # each malformed file produced exactly one per-file error
        self.assertIn("error:", output)
        self.assertIn("33333333", output)  # senpi truncated
        self.assertIn("cccccccc", output)  # codex corrupt line
        self.assertIn("eeeeeeee", output)  # claude truncated
        # absent dsh store is reported, not an error
        self.assertIn("dsh", output)

    def test_duplicate_session_collision_logged(self):
        code, output = self.run_cli("--tier", "metadata")
        self.assertIn("duplicate session_uid senpi:11111111", output)

    def test_delta_flag_refuses(self):
        buf = io.StringIO()
        with redirect_stdout(buf), redirect_stderr(io.StringIO()):
            code = session_import.main(self.argv("--delta"))
        self.assertEqual(code, 1)
        self.assertFalse(os.path.exists(self.out))

    def test_denylisted_files_never_opened(self):
        with helpers.OpenSpy() as spy:
            self.run_cli("--tier", "transcript")
        denied = {"auth.json", ".env", ".claude.json", "settings.json",
                  ".credentials.yaml"}
        self.assertTrue(denied.isdisjoint(spy.basenames()),
                        denied & spy.basenames())

    def test_no_raw_secrets_in_corpus(self):
        self.run_cli("--tier", "transcript")
        with open(self.out, encoding="utf-8") as fh:
            corpus = fh.read()
        for secret in (
            "sk-or-v1-abcdef1234567890abcdef1234567890abcdef12",
            "AKIAIOSFODNN7EXAMPLE",
            "ghp_abcdefghij1234567890abcd",
            "xoxb-123456789012-abcdefghijkl",
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "fake-senpi-credential",
            "fake-codex-credential",
            "fake-claude-credential",
            "fake-env-token",
        ):
            self.assertNotIn(secret, corpus)
        self.assertIn("[REDACTED:", corpus)


class CliCleanRunTest(CliTestBase):
    """A curated store set without malformed fixtures: exit 0, stable reruns."""

    def setUp(self):
        super().setUp()
        base = self._tmp.name
        self.senpi_root = os.path.join(base, "senpi")
        os.makedirs(self.senpi_root)
        _copytree(FIX / "senpi" / "sessions" / "--Users-test-myproj--",
                  os.path.join(self.senpi_root, "sessions", "--Users-test-myproj--"))
        # drop the truncated fixture for the clean run
        os.remove(os.path.join(
            self.senpi_root, "sessions", "--Users-test-myproj--",
            "2026-08-01T01-00-00-000Z_33333333-3333-4333-8333-333333333333.jsonl"))
        self.codex_root = os.path.join(base, "codex")
        _copytree(FIX / "codex" / "sessions", os.path.join(self.codex_root, "sessions"))
        os.remove(os.path.join(
            self.codex_root, "sessions", "2026", "08", "01",
            "rollout-2026-08-01T02-00-00-cccccccc-cccc-4ccc-8ccc-cccccccccccc.jsonl"))

    def argv(self, *extra):
        return [
            "--senpi-root", self.senpi_root,
            "--pi-root", os.path.join(self._tmp.name, "no-pi"),
            "--codex-root", self.codex_root,
            "--claude-root", os.path.join(self._tmp.name, "no-claude"),
            "--dsh-root", os.path.join(self._tmp.name, "no-dsh"),
            "--tasks-root", str(FIX / "senpi" / "senpi-task"),
            "--out", self.out,
            "--checkpoint", self.ckpt,
            *extra,
        ]

    def _corpus_bytes(self):
        with open(self.out, "rb") as fh:
            return fh.read()

    def test_clean_run_exit_zero_and_byte_identical_rerun(self):
        code1, out1 = self.run_cli("--tier", "transcript")
        self.assertEqual(code1, 0, out1)
        first = self._corpus_bytes()
        code2, out2 = self.run_cli("--tier", "transcript")
        self.assertEqual(code2, 0, out2)
        self.assertEqual(first, self._corpus_bytes())
        self.assertIn("unchanged=1", out2)

    def test_metadata_tier_emits_no_message_chunks(self):
        code, _ = self.run_cli("--tier", "metadata")
        self.assertEqual(code, 0)
        with open(self.out, encoding="utf-8") as fh:
            chunks = [json.loads(line) for line in fh if line.strip()]
        self.assertTrue(chunks)
        for chunk in chunks:
            self.assertEqual(chunk["metadata"]["doc_type"], "session_meta")
            self.assertNotIn(":m:", chunk["id"])

    def test_transcript_tier_emits_window_chunks(self):
        code, _ = self.run_cli("--tier", "transcript")
        self.assertEqual(code, 0)
        with open(self.out, encoding="utf-8") as fh:
            chunks = [json.loads(line) for line in fh if line.strip()]
        kinds = {c["metadata"]["doc_type"] for c in chunks}
        self.assertEqual(kinds, {"session_meta", "session_messages"})
        # every chunk carries the filterable lineage metadata
        for chunk in chunks:
            meta = chunk["metadata"]
            for key in ("source", "session_uid", "root_session_id", "parent_id",
                        "depth", "cwd", "created_at_ms", "updated_at_ms",
                        "store_path", "import_batch", "importer_version"):
                self.assertIn(key, meta)

    def test_json_report(self):
        code, output = self.run_cli("--tier", "metadata", "--json")
        self.assertEqual(code, 0)
        report = json.loads(output)
        self.assertEqual(report["exit_code"], 0)
        self.assertGreater(report["chunks_written"], 0)
        stores = {s["store"]: s for s in report["stores"]}
        self.assertEqual(stores["senpi"]["status"], "ok")
        self.assertEqual(stores["dsh"]["status"], "absent")

    def test_parser_defaults(self):
        args = session_import.build_parser().parse_args([])
        self.assertEqual(args.tier, "metadata")
        self.assertFalse(args.delta)


@unittest.skipUnless(helpers.zstd_available(), "no zstd backend on this host")
class CliDshStoreTest(CliTestBase):
    def argv_for(self, dsh_root, *extra):
        return [
            "--senpi-root", os.path.join(self._tmp.name, "none1"),
            "--pi-root", os.path.join(self._tmp.name, "none2"),
            "--codex-root", os.path.join(self._tmp.name, "none3"),
            "--claude-root", os.path.join(self._tmp.name, "none4"),
            "--dsh-root", dsh_root,
            "--out", self.out,
            "--checkpoint", self.ckpt,
            *extra,
        ]

    def run_dsh(self, dsh_root, *extra):
        buf = io.StringIO()
        with redirect_stdout(buf):
            code = session_import.main(self.argv_for(dsh_root, *extra))
        return code, buf.getvalue()

    def _corpus(self):
        with open(self.out, encoding="utf-8") as fh:
            return fh.read()

    def test_dsh_clean_store(self):
        root = helpers.build_dsh_store(self._tmp.name)
        code, output = self.run_dsh(root, "--tier", "transcript")
        self.assertEqual(code, 0, output)
        self.assertIn("dsh:session-99999999", self._corpus())

    def test_dsh_unknown_version_store_hard_stop(self):
        root = helpers.build_dsh_store(
            self._tmp.name, session_file="session-v99.jsonl", with_projcache=False
        )
        code, output = self.run_dsh(root, "--tier", "transcript")
        self.assertEqual(code, 2)
        self.assertIn("unsupported dsh session format version", output)
        self.assertNotIn("dsh:session-88888888", self._corpus())

    def test_dsh_bad_frame_partial(self):
        bad = (FIX / "dsh" / "raw" / "bad-frame.zstd").read_bytes()
        root = helpers.build_dsh_store(self._tmp.name, raw_bytes=bad,
                                       with_projcache=False)
        code, output = self.run_dsh(root, "--tier", "transcript")
        self.assertEqual(code, 2)
        self.assertIn("zstd", output)


if __name__ == "__main__":
    unittest.main()
