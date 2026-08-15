"""CAIRN session-continuity importer.

Sidecar scanner/normalizer that turns local agent session stores
(DeepSeek Harness, Senpi/pi, Codex, Claude Code) into a canonical
chunk JSONL suitable for ``cairn ingest``.

Design contract (advisory architecture, task st_01a005c1):
  * Python stdlib only; runs standalone without the UQA checkout.
  * Two import tiers: metadata (default) and transcript (opt-in).
  * Credential/settings/env files are never opened (see scanutil denylist).
  * Deterministic session/chunk ids; idempotent reruns; incremental
    append handling via a local checkpoint file.
"""

__version__ = "1"

IMPORTER_VERSION = 1
