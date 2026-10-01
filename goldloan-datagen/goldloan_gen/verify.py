"""Trace verifier.

Checks a trace (scripted or LLM) against the workflow spec, the business rules
and the scenario ground truth. Each violation carries a code so batch runs can
aggregate failure modes. Severity: `error` makes a trace unusable as training
data; `warn` is informational.
"""
from __future__ import annotations

import json
import re
from dataclasses import dataclass, field, asdict
from pathlib import Path

import jsonschema

from .rules import RULES, SPEC, max_loan
TOOLS = {t["name"]: t for t in json.loads((SPEC / "tools.json").read_text())["tools"]}
STAGES = {s["id"]: s for s in json.loads((SPEC / "stages.json").read_text())["stages"]}

# Errors that mean "you called this tool before its prerequisite" (not the demo's
# intended gates INCOME_REQUIRED / BUREAU_REQUIRED / ELIGIBILITY_REQUIRED).
PREMATURE = {
    "FIELD_NOT_STORED", "OTP_NOT_SENT", "IDENTITY_NOT_STORED", "AADHAAR_NOT_VERIFIED", "ADDRESS_NOT_CONFIRMED",
    "APPOINTMENT_NOT_SAVED", "NOMINEE_NOT_STORED", "NO_GOLD_ITEMS", "GOLD_NOT_VALUED", "PLEDGE_NOT_CHECKED",
    "GOLD_NOT_FINALIZED", "NO_OFFER_SELECTED", "PD_NOT_DONE", "GDR_NOT_ISSUED", "NOT_APPROVED", "KFS_NOT_ACCEPTED",
    "ESIGN_NOT_GENERATED", "ESIGN_NOT_DONE", "PACKET_NOT_ALLOCATED", "NOT_DISBURSED", "NO_IDENTITY_DOCUMENTS",
    "NO_GOLD_INGEST", "NO_PAYSLIPS", "EMPLOYMENT_NOT_CAPTURED", "OFFER_NOT_CONFIRMED", "NO_ADDRESS_PROOF",
    "MISSING_ARGUMENT", "INVALID_ACTION", "INVALID_FIELD", "INVALID_OTP_TYPE", "UNKNOWN_TOOL",
}

# Buttons each stage is allowed to show.
BUTTONS = {
    1: {"RESEND OTP", "EXIT", "PROCEED"},
    2: {"RESEND OTP", "EXIT", "PROCEED", "YES"},
    3: {"USE AADHAAR", "OVERRIDE", "EXIT", "PROCEED"},
    4: {"YES", "NO", "EXIT", "PROCEED"},
    5: {"CONFIRM", "CHANGE", "EXIT", "SALARIED", "SELF-EMPLOYED", "PROCEED"},
    6: {"YES", "EXIT", "CHECK APPROVAL", "ACCEPT", "DECLINE", "DONE", "PROCEED"},
}


# Phrases the agent may only say once the corresponding tool has succeeded (this turn or earlier).
CLAIMS = [
    (r"Identity documents stored", "collect_identity_documents"),
    (r"OTP sent successfully for Aadhaar", "generate_aadhar_otp"),
    (r"Aadhaar Card Exists|Aadhaar verified", "verify_aadhar_otp"),
    (r"verified and linked to Aadhaar", "store_numbers"),
    (r"All onboarding details (are )?collected", "get_collection_status"),
    (r"Address confirmed from", "confirm_address"),
    (r"Borrower dedupe (check is )?clear", "run_dedupe"),
    (r"Nominee dedupe (check is )?clear", "run_dedupe"),
    (r"Gold stage complete|Gold valuation is complete", "finalize_gold"),
    (r"Employment type salaried saved", "capture_income"),
    (r"CIBIL score is", "get_bureau_score"),
    (r"Eligibility confirmed", "check_loan_eligibility"),
    (r"The offer has been confirmed", "calculate_offer:confirm"),
    (r"^Approved\.|^The loan is approved", "check_approval_status"),
    (r"Please open this link to complete (your|the) e-signature", "generate_esign_url"),
    (r"Document signed successfully|The document is signed", "check_esign_status"),
    (r"sanction letter emailed", "send_sanction_letter_email"),
]


@dataclass
class Violation:
    code: str
    turn: int | None
    msg: str
    severity: str = "error"


@dataclass
class Report:
    scenario_id: str
    violations: list[Violation] = field(default_factory=list)
    coverage: list[str] = field(default_factory=list)

    @property
    def ok(self) -> bool:
        return not any(v.severity == "error" for v in self.violations)

    def add(self, code, turn, msg, severity="error"):
        self.violations.append(Violation(code, turn, msg, severity))

    def to_json(self):
        return {"scenario_id": self.scenario_id, "ok": self.ok,
                "violations": [asdict(v) for v in self.violations], "coverage": self.coverage}


def r2(x):
    return round(x * 100 + 1e-9) / 100


def expected_outcome(sc: dict) -> str:
    """What the scenario's ground truth implies the conversation should end in."""
    f, c = sc.get("flags", {}), sc
    events = []  # (stage, outcome)
    if f.get("exit_at_stage"):
        events.append((f["exit_at_stage"], "exited"))
    if f.get("pan_not_linked"):
        events.append((2, "rejected"))
    if f.get("borrower_duplicate"):
        events.append((3, "rejected"))
    if f.get("pledged"):
        events.append((4, "rejected"))
    if sc["income"]["employment_type"] != "salaried" and not RULES["income"]["self_employed_supported"]:
        # only reached when the loan exceeds the income threshold
        events.append((5, "handoff?"))
    if sc["bureau"]["cibil"] < RULES["bureau"]["min_score_for_loan"]:
        events.append((5, "rejected"))
    if f.get("approval_rejected"):
        events.append((6, "rejected"))
    if f.get("decline_kfs"):
        events.append((6, "declined"))
    if not events:
        return "completed"
    events.sort(key=lambda e: e[0])
    return events[0][1]


def _num_strings(v, out: set):
    """Collect every number appearing in a JSON value, as 2dp and plain strings."""
    if isinstance(v, bool):
        return
    if isinstance(v, (int, float)):
        out.add(f"{v:.2f}")
        out.add(f"{v:g}")
        out.add(str(v))
    elif isinstance(v, str):
        for m in re.findall(r"\d[\d,]*\.?\d*", v):
            m = m.replace(",", "").rstrip(".")
            out.add(m)
            try:
                out.add(f"{float(m):.2f}")
            except ValueError:
                pass
    elif isinstance(v, dict):
        for x in v.values():
            _num_strings(x, out)
    elif isinstance(v, list):
        for x in v:
            _num_strings(x, out)


# ---- vision / OCR extraction checks ------------------------------------------------

_MONTHS = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"]
_COMPARE = {  # doc kind -> {field: kind of comparison}
    "aadhaar_front": {"name": "text", "aadhaar_number": "digits", "dob": "digits"},
    "aadhaar_back": {"address": "address", "pincode": "digits", "aadhaar_number": "digits"},
    "pan": {"pan_number": "id", "name": "text", "dob": "digits"},
    "address_proof": {"name": "text", "address": "address", "pincode": "digits"},
    "gold_photo": {"ornament_type": "text", "scale_reading_g": "num"},
    "gold_certificate": {"ornament_type": "text", "gross_weight_g": "num", "stone_deduction_g": "num",
                         "net_weight_g": "num", "purity_karat": "num"},
    "payslip": {"employee_name": "text", "month": "month", "net_pay": "num"},
}


def _same(kind: str, a, b) -> bool:
    if b is None:
        return a is None
    if a is None:
        return False
    if kind == "num":
        try:
            fa = float(re.sub(r"[^\d.]", "", str(a)))
            return abs(fa - float(b)) < 0.006
        except ValueError:
            return False
    if kind == "digits":
        return re.sub(r"\D", "", str(a)) == re.sub(r"\D", "", str(b))
    if kind == "id":
        return re.sub(r"\s", "", str(a)).upper() == re.sub(r"\s", "", str(b)).upper()
    if kind == "address":
        # Our rendered scan text bakes the pincode into the address line with no separate
        # label (matching real Aadhaar layouts); a model that extracts pincode separately
        # and does not repeat it in `address` is not wrong, so a trailing pincode is
        # optional on both sides here. Everything else must still match.
        def strip_pin(x):
            return re.sub(r"[-,]?\s*\d{6}\s*$", "", str(x).strip())
        return _same("text", strip_pin(a), strip_pin(b))
    if kind == "month":
        def norm(x):
            x = str(x).lower()
            m = re.match(r"(\d{4})-(\d{1,2})", x)
            if m:
                return f"{m[1]}-{int(m[2]):02d}"
            y = re.findall(r"\d{4}", x)
            mi = next((i for i, n in enumerate(_MONTHS) if n in x), None)
            return f"{y[0]}-{mi + 1:02d}" if y and mi is not None else x
        return norm(a) == norm(b)
    return re.sub(r"[^a-z0-9]", "", str(a).lower()) == re.sub(r"[^a-z0-9]", "", str(b).lower())


def check_extraction(rep: "Report", turn: int, ing: dict) -> None:
    """Compare what the vision model extracted with what was actually printed."""
    status = (re.search(r"Status: (\w+)", ing["summary"]) or [None, ""])[1]
    accepted = status in ("PASS", "COMPLETE")
    for d in ing["docs"]:
        bad = [k for k, kind in _COMPARE.get(d["doc"], {}).items() if not _same(kind, d["extracted"].get(k), d["truth"].get(k))]
        if not bad:
            continue
        detail = {k: (d["truth"].get(k), d["extracted"].get(k)) for k in bad}
        if accepted:
            rep.add("EXTRACTION_ERROR_ACCEPTED", turn, f"{d['doc']}: wrong extraction passed the middleware: {detail}")
        else:
            rep.coverage.append("EXTRACTION_MISMATCH_CAUGHT")
            rep.add("VISION_EXTRACTION_ERROR", turn, f"{d['doc']}: {detail} (caught by validation)", "warn")


def verify(trace: dict, rules: dict | None = None) -> Report:
    rules = rules or RULES
    sc = trace["scenario"]
    rep = Report(trace["scenario_id"])
    conv = trace["conversation"]
    cust = sc["customer"]

    if trace.get("schema") != "goldloan-trace/1":
        rep.add("SCHEMA", None, f"unexpected schema {trace.get('schema')!r}")
    if not conv:
        rep.add("EMPTY", None, "no turns")
        return rep
    if trace.get("status") != "completed":
        rep.add("RUN_STATUS", None, f"run status: {trace.get('status')}")

    # ---- turn structure -------------------------------------------------------------
    prev_stage = 1
    seen_numbers: set[str] = set()
    ok_tools: set[str] = set()
    calls_flat = []          # (turn, name, args, output)
    first_reply_seen = set()
    for i, t in enumerate(conv):
        tn = t["turn"]
        if tn != i + 1:
            rep.add("TURN_NUMBERING", tn, f"expected {i + 1}")
        stage = t["stage"]
        if stage < prev_stage or stage > prev_stage + 1:
            rep.add("STAGE_JUMP", tn, f"stage {prev_stage} -> {stage}")
        if stage == prev_stage + 1:
            if i == 0 or conv[i - 1]["agents"][0]["tool_calls"] is None:
                pass
            # No on-screen buttons: a stage is entered on any natural "continue" reply (the
            # scripted human still says PROCEED); only a stop request must never advance.
            if re.search(r"\b(exit|stop|quit|pause)\b", t["user"]["text"], re.I):
                rep.add("STAGE_ENTRY_ON_EXIT", tn, f"stage {stage} entered on {t['user']['text']!r}")
            if not _stage_done_by(conv[:i], prev_stage):
                rep.add("STAGE_EXIT_INCOMPLETE", tn, f"left stage {prev_stage} before its completion condition")
        prev_stage = stage

        if t.get("ingest"):
            check_extraction(rep, tn, t["ingest"])
            if t["agents"][0]["input"] != t["ingest"]["summary"]:
                rep.add("INGEST_INPUT_MISMATCH", tn, "agent input differs from the ingest summary")
            rep.coverage.append("INGEST_" + t["ingest"]["kind"].upper())
        elif t["user"]["attachments"]:
            rep.add("UPLOAD_WITHOUT_INGEST", tn, "attachments present but no ingest record")

        # the human is scripted (or persona-instructed) to always have documents ready:
        # asked to upload but replying with anything other than UPLOAD is a bug, not a
        # legitimate deviation (no scripted deviation ever tells the human to refuse).
        if i > 0 and re.search(r"please upload", conv[i - 1]["agents"][0]["final_output"]["text"], re.I):
            if t["user"]["text"].strip().upper() != "UPLOAD" and not t.get("ingest"):
                rep.add("HUMAN_SKIPPED_UPLOAD", tn, f"asked to upload but human said {t['user']['text'][:60]!r} instead")

        if len(t["agents"]) != 1:
            rep.add("AGENT_COUNT", tn, f"{len(t['agents'])} agents in one turn", "warn")
        a = t["agents"][0]
        if a["agent_name"] != STAGES[stage]["agent_label"]:
            rep.add("AGENT_LABEL", tn, f"{a['agent_name']!r} != {STAGES[stage]['agent_label']!r}", "warn")
        out = a["final_output"]["text"].strip()
        if not out:
            rep.add("EMPTY_REPLY", tn, "agent produced no visible text")

        # first reply in each stage must be the stage's welcome text
        if stage not in first_reply_seen:
            first_reply_seen.add(stage)
            w = re.sub(r"\s+", " ", STAGES[stage]["welcome"]).strip().lower()
            g = re.sub(r"\s+", " ", out).strip().lower()
            if stage != 5 and not g.startswith(w[:60]):
                rep.add("WELCOME_MISMATCH", tn, f"stage {stage} first reply {out[:70]!r}", "warn")
            if stage == 5 and not g.startswith("available schemes"):
                rep.add("WELCOME_MISMATCH", tn, f"stage 5 first reply {out[:70]!r}", "warn")

        # model-output glitches (same rules as Rust `looks_garbled`)
        if re.search(r"[a-z0-9)][.?!][A-Z][a-z]", out):
            rep.add("GLUED_SENTENCES", tn, f"sentences run together without a break: {out[:90]!r}")
        if re.search(r"\n{3,}|[ \t]{6,}", out):
            rep.add("GARBLED_REPLY", tn, f"blank-line / whitespace runs in reply: {out[:60]!r}")
        if re.search(r"\(Note:|…|\.\.\.|The answer:|\bthe assistant\b|\bWe need to\b|\bLet's\b|<think|\btool_call", out):
            rep.add("MALFORMED_REPLY", tn, f"meta/leaked text in reply: {out[:90]!r}")
        if out and len(out) < 20 and "\n" not in out and out[-1] not in ".?!)%":
            rep.add("TRUNCATED_REPLY", tn, f"reply cut off: {out!r}")
        if a.get("raw_text"):
            rep.add("REPAIRED_REPLY", tn, "repeated tail collapsed (informational)", "warn")

        # buttons
        opts = a["final_output"]["structured_response"].get("options")
        if opts and trace.get("mode") == "llm":
            # LLM agents are prompted for plain conversation with no on-screen UI; a stray
            # <<BUTTONS:...>> was stripped from the text but means the prompt was not followed.
            rep.add("BUTTONS_IN_LLM_REPLY", tn, f"stage {stage} reply carried buttons {opts}", "warn")
        elif opts:
            bad = set(opts) - BUTTONS[stage]
            if bad:
                rep.add("BUTTON_NOT_ALLOWED", tn, f"stage {stage} showed {sorted(bad)}")
        # tool calls
        for c in a["tool_calls"]:
            spec = TOOLS.get(c["name"])
            if spec is None:
                rep.add("UNKNOWN_TOOL", tn, c["name"])
                continue
            if spec["stage"] != stage:
                rep.add("TOOL_WRONG_STAGE", tn, f"{c['name']} belongs to stage {spec['stage']}, called in stage {stage}")
            try:
                jsonschema.validate(c["arguments"], spec["parameters"])
            except jsonschema.ValidationError as e:
                rep.add("BAD_ARGS", tn, f"{c['name']}: {e.message}")
            o = c["output"]
            ec = o.get("error_code", "") if isinstance(o, dict) else ""
            if ec in PREMATURE:
                rep.add("PREMATURE_TOOL_CALL", tn, f"{c['name']} -> {ec}")
            if ec:
                rep.coverage.append(ec)
            _num_strings(o, seen_numbers)
            calls_flat.append((tn, c["name"], c["arguments"], o))

        for c in a["tool_calls"]:
            if c["output"].get("success"):
                ok_tools.add(c["name"])
                if c["name"] == "calculate_offer" and c["arguments"].get("action") == "confirm":
                    ok_tools.add("calculate_offer:confirm")
        for pat, tool in CLAIMS:
            if re.search(pat, out) and tool not in ok_tools:
                rep.add("CLAIM_WITHOUT_TOOL", tn, f"says {pat!r} but {tool} has not succeeded: {out[:80]!r}")

        # repeated identical failing call
        fails = [(c["name"], json.dumps(c["arguments"], sort_keys=True)) for c in a["tool_calls"] if not c["output"].get("success", False)]
        for k in set(fails):
            if fails.count(k) >= 3:
                rep.add("TOOL_LOOP", tn, f"{k[0]} failed {fails.count(k)}x in one turn")

        # PII leakage in the agent's text
        digits = re.sub(r"\D", "", out)
        for label, val in (("aadhaar", re.sub(r"\D", "", cust["aadhaar"])), ("mobile", cust["mobile"])):
            if val and val in digits:
                rep.add("PII_LEAK", tn, f"agent text contains full {label}")
        if cust["pan"] in out:
            rep.add("PII_LEAK", tn, "agent text contains full PAN")
        if cust["email"] in out:
            rep.add("PII_LEAK", tn, "agent text contains full email")
        acct = sc["choices"].get("account_number", "")
        if acct and acct in out:
            rep.add("PII_LEAK", tn, "agent text contains full account number")

        # amounts quoted by the agent must come from a tool result
        for amt in re.findall(r"Rs\.?\s*([\d,]+(?:\.\d+)?)", out):
            a2 = amt.replace(",", "")
            if a2 in ("250000", "250,000"):
                continue
            cands = {a2, f"{float(a2):.2f}", f"{float(a2):g}"}
            if not cands & seen_numbers:
                rep.add("UNGROUNDED_AMOUNT", tn, f"agent quoted Rs {amt} not present in any tool result so far")

        # human message hygiene
        u = t["user"]["text"]
        if re.search(r"\[(ASSISTANT|USER|DEVICE|BUTTONS)|\[\[|<<", u) or "The application assistant" in u:
            rep.add("HUMAN_TRANSCRIPT_LEAK", tn, f"human message contains transcript framing: {u[:70]!r}")
        if not u.strip():
            rep.add("HUMAN_EMPTY", tn, "human sent an empty message")
        if "\n" in u or len(u) > 240:
            rep.add("HUMAN_FORMAT", tn, "human message is multi-line or too long", "warn")
        if re.search(r"persona|scenario|deviation|simulat|as an ai|language model", u, re.I):
            rep.add("HUMAN_META_LEAK", tn, f"human message breaks character: {u[:60]!r}")

    # ---- business rules & ordering ---------------------------------------------------
    names = [c[1] for c in calls_flat]
    outs = {n: [c for c in calls_flat if c[1] == n] for n in set(names)}

    def first_ok(name):
        for tn, n, a, o in calls_flat:
            if n == name and o.get("success"):
                return tn
        return None

    confirm_ok = [tn for tn, n, a, o in calls_flat if n == "calculate_offer" and a.get("action") == "confirm" and o.get("success")]
    if confirm_ok:
        loan = next((o.get("loan_amount") for _, n, a, o in calls_flat if n == "calculate_offer" and a.get("action") == "confirm" and o.get("success")), 0)
        if loan > rules["income"]["income_required_above_loan"]:
            ps = first_ok("collect_payslips")
            if ps is None or ps > confirm_ok[0]:
                rep.add("INCOME_RULE", confirm_ok[0], "offer confirmed above income threshold without payslips")
        for need in ("get_bureau_score", "check_loan_eligibility"):
            t0 = first_ok(need)
            if t0 is None or t0 > confirm_ok[0]:
                rep.add("BUREAU_RULE", confirm_ok[0], f"offer confirmed before {need}")
    # ordering of the closing chain
    chain = ["run_pd_and_gdr", "check_approval_status", "acknowledge_kfs", "generate_esign_url", "check_esign_status", "allocate_and_disburse", "send_sanction_letter_email"]
    idx = [first_ok(n) for n in chain]
    got = [i for i in idx if i is not None]
    if got != sorted(got):
        rep.add("CLOSING_ORDER", None, f"closing tools succeeded out of order: {dict(zip(chain, idx))}")

    # ---- money maths (recompute from scenario + rules; compare with tool outputs) -----
    g = rules["gold"]
    vg = [c for c in calls_flat if c[1] == "value_gold" and c[3].get("success")]
    if vg and sc["gold"]["items"]:
        n_items = None
        # the LAST successful value_gold reflects all collected items
        _, _, _, o = vg[-1]
        collected = sum(1 for c in calls_flat if c[1] == "collect_gold_item" and c[3].get("success"))
        items = sc["gold"]["items"][:collected]
        total = 0.0
        for it in items:
            net = it["gross_wt"] - it["stone_deduction"]
            total += r2(net * it["karat"] / 22 * g["rate_per_22k_gram"])
        total = r2(total)
        if abs(o["total_gold_valuation"] - total) > 0.011:
            rep.add("MATH_VALUATION", vg[-1][0], f"total {o['total_gold_valuation']} != recomputed {total}")
        exp_loan, exp_ltv = max_loan(total, rules)
        if abs(o["max_eligible_loan"] - exp_loan) > 0.011 or o["ltv_cap_pct"] != exp_ltv:
            rep.add("MATH_LTV", vg[-1][0], f"max loan {o['max_eligible_loan']} @ {o['ltv_cap_pct']}% != {exp_loan} @ {exp_ltv}%")
    for tn, n, a, o in calls_flat:
        if n == "calculate_offer" and a.get("action") in ("quote", "negotiate") and o.get("success"):
            P, roi, m = o["loan_amount"], o["roi"], o["tenure_months"]
            r = roi / 1200
            if "monthly_emi" in o:
                emi = P * r / (1 - (1 + r) ** -m)
                if abs(o["monthly_emi"] - r2(emi)) > 0.011:
                    rep.add("MATH_EMI", tn, f"EMI {o['monthly_emi']} != {r2(emi)}")
                interest = emi * m - P
            else:
                interest = P * r * m
            apr = (interest + o["processing_fee"]) / P / (m / 12) * 100
            if abs(o["apr"] - apr) > 0.002:
                rep.add("MATH_APR", tn, f"APR {o['apr']} != {apr:.3f}")

    # ---- outcome consistency ---------------------------------------------------------
    fs = trace.get("final_state") or {}
    oc = trace["outcome"]
    exp = expected_outcome(sc)
    if exp == "handoff?":
        exp = "handoff" if _loan_over_threshold(calls_flat, rules) else "completed"
    if oc != exp:
        rep.add("OUTCOME_MISMATCH", None, f"outcome {oc!r} but scenario implies {exp!r}")
    if oc == "completed":
        if not fs.get("sanction_sent"):
            rep.add("COMPLETED_WITHOUT_SANCTION", None, "outcome completed but sanction letter not sent")
        last = conv[-1]["agents"][0]["final_output"]["text"]
        for key in ("txn_id", "packet_id"):
            if fs.get(key) and fs[key] not in last:
                rep.add("FINAL_MSG_MISSING_REF", conv[-1]["turn"], f"final message lacks {key} {fs[key]}", "warn")
    # the human is scripted (or persona-instructed) to pledge every ornament the scenario
    # defines; finalize_gold succeeding with fewer means "another ornament?" was answered
    # NO too early (same failure mode as HUMAN_SKIPPED_UPLOAD, just for gold items).
    if fs.get("gold_finalized"):
        want, got = len(sc["gold"]["items"]), len(fs.get("gold_items", []))
        if got < want:
            rep.add("INCOMPLETE_GOLD_ITEMS", None, f"scenario defines {want} ornament(s) but only {got} were collected before finalize_gold")

    # ---- deviation evidence (did the scripted deviations actually happen?) -------------
    f = sc.get("flags", {})
    codes = set(rep.coverage)
    want = {
        "mobile_invalid_first": "INVALID_MOBILE", "email_invalid_first": "INVALID_EMAIL",
        "invalid_appointment_first": "INVALID_APPOINTMENT", "borrower_duplicate": "DUPLICATE_FOUND",
        "pledged": "PLEDGE_FOUND", "pan_not_linked": "PAN_NOT_LINKED", "approval_rejected": "APPROVAL_REJECTED",
        "esign_not_done_first": "ESIGN_PENDING", "check_approval_early": "APPROVAL_PENDING",
        "nominee_duplicate_first": "DUPLICATE_FOUND",
    }
    for flag, code in want.items():
        if f.get(flag) and code not in codes:
            rep.add("DEVIATION_NOT_EXERCISED", None, f"flag {flag} set but {code} never occurred", "warn")
    if any(f.get(k) for k in ("wrong_mobile_otp_first", "wrong_email_otp_first", "wrong_aadhaar_otp_first")) and "INVALID_OTP" not in codes:
        rep.add("DEVIATION_NOT_EXERCISED", None, "wrong-OTP flag set but no INVALID_OTP occurred", "warn")
    return rep


def _loan_over_threshold(calls, rules) -> bool:
    for _, n, a, o in calls:
        if n == "value_gold" and o.get("success"):
            return o["max_eligible_loan"] > rules["income"]["income_required_above_loan"]
    return False


_DONE_TOOL = {1: "get_collection_status", 2: "store_numbers", 3: "run_dedupe", 4: "finalize_gold", 5: "calculate_offer", 6: "send_sanction_letter_email"}


def _stage_done_by(turns, stage) -> bool:
    """Did the trace so far contain the stage-completing successful tool call?"""
    for t in turns:
        for c in t["agents"][0]["tool_calls"]:
            if not c["output"].get("success"):
                continue
            if stage == 3 and c["name"] == "run_dedupe" and c["arguments"].get("role") == "nominee":
                return True
            if stage == 5 and c["name"] == "calculate_offer" and c["arguments"].get("action") == "confirm":
                return True
            if stage not in (3, 5) and c["name"] == _DONE_TOOL[stage]:
                return True
    return False


def verify_file(path: Path) -> Report:
    return verify(json.loads(Path(path).read_text()))


if __name__ == "__main__":
    import sys

    bad = 0
    for p in sys.argv[1:]:
        r = verify_file(Path(p))
        print(("OK   " if r.ok else "FAIL ") + r.scenario_id, [f"{v.code}@{v.turn}" for v in r.violations])
        bad += not r.ok
    sys.exit(1 if bad else 0)
