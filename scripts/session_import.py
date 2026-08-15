#!/usr/bin/env python3
"""CAIRN session-continuity importer.

Scans local agent session stores (DeepSeek Harness, Senpi/pi, Codex,
Claude Code), normalizes sessions, redacts secrets, and emits a canonical
chunk JSONL corpus suitable for ``cairn ingest`` into a dedicated KB.

Stdlib only. Metadata tier by default; transcript tier is opt-in.
Credential/settings/env files are never opened.

Exit codes: 0 = clean, 2 = partial (one or more stores/files failed),
1 = fatal usage/environment error.
"""

from __future__ import annotations

import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from cairn_sessions import IMPORTER_VERSION, checkpoint as checkpoint_mod  # noqa: E402
from cairn_sessions import emit, scan_claude, scan_codex, scan_dsh, scan_senpi, scanutil  # noqa: E402

DEFAULT_OUT = "~/.cairn/session-import/canonical.jsonl"
DEFAULT_CHECKPOINT = "~/.cairn/session-import/checkpoint.json"

EXIT_OK = 0
EXIT_FATAL = 1
EXIT_PARTIAL = 2


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="session_import.py",
        description=(
            "Import local agent session stores (dsh, senpi/pi, codex, claude-code) "
            "into a canonical CAIRN chunk JSONL corpus. Read-only against the "
            "stores; credential/settings/env files are never opened."
        ),
        epilog=(
            "typical flow: python3 scripts/session_import.py --tier transcript && "
            "cairn embed ~/.cairn/session-import/canonical.jsonl -o embedded.jsonl && "
            "cairn ingest embedded.jsonl --tenant local --kb sessions"
        ),
    )
    parser.add_argument(
        "--tier",
        choices=list(emit.TIERS),
        default=emit.TIER_METADATA,
        help="import tier: metadata (default) or transcript (opt-in; adds "
        "redacted surface message windows)",
    )
    parser.add_argument(
        "--stores",
        default="senpi,codex,claude,dsh",
        help="comma-separated subset of stores to scan "
        "(senpi,codex,claude,dsh; default: all present)",
    )
    parser.add_argument("--senpi-root", default="~/.senpi/agent/sessions",
                        help="senpi sessions root (also scans pi format)")
    parser.add_argument("--pi-root", default="~/.pi/agent/sessions",
                        help="pi sessions root (same format as senpi)")
    parser.add_argument("--codex-root", default="~/.codex/sessions",
                        help="codex rollout sessions root")
    parser.add_argument("--claude-root", default="~/.claude/projects",
                        help="claude code projects root")
    parser.add_argument("--dsh-root", default="~/.dsh/sessions",
                        help="DeepSeek Harness sessions root")
    parser.add_argument(
        "--tasks-root",
        action="append",
        default=None,
        help="senpi-task root (.omo/senpi-task) for lineage; repeatable. "
        "Defaults to ./.omo/senpi-task when present.",
    )
    parser.add_argument("--out", default=DEFAULT_OUT,
                        help=f"canonical corpus output path (default: {DEFAULT_OUT})")
    parser.add_argument("--checkpoint", default=DEFAULT_CHECKPOINT,
                        help=f"checkpoint path (default: {DEFAULT_CHECKPOINT})")
    parser.add_argument(
        "--no-carry-vectors",
        action="store_true",
        help="do not carry embedding vectors forward from a previous corpus",
    )
    parser.add_argument(
        "--delta",
        action="store_true",
        help="delta-publish mode (NOT IMPLEMENTED: tombstone emission pending; "
        "use snapshot ingest instead)",
    )
    parser.add_argument("--json", action="store_true",
                        help="print the import report as JSON")
    parser.add_argument("--zstd-backend", choices=["auto", "stdlib", "cli"],
                        default="auto", help=argparse.SUPPRESS)
    parser.add_argument("--version", action="version",
                        version=f"cairn session importer v{IMPORTER_VERSION}")
    return parser


def _default_tasks_roots(args) -> list[str]:
    if args.tasks_root is not None:
        return [os.path.abspath(os.path.expanduser(p)) for p in args.tasks_root]
    candidate = os.path.join(os.getcwd(), ".omo", "senpi-task")
    return [candidate] if os.path.isdir(candidate) else []


def run_import(args) -> tuple[int, dict]:
    if args.delta:
        print(
            "error: --delta is not implemented (tombstone emission pending); "
            "run a snapshot import instead",
            file=sys.stderr,
        )
        return EXIT_FATAL, {"error": "delta_not_implemented"}

    out_path = os.path.abspath(os.path.expanduser(args.out))
    ckpt_path = os.path.abspath(os.path.expanduser(args.checkpoint))
    prev = emit.PrevCorpusIndex.load(None if args.no_carry_vectors else out_path)
    ckpt = checkpoint_mod.Checkpoint.load(ckpt_path)

    selected = {s.strip() for s in args.stores.split(",") if s.strip()}
    tasks_roots = _default_tasks_roots(args)

    plans = []
    if "senpi" in selected:
        plans.append(("senpi", args.senpi_root, scan_senpi.scan,
                      {"tasks_roots": tasks_roots}))
        plans.append(("pi", args.pi_root, scan_senpi.scan, {"tasks_roots": []}))
    if "codex" in selected:
        plans.append(("codex", args.codex_root, scan_codex.scan, {}))
    if "claude" in selected:
        plans.append(("claude", args.claude_root, scan_claude.scan, {}))
    if "dsh" in selected:
        plans.append(("dsh", args.dsh_root, scan_dsh.scan,
                      {"zstd_backend": args.zstd_backend}))

    store_reports: list[scanutil.StoreReport] = []
    parsed_files = []
    for store_name, root_raw, scan_fn, kwargs in plans:
        root = os.path.abspath(os.path.expanduser(root_raw))
        report = scanutil.StoreReport(store=store_name, root=root)
        store_reports.append(report)
        if not os.path.isdir(root):
            report.status = "absent"
            continue
        try:
            parsed_files.extend(
                scan_fn(
                    root,
                    tier=args.tier,
                    checkpoint=ckpt,
                    prev_index=prev,
                    report=report,
                    **kwargs,
                )
            )
        except scanutil.StoreHardError as exc:
            report.status = "error"
            report.errors.append(scanutil.FileError(store_name, root, str(exc)))

    warnings: list[str] = []
    lines = emit.build_corpus(parsed_files, prev, warnings)
    try:
        chunk_count = emit.write_corpus(lines, out_path)
    except OSError as exc:
        print(f"error: cannot write corpus to {out_path}: {exc}", file=sys.stderr)
        return EXIT_FATAL, {"error": str(exc)}
    # Checkpoint is written strictly after the corpus write succeeds, so a
    # crashed run re-imports rather than skips.
    ckpt.save()

    has_errors = any(r.errors or r.status == "error" for r in store_reports)
    exit_code = EXIT_PARTIAL if has_errors else EXIT_OK
    report = {
        "tier": args.tier,
        "out": out_path,
        "checkpoint": ckpt_path,
        "chunks_written": chunk_count,
        "importer_version": IMPORTER_VERSION,
        "stores": [r.to_dict() for r in store_reports],
        "warnings": warnings,
        "exit_code": exit_code,
    }
    return exit_code, report


def _print_human(report: dict) -> None:
    print("== cairn session import ==")
    print(f"tier: {report['tier']}")
    for store in report["stores"]:
        line = (
            f"  {store['store']:<8} status={store['status']:<7} "
            f"seen={store['files_seen']} imported={store['files_imported']} "
            f"unchanged={store['files_skipped_unchanged']} "
            f"errors={len(store['errors'])}"
        )
        print(line)
    print(f"chunks: {report['chunks_written']} -> {report['out']}")
    for warning in report["warnings"]:
        print(f"warning: {warning}")
    for store in report["stores"]:
        for err in store["errors"]:
            print(f"error: [{err['store']}] {err['path']}: {err['reason']}")
    print(f"exit: {report['exit_code']}")


def main(argv=None) -> int:
    args = build_parser().parse_args(argv)
    exit_code, report = run_import(args)
    if report.get("stores"):
        if args.json:
            print(json.dumps(report, ensure_ascii=False, indent=2, sort_keys=True))
        else:
            _print_human(report)
    return exit_code


if __name__ == "__main__":
    sys.exit(main())
