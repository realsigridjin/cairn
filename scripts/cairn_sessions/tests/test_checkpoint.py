import os
import unittest

import helpers
from cairn_sessions import checkpoint as checkpoint_mod


class CheckpointTest(unittest.TestCase):
    def setUp(self):
        self._tmp = helpers.make_tmp()
        self.addCleanup(self._tmp.cleanup)
        self.path = os.path.join(self._tmp.name, "checkpoint.json")

    def test_roundtrip(self):
        ckpt = checkpoint_mod.Checkpoint.load(self.path)
        ckpt.set_entry("/abs/store/file.jsonl", {
            "size": 100, "mtime_ns": 5, "byte_offset": 100,
            "session_uid": "senpi:abc", "tier": "transcript",
            "msg_count": 9, "full_windows": 1,
            "tail": [{"seq": 8, "role": "user", "ts_ms": 1, "text": "hi"}],
            "updated_at_ms": 9, "first_prompt": "hi",
        })
        ckpt.save()
        loaded = checkpoint_mod.Checkpoint.load(self.path)
        entry = loaded.entry("/abs/store/file.jsonl")
        self.assertEqual(entry["session_uid"], "senpi:abc")
        self.assertEqual(entry["tail"][0]["seq"], 8)
        self.assertEqual(loaded.data["version"], 1)

    def test_missing_file_gives_empty(self):
        ckpt = checkpoint_mod.Checkpoint.load(self.path)
        self.assertEqual(ckpt.data["stores"], {})

    def test_corrupt_file_gives_empty(self):
        with open(self.path, "w", encoding="utf-8") as fh:
            fh.write("{not json")
        ckpt = checkpoint_mod.Checkpoint.load(self.path)
        self.assertEqual(ckpt.data["stores"], {})

    def test_version_mismatch_gives_empty(self):
        with open(self.path, "w", encoding="utf-8") as fh:
            fh.write('{"version": 999, "stores": {"x": {}}}')
        ckpt = checkpoint_mod.Checkpoint.load(self.path)
        self.assertEqual(ckpt.data["stores"], {})

    def test_save_is_atomic_and_creates_parent_dirs(self):
        nested = os.path.join(self._tmp.name, "a", "b", "checkpoint.json")
        ckpt = checkpoint_mod.Checkpoint(nested)
        ckpt.set_entry("/x", {"size": 1})
        ckpt.save()
        self.assertTrue(os.path.exists(nested))
        leftovers = [n for n in os.listdir(os.path.dirname(nested))
                     if n.startswith(".checkpoint")]
        self.assertEqual(leftovers, [])


if __name__ == "__main__":
    unittest.main()
