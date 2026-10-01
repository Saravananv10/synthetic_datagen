#!/usr/bin/env python3
"""Generates spec/tools.json — the tool schemas the stage agents see.

`provenance`: 'observed'  = tool name AND argument shape seen in the demo trace
              'partial'   = name seen, arguments inferred from outputs/error text
              'inferred'  = name inferred from another tool's message (e.g. capture_income)
The real production tool descriptions are empty strings in the demo trace; the
descriptions here are ours.
"""
import json
from pathlib import Path

def trig(default="trigger"):
    return {"type": "object", "properties": {"trigger": {"type": "string", "enum": [default]}}, "required": ["trigger"]}

def obj(props, req=None):
    return {"type": "object", "properties": props, "required": req if req is not None else list(props)}

S = {"type": "string"}
T = [
    # ---- stage 1: onboarding ------------------------------------------------
    ("store_user_detail", 1, "observed", "Store one onboarding field (mobile, name or email) in the session.",
     obj({"field_name": {"type": "string", "enum": ["mobile", "name", "email"]}, "value": S})),
    ("send_otp", 1, "observed", "Send a 6-digit OTP to the customer's mobile number or email (must be stored first).",
     obj({"otp_type": {"type": "string", "enum": ["mobile", "email"]}, "recipient": S}, ["otp_type"])),
    ("verify_otp", 1, "observed", "Verify the OTP the customer received on mobile or email.",
     obj({"otp": S, "otp_type": {"type": "string", "enum": ["mobile", "email"]}})),
    ("get_collection_status", 1, "observed", "Check whether all onboarding fields are collected and verified.", trig()),
    # ---- stage 2: identity --------------------------------------------------
    ("collect_identity_documents", 2, "observed", "Persist the Aadhaar/PAN documents summarised by the upload ingest step.", trig()),
    ("generate_aadhar_otp", 2, "observed", "Send an OTP to the Aadhaar-linked mobile number (Aadhaar OTP eKYC).", trig()),
    ("verify_aadhar_otp", 2, "observed", "Verify the Aadhaar OTP.", obj({"otp": S})),
    ("store_numbers", 2, "observed", "After customer consent, verify the PAN-Aadhaar link and store the verified PAN/Aadhaar numbers.", trig()),
    # ---- stage 3: profile / address ----------------------------------------
    ("confirm_address", 3, "observed", "Confirm the address (from Aadhaar, or override with a proof upload) or save the appointment id and preferred language.",
     obj({"action": {"type": "string", "enum": ["use_aadhaar", "override", "set_appointment"]},
          "appointment_id": S, "preferred_language": S}, ["action"])),
    ("run_dedupe", 3, "observed", "Run the duplicate-customer check for the borrower or the nominee.",
     obj({"role": {"type": "string", "enum": ["borrower", "nominee"]}})),
    ("store_nominee", 3, "observed", "Store the nominee's name, 10-digit phone and relation.",
     obj({"name": S, "phone": S, "relation": {"type": "string", "description": "spouse, child, parent, sibling or other (any letter case; the demo passed 'Sibling')"}})),
    # ---- stage 4: gold valuation -------------------------------------------
    ("collect_gold_item", 4, "observed", "Persist the ornament summarised by the gold-ingest step (photo on scale + certificate).", trig()),
    ("value_gold", 4, "observed", "Value all collected gold items and compute the maximum eligible loan.", trig("value")),
    ("check_pledge", 4, "observed", "Check that the gold is not already pledged.", trig("check")),
    ("finalize_gold", 4, "observed", "Close the gold stage after the last ornament.", trig("finalize")),
    # ---- stage 5: offer -----------------------------------------------------
    ("get_offers", 5, "observed", "Load the available loan schemes.", trig()),
    ("select_scheme", 5, "observed", "Record the scheme the customer selected.", obj({"offer_id": S})),
    ("calculate_offer", 5, "observed", "quote: compute the plan for the selected scheme; negotiate: request a lower ROI; confirm: confirm the offer (fails until income and bureau steps are done).",
     obj({"action": {"type": "string", "enum": ["quote", "negotiate", "confirm"]}, "tenure_months": S,
          "requested_roi": {"type": "number"}}, ["action"])),
    ("capture_income", 5, "inferred", "Record the customer's employment type (required for loans above the income threshold).",
     obj({"employment_type": {"type": "string", "enum": ["salaried", "self_employed"]}, "declared_income": {"type": "number"}}, ["employment_type"])),
    ("collect_payslips", 5, "observed", "Persist the payslips summarised by the payslip-ingest step.", trig()),
    ("get_bureau_score", 5, "observed", "Fetch the customer's credit bureau (CIBIL) score.", trig()),
    ("check_loan_eligibility", 5, "observed", "Compute final loan eligibility from gold value, income and bureau score.", trig()),
    # ---- stage 6: closing ---------------------------------------------------
    ("run_pd_and_gdr", 6, "observed", "pd: record that the personal discussion is complete; gdr: issue the Gold Description Receipt and start the cooling-off wait.",
     obj({"action": {"type": "string", "enum": ["pd", "gdr"]}})),
    ("check_approval_status", 6, "observed", "Check approval after the cooling-off period.", trig()),
    ("acknowledge_kfs", 6, "observed", "Record whether the customer accepted or declined the Key Fact Statement.",
     obj({"action": {"type": "string", "enum": ["accept", "decline"]}})),
    ("generate_esign_url", 6, "observed", "Generate the e-sign link for the customer.", trig()),
    ("check_esign_status", 6, "observed", "Check whether the customer completed the e-sign.", trig()),
    ("allocate_and_disburse", 6, "partial", "allocate: reserve a packet; disburse: soft-allocate the amount (mock transaction) in the chosen mode.",
     obj({"action": {"type": "string", "enum": ["allocate", "disburse"]},
          "disbursement_mode": {"type": "string", "enum": ["NEFT", "IMPS", "CASH"]},
          "account_number": S, "ifsc": S}, ["action"])),
    ("send_sanction_letter_email", 6, "observed", "Email the sanction letter to the customer.", trig()),
]

tools = [{"name": n, "stage": st, "provenance": pv, "description": d, "parameters": p} for n, st, pv, d, p in T]
out = Path(__file__).resolve().parent.parent / "spec" / "tools.json"
out.write_text(json.dumps({"tools": tools}, indent=2))
print(f"wrote {len(tools)} tools -> {out}")
