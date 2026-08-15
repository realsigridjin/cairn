import json
import os
import unittest

import helpers
from cairn_sessions import emit
from cairn_sessions.model import NormalizedSession, SurfaceMessage
from cairn_sessions.scanutil import ParsedFile


def make_session(n_messages=0, uid="senpi:aaaa", native="aaaa"):
    session = NormalizedSession(
        session_uid=uid,
        source=uid.split(":", 1)[0],
        native_id=native,
        root_session_id=native,
        cwd="/tmp/proj",
        model="m",
        provider="p",
        title="title",
        first_prompt="prompt",
        created_at_ms=1000,
        updated_at_ms=2000,
        message_count=n_messages,
        store_path="/tmp/store/file.jsonl",
        store_format="jsonl",
    )
    session.messages = [
        SurfaceMessage(seq=i, role="user" if i % 2 == 0 else "assistant",
                       ts_ms=1500 + i, text=f"msg {i}")
        for i in range(n_messages)
    ]
    return session


def make_parsed(session, *, mtime_ns=100, reused=0):
    return ParsedFile(
        store=session.source,
        path=session.store_path,
        mtime_ns=mtime_ns,
        size=10,
        session_uid=session.session_uid,
        session=session,
        skipped_unchanged=False,
        reused_full_windows=reused,
        updated_at_ms=session.updated_at_ms,
    )


class ChunkShapeTest(unittest.TestCase):
    def test_deterministic_chunk_ids(self):
        session = make_session(n_messages=10)
        meta = emit.chunk_meta(session, "2026-08-01T00:00:02.000Z")
        self.assertEqual(meta["id"], "s:senpi:aaaa:meta")
        windows = list(emit.group_windows(session.messages))
        self.assertEqual([w[0] for w in windows], [0, 1])
        first = emit.chunk_window(session, 0, windows[0][1], None)
        second = emit.chunk_window(session, 1, windows[1][1], None)
        # fixed 8-message boundaries: the tail window keeps the 8-15 id
        self.assertEqual(first["id"], "s:senpi:aaaa:m:0-7")
        self.assertEqual(second["id"], "s:senpi:aaaa:m:8-15")
        self.assertEqual(len(windows[0][1]), 8)
        self.assertEqual(len(windows[1][1]), 2)
        self.assertEqual(second["metadata"]["seq_start"], 8)
        self.assertEqual(second["metadata"]["seq_end"], 15)

    def test_meta_chunk_text_and_metadata(self):
        session = make_session(n_messages=1)
        chunk = emit.chunk_meta(session, "2026-08-01T00:00:02.000Z")
        self.assertIn("session: title", chunk["text"])
        self.assertIn("first_prompt: prompt", chunk["text"])
        meta = chunk["metadata"]
        self.assertEqual(meta["doc_type"], "session_meta")
        self.assertEqual(meta["source"], "senpi")
        self.assertEqual(meta["session_uid"], "senpi:aaaa")
        self.assertEqual(meta["import_batch"], "2026-08-01T00:00:02.000Z")
        self.assertEqual(meta["importer_version"], 1)
        self.assertLessEqual(len(meta), 64)

    def test_window_chunk_text(self):
        session = make_session(n_messages=2)
        index, msgs = list(emit.group_windows(session.messages))[0]
        chunk = emit.chunk_window(session, index, msgs, None)
        self.assertEqual(chunk["text"], "[user] msg 0\n[assistant] msg 1")
        self.assertEqual(chunk["metadata"]["doc_type"], "session_messages")


class CorpusBuildTest(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)

    def _build(self, parsed_files, prev=None):
        warnings = []
        lines = emit.build_corpus(parsed_files, prev or helpers.empty_prev(), warnings)
        return lines, warnings

    def test_byte_identical_rebuild(self):
        first, _ = self._build([make_parsed(make_session(10))])
        second, _ = self._build([make_parsed(make_session(10))])
        self.assertEqual(first, second)

    def test_corpus_sorted_on_write(self):
        out = os.path.join(self._tmp.name, "canon.jsonl")
        lines, _ = self._build([make_parsed(make_session(20))])
        emit.write_corpus(lines, out)
        with open(out, encoding="utf-8") as fh:
            ids = [json.loads(line)["id"] for line in fh if line.strip()]
        self.assertEqual(ids, sorted(ids))
        self.assertEqual(len(ids), len(set(ids)))  # no duplicate ids ever

    def test_vector_carry_forward(self):
        out = os.path.join(self._tmp.name, "canon.jsonl")
        lines, _ = self._build([make_parsed(make_session(10))])
        # simulate `cairn embed`: attach vectors and rewrite the corpus
        embedded = {}
        for cid, raw in lines.items():
            chunk = json.loads(raw)
            chunk["vector"] = [0.1, 0.2, 0.3, 0.4]
            embedded[cid] = json.dumps(chunk, ensure_ascii=False, sort_keys=True,
                                       separators=(",", ":"))
        emit.write_corpus(embedded, out)
        prev = emit.PrevCorpusIndex.load(out)
        # unchanged rebuild: vectors carried forward
        carried, _ = self._build([make_parsed(make_session(10))], prev=prev)
        for cid, raw in carried.items():
            self.assertIn('"vector"', raw, cid)
        # changed message text: that chunk's vector is dropped, others kept
        changed = make_session(10)
        changed.messages[3].text = "msg 3 edited"
        rebuilt, _ = self._build([make_parsed(changed)], prev=prev)
        self.assertNotIn('"vector"', rebuilt["s:senpi:aaaa:m:0-7"])
        self.assertIn('"vector"', rebuilt["s:senpi:aaaa:m:8-15"])
        self.assertIn('"vector"', rebuilt["s:senpi:aaaa:meta"])

    def test_collision_last_mtime_wins(self):
        older = make_session(2)
        older.store_path = "/tmp/older.jsonl"
        newer = make_session(2)
        newer.store_path = "/tmp/newer.jsonl"
        newer.title = "newer title"
        lines, warnings = self._build(
            [make_parsed(older, mtime_ns=100), make_parsed(newer, mtime_ns=200)]
        )
        self.assertEqual(len(warnings), 1)
        self.assertIn("duplicate session_uid", warnings[0])
        meta = lines["s:senpi:aaaa:meta"]
        self.assertIn("newer title", meta)
        self.assertEqual(len([cid for cid in lines if cid.endswith(":meta")]), 1)

    def test_skipped_unchanged_reuses_prev_lines(self):
        out = os.path.join(self._tmp.name, "canon.jsonl")
        lines, _ = self._build([make_parsed(make_session(10))])
        emit.write_corpus(lines, out)
        prev = emit.PrevCorpusIndex.load(out)
        skipped = ParsedFile(
            store="senpi", path="/tmp/store/file.jsonl", mtime_ns=100, size=10,
            session_uid="senpi:aaaa", session=None, skipped_unchanged=True,
            reused_full_windows=0, updated_at_ms=2000,
        )
        reused, _ = self._build([skipped], prev=prev)
        self.assertEqual(reused, lines)

    def test_incremental_reuse_of_full_windows(self):
        out = os.path.join(self._tmp.name, "canon.jsonl")
        base = make_session(10)
        lines, _ = self._build([make_parsed(base)])
        emit.write_corpus(lines, out)
        prev = emit.PrevCorpusIndex.load(out)
        # grew to 13 messages: full window 0-7 reused verbatim from prev corpus
        grown = make_session(13)
        grown.messages = grown.messages[8:]  # tail(2) + new(3), seqs 8..12
        pf = make_parsed(grown, reused=1)
        lines2, _ = self._build([pf], prev=prev)
        self.assertEqual(lines2["s:senpi:aaaa:m:0-7"], lines["s:senpi:aaaa:m:0-7"])
        self.assertIn('"seq_start":8', lines2["s:senpi:aaaa:m:8-15"])
        self.assertNotIn("s:senpi:aaaa:m:16-23", lines2)


if __name__ == "__main__":
    unittest.main()
