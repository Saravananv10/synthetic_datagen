"""Convert verified traces into SFT samples (OpenAI chat format): stage-agent
tool-calling samples, and vision/OCR extraction samples.

Stage samples: one per (conversation, stage). system = that stage's prompt; user
turns = what the stage agent actually received (upload turns carry the ingest
summary text, never raw documents — same as the demo); assistant turns carry
tool calls one at a time followed by the tool result, then the final reply
(with its `<<BUTTONS:...>>` marker restored).

Vision samples: one per uploaded document. system/user prompt mirror
`vision.rs`'s `VISION_SYSTEM` / extraction prompt exactly, so this trains
(or evals) the same OCR/vision role the simulator uses; assistant = the JSON
that was actually extracted (not the hidden truth — see `--vision-target`).

    python -m goldloan_gen.export_sft --traces artifacts/goldloan_dataset/traces \
        --reports artifacts/goldloan_dataset/reports --out artifacts/goldloan_dataset/sft.jsonl \
        --vision-out artifacts/goldloan_dataset/vision_sft.jsonl
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

# Mirrors crates/loop-goldloan-gen/src/vision.rs::VISION_SYSTEM exactly.
VISION_SYSTEM = (
    "You are the OCR and document-understanding component of a loan-origination pipeline for Indian KYC and "
    "gold-loan documents. You receive the raw text layer of ONE scanned document (it may contain layout noise, Devanagari boilerplate or OCR "
    "errors). Extract the requested fields and reply with ONE JSON object and nothing else - no prose, no markdown fences.\n"
    "Rules: transcribe exactly what is printed; never correct, guess, normalise or invent values; use null for a field that is absent or "
    "unreadable; numbers are JSON numbers without units; keep identifiers (PAN, Aadhaar, certificate numbers) exactly as printed."
)

# Mirrors crates/loop-goldloan-gen/src/docs.rs::schema_for exactly.
VISION_SCHEMA = {
    "aadhaar_front": "name, dob (DD/MM/YYYY), aadhaar_number (12 digits, spaces allowed)",
    "aadhaar_back": "address, pincode (6 digits), aadhaar_number (12 digits, spaces allowed)",
    "pan": "pan_number, name, father_name, dob (DD/MM/YYYY)",
    "address_proof": "document_type, name, address, pincode (6 digits)",
    "gold_photo": "ornament_type (one word, lowercase, e.g. necklace), scale_reading_g (number)",
    "gold_certificate": "certificate_no, customer_name, ornament_type (one word, lowercase), item_description, gross_weight_g (number), "
                        "stone_deduction_g (number), net_weight_g (number), purity_karat (number)",
    "payslip": "employee_name, employer, month (format YYYY-MM), net_pay (number, rupees)",
}


def vision_user_prompt(doc: dict) -> str:
    schema = VISION_SCHEMA.get(doc["doc"], "")
    return f"DOCUMENT TYPE: {doc['doc']}\nReturn a JSON object with exactly these keys: {schema}\n\n--- SCAN TEXT ---\n{doc['scan_text']}\n--- END ---"


def vision_to_samples(trace: dict, target: str = "extracted") -> list[dict]:
    """One sample per uploaded document. `target`: 'extracted' (what the vision
    model actually said - trains/evals it as-is, glitches included) or 'truth'
    (what was really printed - a cleaned-up gold-standard target instead)."""
    out = []
    for t in trace["conversation"]:
        ing = t.get("ingest")
        if not ing:
            continue
        for d in ing["docs"]:
            out.append({
                "id": f"{trace['scenario_id']}:t{t['turn']}:{d['doc']}",
                "messages": [
                    {"role": "system", "content": VISION_SYSTEM},
                    {"role": "user", "content": vision_user_prompt(d)},
                    {"role": "assistant", "content": json.dumps(d[target])},
                ],
                "meta": {"scenario_id": trace["scenario_id"], "doc": d["doc"], "fixture": d["fixture"],
                         "vision_model": ing["vision_model"], "target": target,
                         "matched_truth": d[target] == d["truth"]},
            })
    return out


def final_content(a: dict) -> str:
    text = a["final_output"]["text"]
    opts = a["final_output"]["structured_response"].get("options")
    return f"{text} <<BUTTONS:{'/'.join(opts)}>>" if opts else text


def trace_to_samples(trace: dict) -> list[dict]:
    by_stage: dict[int, list[dict]] = {}
    for t in trace["conversation"]:
        by_stage.setdefault(t["stage"], []).append(t)
    out = []
    for stage, turns in sorted(by_stage.items()):
        msgs = [{"role": "system", "content": trace["stage_prompts"][str(stage)]}]
        for t in turns:
            a = t["agents"][0]
            msgs.append({"role": "user", "content": a["input"]})
            for k, c in enumerate(a["tool_calls"]):
                cid = f"call_{t['turn']}_{k}"
                msgs.append({"role": "assistant", "content": None,
                             "tool_calls": [{"id": cid, "type": "function",
                                             "function": {"name": c["name"], "arguments": json.dumps(c["arguments"])}}]})
                msgs.append({"role": "tool", "tool_call_id": cid, "name": c["name"], "content": json.dumps(c["output"])})
            msgs.append({"role": "assistant", "content": final_content(a)})
        out.append({
            "id": f"{trace['scenario_id']}:stage{stage}",
            "messages": msgs,
            "tools": trace["tool_definitions"][str(stage)],
            "meta": {"scenario_id": trace["scenario_id"], "stage": stage, "mode": trace["mode"], "models": trace["models"],
                     "outcome": trace["outcome"], "tags": trace["scenario"].get("tags", [])},
        })
    return out


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--traces", type=Path, required=True)
    ap.add_argument("--reports", type=Path, help="only export traces whose report says ok")
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--vision-out", type=Path, help="also export vision/OCR document-extraction samples here")
    ap.add_argument("--vision-target", choices=["extracted", "truth"], default="extracted",
                    help="assistant target: what the vision model said (default), or the hidden ground truth")
    a = ap.parse_args()
    n_tr = n_s = n_v = 0
    vf = a.vision_out.open("w") if a.vision_out else None
    with a.out.open("w") as f:
        for p in sorted(a.traces.glob("*.json")):
            if a.reports:
                rp = a.reports / p.name
                if not rp.exists() or not json.loads(rp.read_text())["ok"]:
                    continue
            trace = json.loads(p.read_text())
            n_tr += 1
            for s in trace_to_samples(trace):
                f.write(json.dumps(s) + "\n")
                n_s += 1
            if vf:
                for s in vision_to_samples(trace, a.vision_target):
                    vf.write(json.dumps(s) + "\n")
                    n_v += 1
    if vf:
        vf.close()
    print(f"exported {n_s} stage samples from {n_tr} traces -> {a.out}")
    if a.vision_out:
        print(f"exported {n_v} vision samples -> {a.vision_out}")


if __name__ == "__main__":
    main()
