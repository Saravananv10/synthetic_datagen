"""Verifier + generator tests. Needs the simulator binary (cargo build -p loop-goldloan-gen)."""
import copy
import json
import re
import subprocess
from pathlib import Path

import pytest

from goldloan_gen import scenarios as scn
from goldloan_gen.export_sft import trace_to_samples, vision_to_samples, VISION_SCHEMA
from goldloan_gen.verify import verify, expected_outcome

ROOT = Path(__file__).resolve().parents[2]
BIN = ROOT / "target" / "debug" / "loop-goldloan-gen"
GOLDEN = ROOT / "goldloan-datagen" / "scenarios" / "golden_demo.json"
pytestmark = pytest.mark.skipif(not BIN.exists(), reason="simulator binary not built")


def run(tmp_path, scenario_files):
    out = tmp_path / "traces"
    subprocess.run([str(BIN), "--scenario", *map(str, scenario_files), "--out", str(out), "--force"], check=True, capture_output=True, cwd=ROOT)
    return {p.stem: json.loads(p.read_text()) for p in out.glob("*.json")}


@pytest.fixture(scope="module")
def golden(tmp_path_factory):
    return run(tmp_path_factory.mktemp("g"), [GOLDEN])["golden_demo"]


def codes(trace):
    return {v.code for v in verify(trace).violations if v.severity == "error"}


def turn_with(trace, tool):
    for t in trace["conversation"]:
        for c in t["agents"][0]["tool_calls"]:
            if c["name"] == tool:
                return t, c
    raise AssertionError(tool)


def test_golden_matches_the_demo(golden):
    assert golden["outcome"] == "completed" and golden["status"] == "completed"
    names = [c["name"] for t in golden["conversation"] for c in t["agents"][0]["tool_calls"]]
    # the demo's order of first appearance
    first = list(dict.fromkeys(names))
    assert first == ["store_user_detail", "send_otp", "verify_otp", "get_collection_status", "collect_identity_documents",
                     "generate_aadhar_otp", "verify_aadhar_otp", "store_numbers", "confirm_address", "run_dedupe", "store_nominee",
                     "collect_gold_item", "value_gold", "check_pledge", "finalize_gold", "get_offers", "select_scheme",
                     "calculate_offer", "capture_income", "collect_payslips", "get_bureau_score", "check_loan_eligibility",
                     "run_pd_and_gdr", "check_approval_status", "acknowledge_kfs", "generate_esign_url", "check_esign_status",
                     "allocate_and_disburse", "send_sanction_letter_email"]
    assert codes(golden) == set()
    assert golden["final_state"]["loan_amount"] == 399185.46


def test_upload_turns_show_attachments_but_agent_gets_summary(golden):
    ups = [t for t in golden["conversation"] if t["user"]["attachments"]]
    assert len(ups) == 3
    for t in ups:
        assert t["user"]["text"] == "No input message provided."
        assert t["agents"][0]["input"].startswith("[")
    assert {"pan_card", "aadhaar_front", "aadhaar_back"} == set(ups[0]["user"]["attachments"])


def test_all_fixtures_referenced_exist(golden):
    fx = ROOT / "goldloan-datagen" / "fixtures" / "demo"
    for t in golden["conversation"]:
        for a in t["user"]["attachments"]:
            assert (fx / f"{a}.png").exists(), a


def mutate(trace, fn):
    t = copy.deepcopy(trace)
    fn(t)
    return codes(t)


def test_catches_wrong_emi(golden):
    def f(t):
        _, c = turn_with(t, "calculate_offer")
        c["output"]["monthly_emi"] += 5
    assert "MATH_EMI" in mutate(golden, f)


def test_catches_pii_leak(golden):
    def f(t):
        t["conversation"][8]["agents"][0]["final_output"]["text"] += " Aadhaar 8316 4683 3628"
    assert "PII_LEAK" in mutate(golden, f)


def test_catches_ungrounded_amount(golden):
    def f(t):
        tt, _ = turn_with(t, "value_gold")
        tt["agents"][0]["final_output"]["text"] += " You can borrow Rs 777777.00"
    assert "UNGROUNDED_AMOUNT" in mutate(golden, f)


def test_catches_tool_from_wrong_stage(golden):
    def f(t):
        t["conversation"][0]["agents"][0]["tool_calls"].append(
            {"name": "get_bureau_score", "arguments": {"trigger": "trigger"}, "output": {"success": True}, "clock_s": 0})
    assert "TOOL_WRONG_STAGE" in mutate(golden, f)


def test_catches_bad_args_and_unknown_tool(golden):
    def f(t):
        _, c = turn_with(t, "verify_otp")
        c["arguments"]["otp_type"] = "fax"
    assert "BAD_ARGS" in mutate(golden, f)

    def g(t):
        _, c = turn_with(t, "verify_otp")
        c["name"] = "verify_otp_v2"
    assert "UNKNOWN_TOOL" in mutate(golden, g)


def test_catches_premature_call(golden):
    def f(t):
        _, c = turn_with(t, "check_pledge")
        c["output"] = {"success": False, "error_code": "GOLD_NOT_VALUED", "message": "x"}
    assert "PREMATURE_TOOL_CALL" in mutate(golden, f)


def test_catches_skipped_income_and_bureau_rules(golden):
    def f(t):
        for tt in t["conversation"]:
            tt["agents"][0]["tool_calls"] = [c for c in tt["agents"][0]["tool_calls"] if c["name"] not in ("collect_payslips", "get_bureau_score")]
    got = mutate(golden, f)
    assert {"INCOME_RULE", "BUREAU_RULE"} <= got


def test_catches_outcome_mismatch_and_stage_skip(golden):
    assert "OUTCOME_MISMATCH" in mutate(golden, lambda t: t.__setitem__("outcome", "rejected"))

    def f(t):
        t["conversation"][10]["stage"] = 4
    assert "STAGE_JUMP" in mutate(golden, f)


def test_catches_malformed_and_glued_replies(golden):
    def f(t):
        t["conversation"][2]["agents"][0]["final_output"]["text"] = "Please enter the 6-digit OTP ... (Note: user needs to provide)"
    assert "MALFORMED_REPLY" in mutate(golden, f)

    def g(t):
        t["conversation"][2]["agents"][0]["final_output"]["text"] = "Please enter the name.Please provide the customer's name."
    assert "GLUED_SENTENCES" in mutate(golden, g)


def test_scenarios_cover_every_variant_and_are_deterministic():
    a, b = scn.generate(60, 3), scn.generate(60, 3)
    assert json.dumps(a, sort_keys=True) == json.dumps(b, sort_keys=True)
    tags = {t for s in scn.generate(60, 3) for t in s["tags"]}
    assert set(scn.SINGLE) | set(scn.TERMINAL) <= tags | {"happy"}


def test_generated_scenarios_all_verify(tmp_path):
    files = []
    for s in scn.generate(60, 11):
        p = tmp_path / f"{s['id']}.json"
        p.write_text(json.dumps(s))
        files.append(p)
    traces = run(tmp_path, files)
    assert len(traces) == 60
    bad = {k: sorted(codes(v)) for k, v in traces.items() if codes(v)}
    assert not bad, bad
    # every scenario ended the way its ground truth says it should
    for t in traces.values():
        assert t["outcome"] == (expected_outcome(t["scenario"]) if expected_outcome(t["scenario"]) != "handoff?" else t["outcome"])


def test_exports_sft_with_paired_tool_messages(golden):
    samples = trace_to_samples(golden)
    assert [s["meta"]["stage"] for s in samples] == [1, 2, 3, 4, 5, 6]
    for s in samples:
        ids_called = [tc["id"] for m in s["messages"] if m.get("tool_calls") for tc in m["tool_calls"]]
        ids_returned = [m["tool_call_id"] for m in s["messages"] if m["role"] == "tool"]
        assert ids_called == ids_returned
        assert s["messages"][0]["role"] == "system" and s["messages"][-1]["role"] == "assistant"
        assert s["tools"], "tool definitions attached"


def test_invalid_scenario_is_rejected_not_looped(tmp_path):
    sc = json.loads(GOLDEN.read_text())
    sc["id"] = "bad_scenario"
    sc["choices"]["account_number"] = ""
    p = tmp_path / "bad.json"
    p.write_text(json.dumps(sc))
    r = subprocess.run([str(BIN), "--scenario", str(p), "--out", str(tmp_path / "o")], capture_output=True, text=True, cwd=ROOT)
    assert "INVALID" in r.stdout and not list((tmp_path / "o").glob("*.json"))


def test_ingest_records_show_scan_text_and_extraction(golden):
    ing = [t["ingest"] for t in golden["conversation"] if t.get("ingest")]
    assert [i["kind"] for i in ing] == ["identity", "gold", "payslips"]
    ident = ing[0]
    assert {d["doc"] for d in ident["docs"]} == {"pan", "aadhaar_front", "aadhaar_back"}
    pan = next(d for d in ident["docs"] if d["doc"] == "pan")
    assert "BDAPD1327A" in pan["scan_text"] and pan["extracted"] == pan["truth"]
    assert ident["vision_model"] == "deterministic"


def test_catches_accepted_wrong_extraction(golden):
    def f(t):
        ing = next(x["ingest"] for x in t["conversation"] if x.get("ingest", {}).get("kind") == "gold")
        cert = next(d for d in ing["docs"] if d["doc"] == "gold_certificate")
        cert["extracted"]["gross_weight_g"] = 99.0      # GPT misread; summary still says PASS
    assert "EXTRACTION_ERROR_ACCEPTED" in mutate(golden, f)


def test_wrong_extraction_that_validation_rejects_is_only_a_warning(golden):
    t = copy.deepcopy(golden)
    ing = next(x["ingest"] for x in t["conversation"] if x.get("ingest", {}).get("kind") == "gold")
    next(d for d in ing["docs"] if d["doc"] == "gold_certificate")["extracted"]["gross_weight_g"] = 99.0
    ing["summary"] = ing["summary"].replace("Status: PASS", "Status: WEIGHT_MISMATCH")
    r = verify(t)
    assert any(v.code == "VISION_EXTRACTION_ERROR" and v.severity == "warn" for v in r.violations)
    assert "EXTRACTION_ERROR_ACCEPTED" not in {v.code for v in r.violations}


def test_ocr_noise_flag_forces_a_reupload(tmp_path):
    sc = json.loads(GOLDEN.read_text())
    sc["id"], sc["flags"] = "noisy_scan", {"ocr_noise_first": True}
    p = tmp_path / "n.json"
    p.write_text(json.dumps(sc))
    t = run(tmp_path, [p])["noisy_scan"]
    ing = [x["ingest"] for x in t["conversation"] if x.get("ingest") and x["ingest"]["kind"] == "identity"]
    assert len(ing) == 2 and "UNREADABLE" in ing[0]["summary"] and "COMPLETE" in ing[1]["summary"]
    assert t["outcome"] == "completed" and codes(t) == set()


def test_catches_claim_without_tool_call(golden):
    """The GPT failure seen live: 'OTP sent successfully for Aadhaar' with no generate_aadhar_otp call."""
    def f(t):
        tt, _ = turn_with(t, "collect_identity_documents")
        tt["agents"][0]["final_output"]["text"] += " OTP sent successfully for Aadhaar XXXX-XXXX-3628."
        for x in t["conversation"]:
            x["agents"][0]["tool_calls"] = [c for c in x["agents"][0]["tool_calls"] if c["name"] != "generate_aadhar_otp"]
    assert "CLAIM_WITHOUT_TOOL" in mutate(golden, f)


def test_catches_garbled_and_truncated_replies_and_transcript_leaks(golden):
    def f(t):
        t["conversation"][5]["agents"][0]["final_output"]["text"] = "All      \n\n\n\nThe answer: \n\n\n\nAll onboarding details collected."
    assert {"GARBLED_REPLY", "MALFORMED_REPLY"} <= mutate(golden, f)

    def g(t):
        t["conversation"][4]["agents"][0]["final_output"]["text"] = "Please enter"
    assert "TRUNCATED_REPLY" in mutate(golden, g)

    def h(t):
        t["conversation"][4]["user"]["text"] = "aarav.d26@example.com[ASSISTANT]: Please enter the customer'"
    assert "HUMAN_TRANSCRIPT_LEAK" in mutate(golden, h)


def test_vision_sft_export_mirrors_the_ingest_docs(golden):
    samples = vision_to_samples(golden)
    ing_docs = [d for t in golden["conversation"] if t.get("ingest") for d in t["ingest"]["docs"]]
    assert len(samples) == len(ing_docs) == 8  # pan, aadhaar x2, gold photo+cert, 3 payslips
    kinds = {s["meta"]["doc"] for s in samples}
    assert kinds == {"pan", "aadhaar_front", "aadhaar_back", "gold_photo", "gold_certificate", "payslip"}
    for s in samples:
        assert s["messages"][0]["role"] == "system" and "OCR" in s["messages"][0]["content"]
        user = s["messages"][1]["content"]
        assert user.startswith(f"DOCUMENT TYPE: {s['meta']['doc']}\n") and "--- SCAN TEXT ---" in user
        assert VISION_SCHEMA[s["meta"]["doc"]] in user
        got = json.loads(s["messages"][2]["content"])
        assert got  # non-empty JSON object
        # golden is deterministic mode: extraction == ground truth for every doc
        assert s["meta"]["matched_truth"] is True
        assert s["meta"]["vision_model"] == "deterministic"


def test_vision_sft_target_extracted_vs_truth_diverge_on_a_misread(golden):
    t = copy.deepcopy(golden)
    ing = next(x["ingest"] for x in t["conversation"] if x.get("ingest", {}).get("kind") == "gold")
    cert = next(d for d in ing["docs"] if d["doc"] == "gold_certificate")
    cert["extracted"] = dict(cert["extracted"], gross_weight_g=99.0)  # simulate a vision misread
    ex = vision_to_samples(t, "extracted")
    tr = vision_to_samples(t, "truth")
    got_ex = next(json.loads(s["messages"][2]["content"]) for s in ex if s["meta"]["doc"] == "gold_certificate")
    got_tr = next(json.loads(s["messages"][2]["content"]) for s in tr if s["meta"]["doc"] == "gold_certificate")
    assert got_ex["gross_weight_g"] == 99.0 and got_tr["gross_weight_g"] == cert["truth"]["gross_weight_g"]
    assert next(s for s in ex if s["meta"]["doc"] == "gold_certificate")["meta"]["matched_truth"] is False


def test_catches_human_refusing_to_upload(golden):
    """Live bug found with the GPT human-sim: asked to upload, it replied "don't have it"
    instead of UPLOAD, and the conversation later exited instead of completing."""
    upload_ask = next(i for i, c in enumerate(golden["conversation"]) if re.search(r"please upload", c["agents"][0]["final_output"]["text"], re.I))
    upload_turn = upload_ask + 1
    assert golden["conversation"][upload_turn]["ingest"], "fixture assumption: the very next turn is the upload"

    def f(t):
        tt = t["conversation"][upload_turn]
        tt["user"] = {"role": tt["user"]["role"], "text": "don't have it", "attachments": [], "device": []}
        del tt["ingest"]
    got = mutate(golden, f)
    assert "HUMAN_SKIPPED_UPLOAD" in got
    # replying UPLOAD (with its ingest record intact) must NOT trip the check
    assert "HUMAN_SKIPPED_UPLOAD" not in codes(golden)


def test_address_extraction_tolerates_a_dropped_redundant_pincode_but_not_a_real_error(golden):
    """Live false positive found with GPT vision: it correctly extracted pincode as its own
    field and did not repeat it inside `address` (our scan text has no separate PIN label,
    matching real Aadhaar layouts) - that must not be flagged as a bad extraction. A genuinely
    wrong address (different city) still must be."""
    ing = next(x["ingest"] for x in golden["conversation"] if x.get("ingest", {}).get("kind") == "identity")
    back = next(d for d in ing["docs"] if d["doc"] == "aadhaar_back")
    assert back["truth"]["address"].endswith(back["truth"]["pincode"])  # fixture assumption

    def drop_pin(t):
        ing2 = next(x["ingest"] for x in t["conversation"] if x.get("ingest", {}).get("kind") == "identity")
        d = next(x for x in ing2["docs"] if x["doc"] == "aadhaar_back")
        d["extracted"]["address"] = re.sub(r"[-,]?\s*\d{6}\s*$", "", d["extracted"]["address"])
    assert not (mutate(golden, drop_pin) & {"VISION_EXTRACTION_ERROR", "EXTRACTION_ERROR_ACCEPTED"})

    def wrong_city(t):
        ing2 = next(x["ingest"] for x in t["conversation"] if x.get("ingest", {}).get("kind") == "identity")
        d = next(x for x in ing2["docs"] if x["doc"] == "aadhaar_back")
        d["extracted"]["address"] = d["extracted"]["address"].replace("Telangana", "Kerala")
    assert "EXTRACTION_ERROR_ACCEPTED" in mutate(golden, wrong_city)


def test_catches_finalizing_gold_with_fewer_items_than_the_scenario_defines(golden):
    """Live bug found with the GPT human-sim on an extra_ornament scenario: it said NO to
    "another ornament?" after uploading only 1 of the 2 the scenario defines, so gold_finalized
    ended up true with only one item collected."""
    def f(t):
        t["scenario"]["gold"]["items"].append(dict(t["scenario"]["gold"]["items"][0]))
    assert "INCOMPLETE_GOLD_ITEMS" in mutate(golden, f)
    # the golden trace itself (1 defined item, 1 collected) must not be flagged
    assert "INCOMPLETE_GOLD_ITEMS" not in codes(golden)
