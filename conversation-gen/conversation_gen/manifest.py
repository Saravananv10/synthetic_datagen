"""manifest.py — append-only ledger + freeze, for the conversation dataset.

Same pattern as datagen/dataset_manifest.py (append-only JSONL +
content-addressed freeze snapshot), with its own schema — a conversation
record doesn't have a verdict/verification concept, so this isn't
datagen.dataset_manifest.ManifestEntry reused, just the same mechanics.
"""

from __future__ import annotations

import hashlib
import json
from dataclasses import asdict, dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

LOOP_ROOT = Path(__file__).resolve().parents[2]
MANIFEST_ROOT = LOOP_ROOT / "artifacts" / "conversation_dataset"
ENTRIES_PATH = MANIFEST_ROOT / "entries.jsonl"
SNAPSHOTS_DIR = MANIFEST_ROOT / "snapshots"
TRACES_DIR = MANIFEST_ROOT / "traces"


def compute_topic_hash(topic: str) -> str:
    return hashlib.sha256(topic.encode("utf-8")).hexdigest()


@dataclass
class ConversationManifestEntry:
    conversation_id: str
    topic_hash: str
    personas: str  # "Farmer(qwen3-8-27b) vs Agronomy Expert(gpt)"
    round_count: int
    total_turns: int
    status: str  # "completed" | "error"
    recorded_at: str = field(default_factory=lambda: datetime.now(timezone.utc).isoformat())

    def to_dict(self) -> dict[str, Any]:
        return asdict(self)


def entry_from_trace(trace: dict[str, Any], *, round_count: int) -> ConversationManifestEntry:
    personas = " vs ".join(f"{p['name']}({p['model']})" for p in trace.get("personas", []))
    return ConversationManifestEntry(
        conversation_id=trace["conversation_id"],
        topic_hash=compute_topic_hash(trace.get("topic", "")),
        personas=personas,
        round_count=round_count,
        total_turns=len(trace.get("transcript", [])),
        status=trace.get("status", "unknown"),
    )


def append_entry(entry: ConversationManifestEntry, *, path: Path = ENTRIES_PATH) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a", encoding="utf-8") as fh:
        fh.write(json.dumps(entry.to_dict(), ensure_ascii=False) + "\n")


def load_entries(*, path: Path = ENTRIES_PATH) -> list[dict[str, Any]]:
    if not path.is_file():
        return []
    entries: list[dict[str, Any]] = []
    with path.open("r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            try:
                entries.append(json.loads(line))
            except json.JSONDecodeError:
                continue
    return entries


def _dataset_id(entries: list[dict[str, Any]]) -> str:
    canonical = json.dumps(entries, sort_keys=True, ensure_ascii=False)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def freeze_dataset(
    *,
    entries_path: Path = ENTRIES_PATH,
    snapshots_dir: Path = SNAPSHOTS_DIR,
) -> dict[str, Any]:
    """Cut an immutable, content-addressed snapshot from the current
    manifest entries. Same input entries always produce the same dataset_id."""
    entries = load_entries(path=entries_path)
    entries_sorted = sorted(entries, key=lambda e: e.get("conversation_id", ""))
    dataset_id = _dataset_id(entries_sorted)

    snapshot = {
        "dataset_id": dataset_id,
        "created_at": datetime.now(timezone.utc).isoformat(),
        "entry_count": len(entries_sorted),
        "completed_count": sum(1 for e in entries_sorted if e.get("status") == "completed"),
        "entries": entries_sorted,
    }

    snapshots_dir.mkdir(parents=True, exist_ok=True)
    snapshot_path = snapshots_dir / f"{dataset_id}.json"
    snapshot_path.write_text(json.dumps(snapshot, indent=2, ensure_ascii=False), encoding="utf-8")
    snapshot["_path"] = str(snapshot_path)
    return snapshot
