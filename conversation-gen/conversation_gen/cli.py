"""cli.py — batch runner for the conversation dataset generator.

Loads ConversationSpec files, skips ones already done (checkpoint) or marked
good (curation), invokes the loop-conversation-gen Rust binary per spec,
reads its trace JSON back, appends a manifest entry, and updates
checkpoint/curation state. Mirrors datagen/cli.py's shape, much simpler (no
phase-gating — a conversation either completes or errors).
"""

from __future__ import annotations

import argparse
import json
import subprocess
import sys
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]  # conversation-gen/
LOOP_ROOT = REPO_ROOT.parent
sys.path.insert(0, str(REPO_ROOT))

from conversation_gen.checkpoint import CheckpointStore  # noqa: E402
from conversation_gen.curation import CurationList  # noqa: E402

from conversation_gen import manifest  # noqa: E402
from conversation_gen.spec_gen import TASK_BANK_DIR  # noqa: E402

DEFAULT_BIN = LOOP_ROOT / "target" / "debug" / "loop-conversation-gen"
CHECKPOINT_PATH = LOOP_ROOT / "artifacts" / "conversation_checkpoint" / "checkpoint.json"
CURATION_PATH = LOOP_ROOT / "artifacts" / "conversation_curation" / "skip_done.json"


def load_spec_ids(bank_dir: Path) -> list[str]:
    return sorted(p.stem for p in bank_dir.glob("*.json"))


def run_one(spec_path: Path, *, binary: Path, out_dir: Path) -> dict:
    """Invoke the Rust binary for a single spec, return its trace dict."""
    result = subprocess.run(
        [str(binary), "--spec", str(spec_path), "--out", str(out_dir)],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(f"loop-conversation-gen exited {result.returncode}: {result.stderr[-2000:]}")
    conversation_id = spec_path.stem
    trace_path = out_dir / f"{conversation_id}.json"
    if not trace_path.is_file():
        raise RuntimeError(f"no trace written for {conversation_id} (stdout: {result.stdout[-1000:]})")
    return json.loads(trace_path.read_text(encoding="utf-8"))


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Run a bank of conversation specs")
    parser.add_argument("--bank", type=Path, default=TASK_BANK_DIR)
    parser.add_argument("--out", type=Path, default=LOOP_ROOT / "artifacts" / "conversation_dataset" / "traces")
    parser.add_argument("--binary", type=Path, default=DEFAULT_BIN)
    parser.add_argument("--dry-run", action="store_true", help="List specs that would run; no model calls")
    parser.add_argument("--force-rerun", action="store_true", help="Ignore checkpoint/curation skips")
    parser.add_argument("--freeze-dataset", action="store_true", help="Cut a snapshot after the batch")
    parser.add_argument("--limit", type=int, default=None, help="Run at most N specs (for a quick sample)")
    args = parser.parse_args(argv)

    if not args.bank.is_dir():
        print(f"No task bank at {args.bank} — run `python -m conversation_gen.spec_gen` first.")
        return 1

    spec_ids = load_spec_ids(args.bank)
    if not spec_ids:
        print(f"No specs found in {args.bank}.")
        return 0

    checkpoint = CheckpointStore(path=CHECKPOINT_PATH)
    curation = CurationList(path=CURATION_PATH)

    if args.dry_run:
        for spec_id in spec_ids:
            skip = (not args.force_rerun) and (checkpoint.is_done(spec_id) or curation.is_marked_good(spec_id))
            print(f"{'[skip]' if skip else '[run] '} {spec_id}")
        print(f"\n{len(spec_ids)} spec(s) total.")
        return 0

    if args.limit is not None:
        spec_ids = spec_ids[: args.limit]

    completed = 0
    errored = 0
    skipped = 0
    for spec_id in spec_ids:
        if not args.force_rerun and (checkpoint.is_done(spec_id) or curation.is_marked_good(spec_id)):
            skipped += 1
            continue

        spec_path = args.bank / f"{spec_id}.json"
        print(f"=== {spec_id} ===")
        checkpoint.mark_running(spec_id)
        try:
            trace = run_one(spec_path, binary=args.binary, out_dir=args.out)
        except Exception as exc:  # noqa: BLE001 — batch loop must not die on one bad spec
            print(f"  ERROR: {exc}")
            checkpoint.mark_failed(spec_id, detail=str(exc))
            errored += 1
            continue

        spec_json = json.loads(spec_path.read_text(encoding="utf-8"))
        entry = manifest.entry_from_trace(trace, round_count=spec_json["rounds_per_persona"])
        manifest.append_entry(entry)

        if trace.get("status") == "completed":
            checkpoint.mark_done(spec_id)
            completed += 1
            print(f"  -> completed ({entry.total_turns} turns)")
        else:
            checkpoint.mark_failed(spec_id, detail=trace.get("error", "unknown"))
            errored += 1
            print(f"  -> error: {trace.get('error')}")

    print(f"\n=== {completed} completed, {errored} errored, {skipped} skipped (of {len(spec_ids)}) ===")

    if args.freeze_dataset:
        snapshot = manifest.freeze_dataset()
        print(f"Froze dataset {snapshot['dataset_id']} ({snapshot['entry_count']} entries) -> {snapshot['_path']}")

    return 0


if __name__ == "__main__":
    raise SystemExit(main())
