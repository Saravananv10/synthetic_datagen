"""Coverage-driven scenario generator.

Every scenario is a fictional customer + a set of scripted deviations. Identity
values are synthetic (random PAN-/Aadhaar-shaped strings, example.com emails);
uploaded-image attachments refer to the demo's *sample* fixtures as placeholders
(the pipeline's agents only ever see the text ingest summaries, which carry the
scenario's real values).

    python -m goldloan_gen.scenarios --n 200 --out scenarios/generated --seed 1
"""
from __future__ import annotations

import argparse
import json
import random
import string
from pathlib import Path

from .rules import RULES, max_loan

FIRST = ["Aarav", "Vivaan", "Aditya", "Sai", "Arjun", "Rohan", "Karthik", "Suresh", "Ramesh", "Mahesh", "Priya", "Ananya",
         "Divya", "Lakshmi", "Meena", "Kavya", "Sneha", "Pooja", "Deepa", "Anjali", "Venkat", "Harish", "Naveen", "Ganesh"]
LAST = ["Reddy", "Sharma", "Iyer", "Nair", "Patel", "Kumar", "Singh", "Rao", "Naidu", "Gupta", "Menon", "Das", "Joshi", "Pillai"]
PLACES = [("Hyderabad", "Telangana", "5000{:02d}"), ("Chennai", "Tamil Nadu", "6000{:02d}"), ("Bengaluru", "Karnataka", "5600{:02d}"),
          ("Pune", "Maharashtra", "4110{:02d}"), ("Kochi", "Kerala", "6820{:02d}"), ("Vijayawada", "Andhra Pradesh", "5200{:02d}")]
STREETS = ["Gandhi Nagar", "Ashok Colony", "Lake View Road", "Rajaji Street", "MG Road", "Temple Street", "Green Park"]
ORNAMENTS = [("necklace", "Hallmarked gold necklace with pendant"), ("bangle", "Hallmarked gold bangles (pair)"),
             ("chain", "Hallmarked gold chain"), ("ring", "Hallmarked gold ring"), ("earrings", "Hallmarked gold earrings"),
             ("mangalsutra", "Hallmarked gold mangalsutra")]
LANGS = RULES["languages"]
OFFERS = [s["offer_id"] for s in RULES["schemes"]]
EMPLOYERS = ["Sunrise Infosystems Pvt Ltd", "Lakshmi General Hospital", "Vidya Bharati School", "Greenfield Auto Components", "Kaveri Logistics Pvt Ltd",
             "Anand Textiles", "Metro Care Clinic", "Bluewave Solutions Pvt Ltd", "Shree Ganesh Traders", "Horizon Pharma Distributors"]
SALARIED = ["software support engineer", "staff nurse", "school teacher", "factory supervisor", "accounts executive", "logistics coordinator"]
SELF_EMPLOYED = ["kirana shop owner", "tailor", "small farmer", "auto-rickshaw owner", "beautician"]
TEMPERAMENTS = ["hurried and impatient", "anxious first-time borrower", "chatty and friendly", "sceptical, double-checks everything",
                "very polite and formal", "tech-savvy and terse", "easily confused by jargon", "distracted, answers half-attentively"]
VOICES = ["plain English, lowercase, little punctuation", "English with light Hinglish", "full formal sentences", "terse with abbreviations",
          "English with the occasional typo"]
STYLES = ["", "Terse and businesslike, lowercase is fine.", "Polite and slightly wordy, full sentences where natural.",
          "Types fast: occasional typos and no punctuation.", "Formal, uses the customer's full details exactly as given."]

SINGLE = ["mobile_invalid_first", "email_invalid_first", "wrong_mobile_otp_first", "wrong_email_otp_first", "wrong_aadhaar_otp_first",
          "identity_upload_incomplete_first", "override_address", "invalid_appointment_first", "nominee_duplicate_first",
          "gold_weight_mismatch_first", "extra_ornament", "change_scheme_once", "negotiate_roi", "negotiate_roi_over_limit",
          "payslips_bad_first", "ocr_noise_first", "check_approval_early", "esign_not_done_first", "bullet_scheme", "small_loan", "cash_disbursement",
          "imps_disbursement", "sanction_email_retry"]
TERMINAL = ["pan_not_linked", "borrower_duplicate", "pledged", "low_cibil", "self_employed", "approval_rejected", "decline_kfs",
            "exit_stage_1", "exit_stage_2", "exit_stage_3", "exit_stage_4", "exit_stage_5", "exit_stage_6"]
COMBINABLE = [v for v in SINGLE if v not in ("small_loan",)]


def _digits(rng, n):
    return "".join(rng.choice(string.digits) for _ in range(n))


def _base(rng: random.Random, sid: str, seed: int) -> dict:
    first, last = rng.choice(FIRST), rng.choice(LAST)
    name = f"{first} {last}"
    city, state, pin_t = rng.choice(PLACES)
    pin = pin_t.format(rng.randint(1, 99))
    pan = "".join(rng.choice(string.ascii_uppercase) for _ in range(3)) + "P" + last[0].upper() + _digits(rng, 4) + rng.choice(string.ascii_uppercase)
    n_items = 1
    items = []
    for _ in range(n_items):
        kind, desc = rng.choice(ORNAMENTS)
        gross = round(rng.uniform(58, 140), 2)
        ded = round(rng.choice([0.0, 0.4, 0.9, 1.6, 2.1, 3.2]), 2)
        items.append({"type": kind, "description": desc, "gross_wt": gross, "stone_deduction": ded, "karat": rng.choice([18, 20, 22]),
                      "scale_reading": gross, "photo_fixture": "necklace_on_scale", "doc_fixture": "gold_valuation_certificate"})
    months = ["2026-08", "2026-07", "2026-06"] if rng.random() < 0.5 else ["2026-07", "2026-06", "2026-05"]
    net = rng.choice(range(28000, 150001, 500))
    slips = [{"month": m, "net_pay": float(net + rng.choice([-500, 0, 0, 500])), "name": name, "fixture": f"payslip_2026_0{k}"}
             for m, k in zip(sorted(months), (4, 5, 6))]
    nominee = {"name": f"{rng.choice(FIRST)} {last}", "phone": rng.choice("6789") + _digits(rng, 9), "relation": rng.choice(RULES["nominee_relations"])}
    persona = {"age": rng.randint(24, 62), "occupation": rng.choice(SALARIED), "temperament": rng.choice(TEMPERAMENTS),
               "background": "Needs short-term funds and has gold at home.", "voice": rng.choice(VOICES)}
    return {
        "id": sid, "seed": seed, "start_epoch": 1_790_100_000 + rng.randint(0, 80_000), "today_month": "2026-09",
        "human_role": "operator",
        "customer": {"mobile": rng.choice("6789") + _digits(rng, 9), "name": name,
                     "email": f"{first.lower()}.{last.lower()}{rng.randint(1, 99)}@example.com",
                     "aadhaar": f"{rng.randint(2, 9)}{_digits(rng, 3)} {_digits(rng, 4)} {_digits(rng, 4)}", "pan": pan,
                     "address": f"Flat No- {rng.randint(1, 900)}, {rng.choice(STREETS)}, {city}, {state} - {pin}", "pincode": pin,
                     "dob": f"{rng.randint(1, 28):02d}/{rng.randint(1, 12):02d}/{rng.randint(1965, 2001)}",
                     "father": f"{rng.choice(FIRST)} {last}"},
        "profile": {"appointment_id": f"AP{rng.randint(1, 99):02d}", "language": rng.choice(LANGS), "nominee": nominee},
        "gold": {"items": items},
        "income": {"employment_type": "salaried", "employer": rng.choice(EMPLOYERS), "payslips": slips},
        "persona": persona,
        "bureau": {"cibil": rng.randint(700, 820)},
        "choices": {"offer_id": rng.choice(OFFERS), "disbursement_mode": "NEFT", "account_number": _digits(rng, rng.choice([11, 12, 14])),
                    "ifsc": rng.choice(["SBIN", "HDFC", "ICIC", "UBIN"]) + "0" + _digits(rng, 6)},
        "flags": {}, "tool_failures": [], "style": rng.choice(STYLES), "tags": [],
    }


def apply_variant(sc: dict, v: str, rng: random.Random) -> None:
    f = sc["flags"]
    sc["tags"].append(v)
    if v in ("mobile_invalid_first", "email_invalid_first", "wrong_mobile_otp_first", "wrong_email_otp_first", "wrong_aadhaar_otp_first",
             "identity_upload_incomplete_first", "override_address", "invalid_appointment_first", "nominee_duplicate_first",
             "gold_weight_mismatch_first", "change_scheme_once", "payslips_bad_first", "ocr_noise_first", "check_approval_early", "esign_not_done_first",
             "pan_not_linked", "borrower_duplicate", "pledged", "approval_rejected", "decline_kfs"):
        f[v] = True
    if v == "nominee_duplicate_first":
        alt = dict(sc["profile"]["nominee"])
        alt.update(name=f"{rng.choice(FIRST)} {sc['customer']['name'].split()[-1]}", phone=rng.choice("6789") + _digits(rng, 9))
        sc["profile"]["nominee_alt"] = alt
    elif v == "extra_ornament":
        n = rng.choice([2, 3])
        for _ in range(n - 1):
            kind, desc = rng.choice(ORNAMENTS)
            g = round(rng.uniform(12, 60), 2)
            sc["gold"]["items"].append({"type": kind, "description": desc, "gross_wt": g, "stone_deduction": round(rng.choice([0.0, 0.5, 1.2]), 2),
                                        "karat": rng.choice([18, 20, 22]), "scale_reading": g,
                                        "photo_fixture": "necklace_on_scale", "doc_fixture": "gold_valuation_certificate"})
    elif v == "change_scheme_once":
        others = [o for o in OFFERS if o != sc["choices"]["offer_id"]]
        sc["choices"]["first_offer_id"] = rng.choice(others)
    elif v in ("negotiate_roi", "negotiate_roi_over_limit"):
        f["negotiate_roi"] = True
        roi = next(s["roi"] for s in RULES["schemes"] if s["offer_id"] == sc["choices"]["offer_id"])
        sc["choices"]["requested_roi"] = round(roi - (0.3 if v == "negotiate_roi" else 0.9), 2)
        if v == "negotiate_roi_over_limit":
            sc["tags"].append("negotiation_refused")
    elif v == "bullet_scheme":
        sc["choices"]["offer_id"] = "GL_FULL_BULLET_12"
    elif v == "small_loan":
        g = round(rng.uniform(22, 40), 2)
        it = sc["gold"]["items"][0]
        it.update(gross_wt=g, scale_reading=g, karat=22, stone_deduction=0.0)
    elif v == "cash_disbursement":
        sc["choices"].update(disbursement_mode="CASH", account_number="", ifsc="")
    elif v == "imps_disbursement":
        sc["choices"]["disbursement_mode"] = "IMPS"
    elif v == "sanction_email_retry":
        sc["tool_failures"].append({"tool": "send_sanction_letter_email", "times": 1, "error_type": "RuntimeError",
                                    "message": "OAuth token request timed out after 45s: ConnectTimeout"})
    elif v == "low_cibil":
        sc["bureau"]["cibil"] = rng.randint(520, 649)
    elif v == "self_employed":
        sc["income"]["employment_type"] = "self_employed"
        sc["income"]["payslips"] = []
        sc["persona"]["occupation"] = rng.choice(SELF_EMPLOYED)
    elif v.startswith("exit_stage_"):
        f["exit_at_stage"] = int(v.rsplit("_", 1)[1])


# Variants that cannot coexist (they set the same scenario field), and the order
# scheme-affecting variants must be applied in (later ones read the chosen offer).
EXCLUSIVE = [{"cash_disbursement", "imps_disbursement"}, {"negotiate_roi", "negotiate_roi_over_limit"}]
APPLY_FIRST = ["bullet_scheme", "change_scheme_once"]


def normalize(variants: list[str]) -> list[str]:
    out: list[str] = []
    for v in variants:
        if any(v in g and (g & set(out)) for g in EXCLUSIVE):
            continue
        out.append(v)
    return sorted(out, key=lambda v: (APPLY_FIRST.index(v) if v in APPLY_FIRST else len(APPLY_FIRST)))


def generate(n: int, seed: int) -> list[dict]:
    rng = random.Random(seed)
    plan: list[list[str]] = []
    # deterministic coverage sweep first, then a weighted random tail
    plan.append([])
    plan += [[v] for v in SINGLE]
    plan += [[v] for v in TERMINAL]
    while len(plan) < n:
        r = rng.random()
        if r < 0.15:
            plan.append([])
        elif r < 0.60:
            plan.append([rng.choice(SINGLE)])
        elif r < 0.90:
            plan.append(rng.sample(COMBINABLE, rng.choice([2, 3, 4])))
        else:
            plan.append([rng.choice(TERMINAL)])
    out = []
    for i, variants in enumerate(normalize(v) for v in plan[:n]):
        sid = f"gl_{i:04d}_" + ("happy" if not variants else "+".join(variants)[:60])
        sc = _base(rng, sid, seed * 100_000 + i)
        for v in variants:
            apply_variant(sc, v, rng)
        # some scenarios that need a large loan to exercise the income branch
        if "self_employed" in variants:
            it = sc["gold"]["items"][0]
            it.update(gross_wt=round(rng.uniform(90, 140), 2), scale_reading=None, karat=22, stone_deduction=0.0)
            it["scale_reading"] = it["gross_wt"]
        if "small_loan" not in variants:
            _ensure_large_loan(sc)
        if not sc["tags"]:
            sc["tags"].append("happy")
        # the scripted email-failure is realistic in ~25% of everything else too
        if not sc["tool_failures"] and rng.random() < 0.25:
            sc["tool_failures"].append({"tool": "send_sanction_letter_email", "times": 1, "error_type": "RuntimeError",
                                        "message": "OAuth token request timed out after 45s: ConnectTimeout"})
        out.append(sc)
    return out


def _loan(sc) -> float:
    g = RULES["gold"]
    tot = sum(round((it["gross_wt"] - it["stone_deduction"]) * it["karat"] / 22 * g["rate_per_22k_gram"], 2) for it in sc["gold"]["items"])
    return max_loan(round(tot, 2))[0]


def _ensure_large_loan(sc) -> None:
    """Non-'small_loan' scenarios should exercise the income branch (loan > 250k)."""
    it = sc["gold"]["items"][0]
    while _loan(sc) <= 275_000:
        it["gross_wt"] = round(it["gross_wt"] + 15, 2)
        it["scale_reading"] = it["gross_wt"]


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--n", type=int, default=100)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--out", type=Path, default=Path("scenarios/generated"))
    a = ap.parse_args()
    a.out.mkdir(parents=True, exist_ok=True)
    scs = generate(a.n, a.seed)
    for sc in scs:
        (a.out / f"{sc['id']}.json").write_text(json.dumps(sc, indent=2))
    print(f"wrote {len(scs)} scenarios to {a.out}")


if __name__ == "__main__":
    main()
