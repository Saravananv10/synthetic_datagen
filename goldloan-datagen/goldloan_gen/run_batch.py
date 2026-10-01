"""End-to-end batch: generate scenarios -> run the simulator -> verify -> manifest (-> freeze).

    python -m goldloan_gen.run_batch --n 200 --seed 1 --mode scripted --out artifacts/goldloan_dataset
    python -m goldloan_gen.run_batch --n 40 --mode llm --llm-personas --workers 4 ...   # gpt for agents, human, vision and personas

Re-running is safe: the simulator skips scenarios whose trace already completed,
and the manifest is rebuilt from what is on disk.
"""
from __future__ import annotations

import argparse
import collections
import hashlib
import json
import subprocess
import sys
from datetime import datetime, timezone
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

from .persona_llm import enrich
from .scenarios import generate
from .verify import verify

ROOT = Path(__file__).resolve().parents[2]
DEFAULT_BIN = ROOT / "target" / "debug" / "loop-goldloan-gen"


def run_simulator(bin_path: Path, files: list[Path], out: Path, args) -> int:
    cmd = [str(bin_path), "--scenario", *map(str, files), "--out", str(out), "--mode", args.mode,
           "--max-turns", str(args.max_turns)]
    if args.mode == "llm":
        cmd += ["--agent-model", args.agent_model, "--human-model", args.human_model, "--vision-model", args.vision_model]
        if args.scripted_human:
            cmd.append("--scripted-human")
    if args.force:
        cmd.append("--force")
    if args.langfuse:
        cmd += ["--langfuse", "--run-id", args.run_id]
    return subprocess.run(cmd, cwd=ROOT).returncode


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=100)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out", type=Path, default=ROOT / "artifacts" / "goldloan_dataset")
    ap.add_argument("--mode", choices=["scripted", "llm"], default="scripted")
    ap.add_argument("--agent-model", default="soket/gpt")
    ap.add_argument("--human-model", default="soket/gpt")
    ap.add_argument("--vision-model", default="soket/gpt")
    ap.add_argument("--llm-personas", action="store_true", help="have GPT write each scenario's persona/name/address/employer")
    ap.add_argument("--persona-model", default="gpt")
    ap.add_argument("--scripted-human", action="store_true")
    ap.add_argument("--workers", type=int, default=1)
    ap.add_argument("--max-turns", type=int, default=150)
    ap.add_argument("--bin", type=Path, default=DEFAULT_BIN)
    ap.add_argument("--force", action="store_true")
    ap.add_argument("--langfuse", action="store_true", help="export every model call to Langfuse (goldloan-datagen/.env)")
    ap.add_argument("--run-id", default=None, help="Langfuse session-id suffix shared by all workers (default: current UTC time)")
    ap.add_argument("--freeze", action="store_true", help="write an immutable snapshot of the passing traces")
    args = ap.parse_args()
    # Resolve to absolute paths before they're ever used: run_simulator() invokes the Rust
    # binary with cwd=ROOT (see below), so a relative --out (or --bin) given relative to
    # wherever this script happened to be launched from would silently resolve against the
    # WRONG directory inside that subprocess. A live batch hit exactly this: "--out
    # ../artifacts/x" from inside goldloan-datagen/ landed one level *above* the repo when
    # the subprocess interpreted the same relative string against ROOT, and every worker
    # failed instantly with "No such file or directory" trying to load its --scenario files.
    args.out = args.out.resolve()
    # One id for the whole batch (every worker process), so a rerun of the same seed gets
    # fresh Langfuse traces instead of appending to the previous run's, and a single run
    # can be filtered by its `run:<id>` tag.
    args.run_id = args.run_id or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S")
    if args.langfuse:
        print(f"langfuse run id: {args.run_id}")
    args.bin = args.bin.resolve()

    sc_dir, tr_dir, rp_dir = args.out / "scenarios", args.out / "traces", args.out / "reports"
    for d in (sc_dir, tr_dir, rp_dir):
        d.mkdir(parents=True, exist_ok=True)

    # 1. scenarios (deterministic in --n/--seed; existing files are kept)
    written = 0
    fresh = [sc for sc in generate(args.n, args.seed) if not (sc_dir / f"{sc['id']}.json").exists()]
    if args.llm_personas and fresh:
        n = enrich(fresh, model=args.persona_model, workers=max(2, args.workers), cache=args.out / "persona_cache")
        print(f"llm personas: {n}/{len(fresh)} enriched")
    for sc in fresh:
        (sc_dir / f"{sc['id']}.json").write_text(json.dumps(sc, indent=2))
        written += 1
    files = sorted(sc_dir.glob("*.json"))
    print(f"scenarios: {len(files)} ({written} new)")

    # 2. simulate
    if not args.bin.exists():
        sys.exit(f"simulator binary not found: {args.bin} (cargo build -p loop-goldloan-gen)")
    chunks = [files[i::max(1, args.workers)] for i in range(max(1, args.workers))]
    with ThreadPoolExecutor(max_workers=max(1, args.workers)) as ex:
        codes = list(ex.map(lambda c: run_simulator(args.bin, c, tr_dir, args) if c else 0, chunks))
    if any(codes):
        print(f"warning: simulator exit codes {codes}", file=sys.stderr)

    # 3. verify + manifest
    viol = collections.Counter()
    outcomes, reached, coverage = collections.Counter(), collections.Counter(), collections.Counter()
    tags_ok, tags_all = collections.Counter(), collections.Counter()
    entries, passing = [], []
    for p in sorted(tr_dir.glob("*.json")):
        trace = json.loads(p.read_text())
        rep = verify(trace)
        (rp_dir / p.name).write_text(json.dumps(rep.to_json(), indent=2))
        for v in rep.violations:
            if v.severity == "error":
                viol[v.code] += 1
        outcomes[trace["outcome"]] += 1
        reached[max(t["stage"] for t in trace["conversation"])] += 1
        coverage.update(set(rep.coverage))
        for tag in trace["scenario"].get("tags", []):
            tags_all[tag] += 1
            tags_ok[tag] += rep.ok
        if rep.ok:
            passing.append(p.stem)
        entries.append({"id": p.stem, "mode": trace["mode"], "outcome": trace["outcome"], "turns": len(trace["conversation"]),
                        "ok": rep.ok, "violations": sorted({v.code for v in rep.violations if v.severity == "error"}),
                        "tags": trace["scenario"].get("tags", []),
                        "trace_sha": hashlib.sha256(p.read_bytes()).hexdigest()[:16]})
    (args.out / "entries.jsonl").write_text("".join(json.dumps(e) + "\n" for e in entries))

    n = len(entries)
    print(f"\ntraces: {n}   verified ok: {len(passing)} ({100 * len(passing) / max(1, n):.0f}%)")
    print("outcomes:", dict(outcomes))
    print("deepest stage reached:", dict(sorted(reached.items())))
    print("violations (errors):", dict(viol) or "none")
    print(f"tool error codes exercised: {len(coverage)} distinct")
    weak = {t: f"{tags_ok[t]}/{tags_all[t]}" for t in tags_all if tags_ok[t] < tags_all[t]}
    if weak:
        print("variants with failures:", weak)

    if args.freeze:
        body = json.dumps(sorted(passing)).encode()
        did = hashlib.sha256(body + b"".join((tr_dir / f"{i}.json").read_bytes() for i in sorted(passing))).hexdigest()[:16]
        snap = args.out / "snapshots"
        snap.mkdir(exist_ok=True)
        (snap / f"{did}.json").write_text(json.dumps({"dataset_id": did, "mode": args.mode, "n_traces": len(passing), "trace_ids": sorted(passing)}, indent=2))
        print(f"frozen snapshot {did} ({len(passing)} traces)")


if __name__ == "__main__":
    main()
