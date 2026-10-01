//! Deterministic oracle agent: implements each stage prompt as a state machine.
//! Serves three purposes: (1) golden traces for the verifier, (2) an offline
//! regression oracle for the backend, (3) the "expected behaviour" the LLM
//! agents are compared against.

use std::collections::HashMap;

use regex::Regex;
use serde_json::{json, Value};

use crate::backend::{mask_email, mask_mobile, Backend};
use crate::reply::*;
use crate::stages;

#[derive(Default)]
struct Mem {
    turn: u32,
    tenure: String,
    appt: Option<String>,
    nominee_name: Option<String>,
    nominee_phone: Option<String>,
    awaiting_change: bool,
    mode: Option<String>,
    acct: Option<String>,
}

#[derive(Default)]
pub struct ScriptedAgent {
    mem: HashMap<u8, Mem>,
}

fn money(x: f64) -> String {
    format!("{x:.2}")
}

impl ScriptedAgent {
    pub fn respond(&mut self, stage: u8, input: &str, be: &mut Backend) -> String {
        let mut m = self.mem.remove(&stage).unwrap_or_default();
        let s = input.trim();
        let out = match stage {
            1 => self.s1(&mut m, s, be),
            2 => self.s2(&mut m, s, be),
            3 => self.s3(&mut m, s, be),
            4 => self.s4(&mut m, s, be),
            5 => self.s5(&mut m, s, be),
            _ => self.s6(&mut m, s, be),
        };
        m.turn += 1;
        self.mem.insert(stage, m);
        out
    }

    fn otp_prompt(&self, be: &Backend, kind: &str) -> String {
        let masked = if kind == "mobile" { be.s.mobile.as_deref().map(mask_mobile) } else { be.s.email.as_deref().map(mask_email) };
        btn(&format!("Please enter the 6-digit OTP sent to {}.", masked.unwrap_or_default()), &["RESEND OTP", "EXIT"])
    }

    fn otp_fail(&self, r: &Value, be: &Backend, kind: &str) -> String {
        match code(r) {
            "INVALID_OTP" => btn(&format!("The OTP does not match. {} attempt(s) left. Please enter the 6-digit OTP again.", r["attempts_left"]), &["RESEND OTP", "EXIT"]),
            "OTP_ATTEMPTS_EXCEEDED" => btn("Too many wrong attempts, so the OTP is locked. Please resend the OTP.", &["RESEND OTP", "EXIT"]),
            _ => self.otp_prompt(be, kind),
        }
    }

    // ---- stage 1 ---------------------------------------------------------------
    fn s1(&mut self, m: &mut Mem, s: &str, be: &mut Backend) -> String {
        let u = s.to_uppercase();
        if m.turn == 0 {
            return stages::stage(1).welcome;
        }
        if be.s.mobile.is_none() || (!be.s.mobile_verified && be.s.mobile.is_none()) {
            let r = be.call("store_user_detail", &json!({"field_name": "mobile", "value": s}));
            if !succ(&r) {
                return "That mobile number is not valid. Please enter customer's 10-digit mobile number".into();
            }
            be.call("send_otp", &json!({"otp_type": "mobile", "recipient": s}));
            return self.otp_prompt(be, "mobile");
        }
        if !be.s.mobile_verified {
            if u == "RESEND OTP" {
                let n = be.s.mobile.clone().unwrap();
                be.call("send_otp", &json!({"otp_type": "mobile", "recipient": n}));
                return self.otp_prompt(be, "mobile");
            }
            let r = be.call("verify_otp", &json!({"otp": s, "otp_type": "mobile"}));
            return if succ(&r) { "Please enter the customer's full name.".into() } else { self.otp_fail(&r, be, "mobile") };
        }
        if be.s.name.is_none() {
            let r = be.call("store_user_detail", &json!({"field_name": "name", "value": s}));
            return if succ(&r) { "Please enter the customer's email address.".into() } else { "That name does not look valid. Please enter the customer's full name.".into() };
        }
        if be.s.email.is_none() {
            let r = be.call("store_user_detail", &json!({"field_name": "email", "value": s}));
            if !succ(&r) {
                return "That email address does not look valid. Please enter the customer's email address.".into();
            }
            be.call("send_otp", &json!({"otp_type": "email", "recipient": s}));
            return self.otp_prompt(be, "email");
        }
        if !be.s.email_verified {
            if u == "RESEND OTP" {
                let e = be.s.email.clone().unwrap();
                be.call("send_otp", &json!({"otp_type": "email", "recipient": e}));
                return self.otp_prompt(be, "email");
            }
            let r = be.call("verify_otp", &json!({"otp": s, "otp_type": "email"}));
            if !succ(&r) {
                return self.otp_fail(&r, be, "email");
            }
            let g = be.call("get_collection_status", &json!({"trigger": "trigger"}));
            return if succ(&g) { btn("All onboarding details collected.", &["PROCEED", "EXIT"]) } else { format!("Some details are still missing: {}", msg(&g)) };
        }
        btn("All onboarding details collected.", &["PROCEED", "EXIT"])
    }

    // ---- stage 2 ---------------------------------------------------------------
    fn s2(&mut self, m: &mut Mem, s: &str, be: &mut Backend) -> String {
        let u = s.to_uppercase();
        if m.turn == 0 {
            return stages::stage(2).welcome;
        }
        if s.starts_with("[DOCUMENT SUMMARY]") {
            if s.contains("Status: COMPLETE") {
                let r = be.call("collect_identity_documents", &json!({"trigger": "trigger"}));
                return if succ(&r) { btn("Identity documents stored. Proceed to Aadhaar OTP eKYC?", &["PROCEED", "EXIT"]) } else { msg(&r).to_string() };
            }
            let action = s.split("Action:").nth(1).unwrap_or(" Please upload clear Aadhaar and PAN images.").trim();
            return format!("The upload could not be accepted. {action}");
        }
        if be.s.identity_stored && !be.s.aadhaar_otp_sent || (be.s.aadhaar_otp_sent && u == "RESEND OTP" && !be.s.aadhaar_verified) {
            let r = be.call("generate_aadhar_otp", &json!({"trigger": "trigger"}));
            return btn(&format!("OTP sent successfully for Aadhaar {}. Please enter the OTP received by the customer.", strv(&r, "masked_aadhaar")), &["RESEND OTP", "EXIT"]);
        }
        if be.s.aadhaar_otp_sent && !be.s.aadhaar_verified {
            let r = be.call("verify_aadhar_otp", &json!({"otp": s}));
            if succ(&r) {
                return btn("Aadhaar Card Exists.\nDo you give consent to verify the PAN-Aadhaar link?", &["YES", "EXIT"]);
            }
            return match code(&r) {
                "INVALID_OTP" => btn(&format!("The OTP does not match. {} attempt(s) left. Please enter the Aadhaar OTP again.", r["attempts_left"]), &["RESEND OTP", "EXIT"]),
                _ => btn("Too many wrong attempts, so the OTP is locked. Please resend the OTP.", &["RESEND OTP", "EXIT"]),
            };
        }
        if be.s.aadhaar_verified && !be.s.pan_verified {
            if u == "YES" {
                let r = be.call("store_numbers", &json!({"trigger": "trigger"}));
                if succ(&r) {
                    return btn(msg(&r), &["PROCEED", "EXIT"]);
                }
                return btn(&format!("{} The application cannot continue until this is resolved.", msg(&r)), &["EXIT"]);
            }
            return btn("The PAN-Aadhaar link check is mandatory. Do you give consent to verify the PAN-Aadhaar link?", &["YES", "EXIT"]);
        }
        if be.s.identity_stored && !be.s.pan_verified {
            return "Please upload Aadhaar and PAN images.".into();
        }
        btn("KYC is complete.", &["PROCEED", "EXIT"])
    }

    // ---- stage 3 ---------------------------------------------------------------
    fn s3(&mut self, m: &mut Mem, s: &str, be: &mut Backend) -> String {
        let u = s.to_uppercase();
        if m.turn == 0 {
            return btn(&stages::stage(3).welcome, &["USE AADHAAR", "OVERRIDE", "EXIT"]);
        }
        let appt_prompt = "Please provide the customer's appointment ID (mandatory for doorstep/agent valuation).";
        if be.s.address.is_none() {
            if u == "USE AADHAAR" {
                let r = be.call("confirm_address", &json!({"action": "use_aadhaar"}));
                return format!("Address confirmed from Aadhaar: {}\n{appt_prompt}", strv(&r, "address"));
            }
            if u == "OVERRIDE" {
                return "Please upload the address proof.".into();
            }
            if s.starts_with("[DOCUMENT SUMMARY]") && s.contains("Status: COMPLETE") {
                let r = be.call("confirm_address", &json!({"action": "override"}));
                return format!("Address confirmed from the uploaded proof: {}\n{appt_prompt}", strv(&r, "address"));
            }
            if s.starts_with("[DOCUMENT SUMMARY]") {
                return "The address proof could not be read. Please upload a clearer address proof.".into();
            }
            return btn("Confirm address from Aadhaar, or override with proof upload.", &["USE AADHAAR", "OVERRIDE", "EXIT"]);
        }
        if be.s.appointment_id.is_none() {
            if m.appt.is_none() {
                m.appt = Some(s.to_string());
                return "Please provide the customer's preferred language.".into();
            }
            let r = be.call("confirm_address", &json!({"action": "set_appointment", "appointment_id": m.appt, "preferred_language": s}));
            if !succ(&r) {
                m.appt = None;
                return if code(&r) == "INVALID_LANGUAGE" {
                    format!("That language is not supported. {appt_prompt}")
                } else {
                    format!("That appointment ID is not valid. {appt_prompt}")
                };
            }
            let d = be.call("run_dedupe", &json!({"role": "borrower"}));
            return if succ(&d) {
                "Borrower dedupe clear.\nPlease provide the customer's nominee full name.".into()
            } else {
                btn("An existing customer record matches this borrower, so the application cannot continue.", &["EXIT"])
            };
        }
        if m.nominee_name.is_none() {
            m.nominee_name = Some(s.to_string());
            return "Please provide the nominee's 10-digit phone number.".into();
        }
        if m.nominee_phone.is_none() {
            m.nominee_phone = Some(s.to_string());
            return "Please provide the nominee's relation (spouse/child/parent/sibling/other).".into();
        }
        let r = be.call("store_nominee", &json!({"name": m.nominee_name, "phone": m.nominee_phone, "relation": s}));
        if !succ(&r) {
            return match code(&r) {
                "INVALID_PHONE" => {
                    m.nominee_phone = None;
                    "That phone number is not valid. Please provide the nominee's 10-digit phone number.".into()
                }
                "INVALID_RELATION" => "That relation is not valid. Please provide the nominee's relation (spouse/child/parent/sibling/other).".into(),
                _ => {
                    m.nominee_name = None;
                    m.nominee_phone = None;
                    "That nominee name is not valid. Please provide the customer's nominee full name.".into()
                }
            };
        }
        let d = be.call("run_dedupe", &json!({"role": "nominee"}));
        if succ(&d) {
            return btn("Nominee dedupe clear. Profile can proceed.", &["PROCEED", "EXIT"]);
        }
        m.nominee_name = None;
        m.nominee_phone = None;
        "This nominee matches an existing record. Please provide a different nominee's full name.".into()
    }

    // ---- stage 4 ---------------------------------------------------------------
    fn s4(&mut self, m: &mut Mem, s: &str, be: &mut Backend) -> String {
        let u = s.to_uppercase();
        if m.turn == 0 {
            return stages::stage(4).welcome;
        }
        if s.starts_with("[GOLD INGEST SUMMARY]") {
            if s.contains("Status: PASS") {
                let c = be.call("collect_gold_item", &json!({"trigger": "trigger"}));
                if !succ(&c) {
                    return format!("{} Please upload a different ornament or continue without it.", msg(&c));
                }
                let v = be.call("value_gold", &json!({"trigger": "value"}));
                return btn(&format!(
                    "Item value: Rs {iv} Total gold valuation: Rs {tv} Max eligible loan: Rs {ml}\n\n| Description | Value |\n|---|---|\n| Item Value | Rs {iv} |\n| Total Gold Valuation | Rs {tv} |\n| Max Eligible Loan | Rs {ml} |\n\nDoes the customer have another ornament?",
                    iv = money(num(&v, "item_value")), tv = money(num(&v, "total_gold_valuation")), ml = money(num(&v, "max_eligible_loan"))
                ), &["YES", "NO", "EXIT"]);
            }
            let action = s.split("Action:").nth(1).unwrap_or(" Please upload a clear scale photo and certificate.").trim();
            return format!("The upload could not be accepted. {action}");
        }
        if u == "YES" {
            return "Please upload the next ornament's photo on the weighing scale and its purchase bill or valuation certificate.".into();
        }
        if u == "NO" {
            let p = be.call("check_pledge", &json!({"trigger": "check"}));
            if !succ(&p) {
                return btn(&format!("{} The application cannot continue.", msg(&p)), &["EXIT"]);
            }
            let f = be.call("finalize_gold", &json!({"trigger": "finalize"}));
            let (n, tv, ml) = (be.s.gold_items.len(), be.s.total_gold_value, be.s.max_loan);
            let _ = f;
            return btn(&format!(
                "Gold stage complete. Items={n}, value=Rs {tv}, max loan=Rs {ml}.\n\n| Description | Value |\n|---|---|\n| Total Items | {n} |\n| Total Gold Valuation | Rs {tv} |\n| Max Eligible Loan | Rs {ml} |",
                tv = money(tv), ml = money(ml)
            ), &["PROCEED", "EXIT"]);
        }
        btn("Does the customer have another ornament?", &["YES", "NO", "EXIT"])
    }

    // ---- stage 5 ---------------------------------------------------------------
    fn quote_reply(&self, plan: &Value, first: bool) -> String {
        let (emi_label, emi) = if plan.get("monthly_emi").is_some() { ("Monthly EMI", num(plan, "monthly_emi")) } else { ("Monthly Interest", num(plan, "monthly_interest")) };
        let head = if first { "The customer's plan is as follows:" } else { "Here is the updated plan:" };
        btn(&format!(
            "{head}\n\n| Plan Details | Value |\n|---|---|\n| Loan Amount | Rs {} |\n| ROI | {}% |\n| Tenure | {} months |\n| {emi_label} | Rs {} |\n| APR | ~{}% |\n| LTV | {}% |",
            money(num(plan, "loan_amount")), num(plan, "roi"), plan["tenure_months"], money(emi), num(plan, "apr"), num(plan, "ltv")
        ), &["CONFIRM", "CHANGE", "EXIT"])
    }

    fn confirm_reply(&self, r: &Value) -> String {
        match code(r) {
            "INCOME_REQUIRED" => btn("The loan amount exceeds Rs 250,000, so income capture is required. Please provide the customer's employment type.", &["SALARIED", "SELF-EMPLOYED", "EXIT"]),
            "BUREAU_REQUIRED" => "The system requires a bureau check before the final confirmation. I will fetch the customer's bureau score.".into(),
            "ELIGIBILITY_REQUIRED" => "The system requires an eligibility check before the final confirmation. I will check the customer's eligibility.".into(),
            _ if succ(r) => btn(&format!("The offer has been confirmed at the full eligible amount of Rs {}.", money(num(r, "loan_amount"))), &["PROCEED", "EXIT"]),
            _ => format!("The offer could not be confirmed: {}", msg(r)),
        }
    }

    fn s5(&mut self, m: &mut Mem, s: &str, be: &mut Backend) -> String {
        let u = s.to_uppercase();
        if m.turn == 0 {
            let r = be.call("get_offers", &json!({"trigger": "trigger"}));
            let mut t = String::from("Available schemes:\n\n| Offer ID | Scheme Type | ROI | Tenure | Repayment |\n|---|---|---|---|---|\n");
            for o in r["offers"].as_array().cloned().unwrap_or_default() {
                t += &format!("| {} | {} | {:.1}% | {}m | {} |\n", strv(&o, "offer_id"), strv(&o, "scheme_type"), num(&o, "roi"), o["tenure_months"], strv(&o, "repayment"));
            }
            t += "\nWhich offer did the customer select?";
            return t;
        }
        if s.starts_with("[PAYSLIP SUMMARY]") {
            if s.contains("Status: PASS") {
                be.call("collect_payslips", &json!({"trigger": "trigger"}));
                let r = be.call("calculate_offer", &json!({"action": "confirm", "tenure_months": m.tenure}));
                return self.confirm_reply(&r);
            }
            let action = s.split("Action:").nth(1).unwrap_or(" Please upload corrected payslips.").trim();
            return format!("The payslips could not be accepted. {action}");
        }
        if u == "SALARIED" || u == "SELF-EMPLOYED" {
            let et = if u == "SALARIED" { "salaried" } else { "self_employed" };
            let r = be.call("capture_income", &json!({"employment_type": et}));
            return if succ(&r) {
                "Employment type salaried saved. Please upload the customer's last 3 consecutive monthly payslips.".into()
            } else {
                btn("Self-employed applications must be handled at the branch (manual underwriting).", &["EXIT"])
            };
        }
        if u == "CONFIRM" && be.s.selected_offer.is_some() {
            let r = be.call("calculate_offer", &json!({"action": "confirm", "tenure_months": m.tenure}));
            return self.confirm_reply(&r);
        }
        if u == "CHANGE" {
            m.awaiting_change = true;
            return "Would the customer like a different scheme or a lower ROI?".into();
        }
        if u == "PROCEED" {
            if be.s.bureau_score.is_none() && be.s.selected_offer.is_some() {
                let r = be.call("get_bureau_score", &json!({"trigger": "trigger"}));
                if !succ(&r) {
                    return btn(&format!("{} The application cannot continue.", msg(&r)), &["EXIT"]);
                }
                if r["eligible_for_loan"].as_bool() != Some(true) {
                    return btn(&format!("The customer is not eligible because of the bureau score. {}", msg(&r)), &["EXIT"]);
                }
                return format!("The customer's CIBIL score is {} ({}). I am now checking the final loan eligibility.", r["cibil_score"], strv(&r, "score_tier"));
            }
            if !be.s.eligibility_ok && be.s.bureau_score.is_some() {
                let r = be.call("check_loan_eligibility", &json!({"trigger": "trigger"}));
                return if succ(&r) {
                    format!("Eligibility confirmed for Rs {}. I am now finalizing the offer.", money(be.s.eligible_amount))
                } else {
                    btn(&format!("The customer is not eligible for this loan: {}", msg(&r)), &["EXIT"])
                };
            }
            if be.s.eligibility_ok && !be.s.offer_confirmed {
                let r = be.call("calculate_offer", &json!({"action": "confirm", "tenure_months": m.tenure}));
                return self.confirm_reply(&r);
            }
        }
        if m.awaiting_change {
            if let Some(c) = Regex::new(r"(\d+(?:\.\d+)?)\s*%?").unwrap().captures(s) {
                if !u.contains("GL_") {
                    let roi: f64 = c[1].parse().unwrap_or(0.0);
                    let r = be.call("calculate_offer", &json!({"action": "negotiate", "tenure_months": m.tenure, "requested_roi": roi}));
                    if succ(&r) {
                        m.awaiting_change = false;
                        return self.quote_reply(&r, false);
                    }
                    return btn(&format!("{} Would the customer like to continue with the counter offer?", msg(&r)), &["CONFIRM", "CHANGE", "EXIT"]);
                }
            }
        }
        if let Some(sch) = be.rules.schemes.iter().find(|x| u.contains(&x.offer_id.to_uppercase())).cloned() {
            let r = be.call("select_scheme", &json!({"offer_id": sch.offer_id}));
            if !succ(&r) {
                return "That offer is not available. Which offer did the customer select?".into();
            }
            m.awaiting_change = false;
            m.tenure = sch.tenure_months.to_string();
            let q = be.call("calculate_offer", &json!({"action": "quote", "tenure_months": m.tenure}));
            return self.quote_reply(&q, true);
        }
        "Which offer did the customer select?".into()
    }

    // ---- stage 6 ---------------------------------------------------------------
    fn finish(&self, be: &mut Backend) -> String {
        let mode = be.s.disb_mode.clone().unwrap_or_default();
        be.call("allocate_and_disburse", &json!({"action": "allocate", "disbursement_mode": mode}));
        be.call("allocate_and_disburse", &json!({"action": "disburse", "disbursement_mode": mode}));
        let mut r = be.call("send_sanction_letter_email", &json!({"trigger": "trigger"}));
        if !succ(&r) {
            r = be.call("send_sanction_letter_email", &json!({"trigger": "trigger"}));
        }
        if succ(&r) { msg(&r).to_string() } else { "The sanction letter could not be emailed right now; it will be re-sent shortly.".to_string() }
    }

    fn s6(&mut self, m: &mut Mem, s: &str, be: &mut Backend) -> String {
        let u = s.to_uppercase();
        if m.turn == 0 {
            return btn(&stages::stage(6).welcome, &["YES", "EXIT"]);
        }
        if !be.s.pd_done {
            if u == "YES" {
                be.call("run_pd_and_gdr", &json!({"action": "pd"}));
                let g = be.call("run_pd_and_gdr", &json!({"action": "gdr"}));
                return btn(&format!("{}\nPlease check the approval after the waiting period.", strv(&g, "gdr_number")), &["CHECK APPROVAL", "EXIT"]);
            }
            return btn("The PD checklist must be completed first. Has the Personal Discussion (PD) checklist been completed with the customer?", &["YES", "EXIT"]);
        }
        if !be.s.approved {
            if u == "CHECK APPROVAL" {
                let r = be.call("check_approval_status", &json!({"trigger": "trigger"}));
                if succ(&r) {
                    return btn("Approved.\nPlease ask the customer to acknowledge the Key Fact Statement (KFS).", &["ACCEPT", "DECLINE", "EXIT"]);
                }
                if code(&r) == "APPROVAL_PENDING" {
                    return btn(&format!("The cooling-off wait is not over yet ({}s remaining). Please check again shortly.", r["remaining_seconds"]), &["CHECK APPROVAL", "EXIT"]);
                }
                return btn("The application was not approved.", &["EXIT"]);
            }
            return btn("Please check the approval after the waiting period.", &["CHECK APPROVAL", "EXIT"]);
        }
        if be.s.kfs_accepted.is_none() {
            if u == "ACCEPT" {
                be.call("acknowledge_kfs", &json!({"action": "accept"}));
                let r = be.call("generate_esign_url", &json!({"trigger": "trigger"}));
                return btn(&format!("Please open this link to complete your e-signature: {}", strv(&r, "esign_url")), &["DONE", "EXIT"]);
            }
            if u == "DECLINE" {
                be.call("acknowledge_kfs", &json!({"action": "decline"}));
                return "The KFS was declined, so the application is closed.".into();
            }
            return btn("Please ask the customer to acknowledge the Key Fact Statement (KFS).", &["ACCEPT", "DECLINE", "EXIT"]);
        }
        if !be.s.esign_done {
            if u == "DONE" {
                let r = be.call("check_esign_status", &json!({"trigger": "trigger"}));
                if succ(&r) {
                    return btn("Document signed successfully.\nPlease provide the disbursement type (NEFT, IMPS, or CASH) and the disbursed account number if electronic.", &["PROCEED", "EXIT"]);
                }
                return btn("The signature is not complete yet. Please finish it using the link and confirm.", &["DONE", "EXIT"]);
            }
            return btn("Please complete the e-signature and confirm.", &["DONE", "EXIT"]);
        }
        if m.mode.is_none() {
            if ["NEFT", "IMPS", "CASH"].contains(&u.as_str()) {
                m.mode = Some(u.clone());
                be.s.disb_mode = Some(u.clone());
                if u == "CASH" {
                    return self.finish(be);
                }
                return format!("Please provide the disbursed account number for the {u} transfer.");
            }
            return "Please provide the disbursement type (NEFT, IMPS, or CASH).".into();
        }
        if m.acct.is_none() {
            let digits: String = s.chars().filter(|c| c.is_ascii_digit()).collect();
            let probe = be.call("allocate_and_disburse", &json!({"action": "allocate", "disbursement_mode": m.mode, "account_number": digits}));
            if !succ(&probe) {
                return format!("{} Please provide the disbursed account number for the {} transfer.", msg(&probe), m.mode.clone().unwrap_or_default());
            }
            m.acct = Some(digits.clone());
            be.call("allocate_and_disburse", &json!({"action": "disburse", "disbursement_mode": m.mode, "account_number": digits}));
            let mut r = be.call("send_sanction_letter_email", &json!({"trigger": "trigger"}));
            if !succ(&r) {
                r = be.call("send_sanction_letter_email", &json!({"trigger": "trigger"}));
            }
            return if succ(&r) { msg(&r).to_string() } else { "The sanction letter could not be emailed right now; it will be re-sent shortly.".to_string() };
        }
        "The application is complete.".into()
    }
}
