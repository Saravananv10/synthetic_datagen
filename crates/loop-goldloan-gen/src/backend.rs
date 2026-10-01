//! Deterministic, stateful mock of everything behind the agents' tools:
//! OTP provider, UIDAI, NSDL, dedupe, valuation, schemes, bureau, approval,
//! e-sign, disbursement, email. One `Backend` per conversation.
//!
//! Output strings/shapes follow the demo trace wherever it showed them; tool
//! *behaviours* the demo did not show (failure paths) are documented in
//! `goldloan-datagen/spec/business_rules.json` as assumed.

use std::collections::HashMap;

use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use regex::Regex;
use serde::Serialize;
use serde_json::{json, Map, Value};

use crate::rules::Rules;
use crate::scenario::{Nominee, Payslip, Scenario};

#[derive(Debug, Clone, Serialize)]
pub struct CallRecord {
    pub stage: u8,
    pub name: String,
    pub args: Value,
    pub output: Value,
    pub clock_s: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct GoldItem {
    pub kind: String,
    pub description: String,
    pub gross_wt: f64,
    pub deduction: f64,
    pub karat: f64,
    pub net_wt: f64,
    pub fine_wt: f64,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct IdentityData {
    pub aadhaar: String,
    pub pan: String,
    pub name: String,
    pub address: String,
    pub pincode: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct Session {
    pub clock_s: u64,
    // stage 1
    pub mobile: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub mobile_verified: bool,
    pub email_verified: bool,
    pub onboarding_complete: bool,
    // stage 2
    pub identity_ready: bool,
    pub identity_stored: bool,
    pub identity: Option<IdentityData>,
    pub aadhaar_otp_sent: bool,
    pub aadhaar_verified: bool,
    pub pan_verified: bool,
    // stage 3
    pub address: Option<String>,
    pub address_proof_ready: bool,
    pub address_proof_addr: Option<String>,
    pub appointment_id: Option<String>,
    pub language: Option<String>,
    pub borrower_dedupe: bool,
    pub nominee: Option<Nominee>,
    pub nominee_dedupe: bool,
    // stage 4
    pub pending_gold: Option<GoldItem>,
    pub gold_items: Vec<GoldItem>,
    pub gold_valued: bool,
    pub pledge_checked: bool,
    pub gold_finalized: bool,
    pub total_gold_value: f64,
    pub max_loan: f64,
    pub ltv_pct: f64,
    // stage 5
    pub selected_offer: Option<String>,
    pub roi_override: Option<f64>,
    pub employment_type: Option<String>,
    pub payslips_ready: Option<Vec<Payslip>>,
    pub payslips_ok: bool,
    pub avg_net_pay: f64,
    pub bureau_score: Option<u32>,
    pub eligibility_ok: bool,
    pub eligible_amount: f64,
    pub offer_confirmed: bool,
    pub loan_amount: f64,
    // stage 6
    pub pd_done: bool,
    pub gdr: Option<String>,
    pub gdr_at: Option<u64>,
    pub approved: bool,
    pub kfs_accepted: Option<bool>,
    pub esign_url: Option<String>,
    pub esign_done: bool,
    pub packet_id: Option<String>,
    pub disb_mode: Option<String>,
    pub disb_account: Option<String>,
    pub txn_id: Option<String>,
    pub sanction_sent: bool,
    pub sanction_ref: Option<String>,
    /// completed | declined | rejected | exited | handoff | timeout | error
    pub outcome: Option<String>,
}

pub struct Backend {
    pub rules: Rules,
    pub sc: Scenario,
    pub s: Session,
    pub stage: u8,
    pub log: Vec<CallRecord>,
    /// Device notifications (OTP SMS/email) not yet shown to the human.
    pub inbox: Vec<String>,
    rng: StdRng,
    otp: HashMap<String, String>,
    otp_attempts: HashMap<String, u32>,
    aadhaar_otp: Option<String>,
    aadhaar_attempts: u32,
    failures_left: HashMap<String, u32>,
    upload_attempts: HashMap<&'static str, u32>,
    nominee_dup_shown: bool,
    /// Tools that have succeeded so far (`calculate_offer:confirm` for the confirming call).
    ok_tools: std::collections::HashSet<String>,
    esign_checks: u32,
    esign_ref: String,
}

/// Phrases the agent may only say once the corresponding tool has succeeded
/// (mirrored by `CLAIMS` in goldloan_gen/verify.py).
pub const CLAIMS: &[(&str, &str)] = &[
    (r"Identity documents stored", "collect_identity_documents"),
    (r"OTP sent successfully for Aadhaar", "generate_aadhar_otp"),
    (r"Aadhaar Card Exists", "verify_aadhar_otp"),
    (r"verified and linked to Aadhaar", "store_numbers"),
    (r"All onboarding details collected", "get_collection_status"),
    (r"Address confirmed from", "confirm_address"),
    (r"Borrower dedupe clear", "run_dedupe"),
    (r"Nominee dedupe clear", "run_dedupe"),
    (r"Gold stage complete", "finalize_gold"),
    (r"Employment type salaried saved", "capture_income"),
    (r"CIBIL score is", "get_bureau_score"),
    (r"Eligibility confirmed", "check_loan_eligibility"),
    (r"The offer has been confirmed", "calculate_offer:confirm"),
    (r"^Approved\.", "check_approval_status"),
    (r"Please open this link to complete your e-signature", "generate_esign_url"),
    (r"Document signed successfully", "check_esign_status"),
    (r"sanction letter emailed", "send_sanction_letter_email"),
];

// ---- small helpers ----------------------------------------------------------

fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

fn ok(msg: impl Into<String>, extra: Value) -> Value {
    let mut m = Map::new();
    m.insert("success".into(), json!(true));
    if let Value::Object(e) = extra {
        for (k, v) in e {
            m.insert(k, v);
        }
    }
    m.insert("message".into(), json!(msg.into()));
    Value::Object(m)
}

fn err(code: &str, msg: impl Into<String>) -> Value {
    json!({"success": false, "error_code": code, "message": msg.into()})
}

fn s_arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get(k).and_then(|v| v.as_str()).map(|s| s.trim())
}

pub fn mask_mobile(m: &str) -> String {
    let d: Vec<char> = m.chars().collect();
    let keep = d.len().saturating_sub(4);
    format!("{}{}", "X".repeat(keep), d[keep..].iter().collect::<String>())
}

pub fn mask_email(e: &str) -> String {
    match e.split_once('@') {
        Some((u, d)) => format!("{}****@{}", u.chars().take(2).collect::<String>(), d),
        None => "****".into(),
    }
}

pub fn mask_pan(p: &str) -> String {
    p.chars()
        .enumerate()
        .map(|(i, c)| if i == 3 || i == 4 || i == 8 { 'X' } else { c })
        .collect()
}

pub fn mask_aadhaar(a: &str) -> String {
    let digits: String = a.chars().filter(|c| c.is_ascii_digit()).collect();
    let last = &digits[digits.len().saturating_sub(4)..];
    format!("XXXX-XXXX-{last}")
}

fn str_of(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.trim().is_empty() && !s.trim().eq_ignore_ascii_case("null") => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn digits_of(v: Option<&Value>) -> String {
    str_of(v).unwrap_or_default().chars().filter(|c| c.is_ascii_digit()).collect()
}

/// Lenient number: `93.58`, `"93.58"`, `"93.58 g"`, `"Rs 48,900"`.
fn num_of(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t: String = s.chars().filter(|c| c.is_ascii_digit() || *c == '.').collect();
            t.parse().ok()
        }
        _ => None,
    }
}

fn fmt_opt(v: Option<f64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "?".into())
}

fn norm_name(s: &str) -> String {
    s.to_uppercase().chars().map(|c| if c.is_alphabetic() { c } else { ' ' }).collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `2026-06`, `Jun 2026`, `June 2026`, `06/2026` -> `2026-06`.
fn parse_month(v: Option<&Value>) -> Option<String> {
    let s = str_of(v)?;
    if let Some((y, m)) = s.split_once('-') {
        if y.len() == 4 && m.parse::<u32>().is_ok() {
            return Some(format!("{y}-{:02}", m.parse::<u32>().ok()?));
        }
    }
    let lower = s.to_lowercase();
    let year: String = lower.chars().filter(|c| c.is_ascii_digit()).collect();
    const N: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];
    let mi = N.iter().position(|m| lower.contains(m))?;
    (year.len() == 4).then(|| format!("{year}-{:02}", mi + 1))
}

fn month_idx(m: &str) -> Option<i32> {
    let (y, mo) = m.split_once('-')?;
    Some(y.parse::<i32>().ok()? * 12 + mo.parse::<i32>().ok()? - 1)
}

fn month_name(m: &str) -> String {
    const N: [&str; 12] = [
        "January", "February", "March", "April", "May", "June", "July", "August", "September", "October",
        "November", "December",
    ];
    match m.split_once('-') {
        Some((y, mo)) => {
            let i: usize = mo.parse().unwrap_or(1);
            format!("{} {}", N[(i.max(1) - 1).min(11)], y)
        }
        None => m.to_string(),
    }
}

impl Backend {
    pub fn new(rules: Rules, sc: Scenario) -> Backend {
        let mut failures_left = HashMap::new();
        for f in &sc.tool_failures {
            *failures_left.entry(f.tool.clone()).or_insert(0) += f.times;
        }
        let mut rng = StdRng::seed_from_u64(sc.seed);
        let esign_ref = format!("eKYC_URL_REF{:06}", rng.gen_range(0..1_000_000));
        Backend {
            rules,
            s: Session::default(),
            stage: 1,
            log: vec![],
            inbox: vec![],
            rng,
            otp: HashMap::new(),
            otp_attempts: HashMap::new(),
            aadhaar_otp: None,
            aadhaar_attempts: 0,
            failures_left,
            upload_attempts: HashMap::new(),
            nominee_dup_shown: false,
            ok_tools: Default::default(),
            esign_checks: 0,
            esign_ref,
            sc,
        }
    }

    pub fn advance(&mut self, secs: u64) {
        self.s.clock_s += secs;
    }

    pub fn epoch(&self) -> u64 {
        self.sc.start_epoch + self.s.clock_s
    }

    fn new_otp(&mut self) -> String {
        format!("{:0width$}", self.rng.gen_range(0..10u64.pow(self.rules.otp.length as u32)), width = self.rules.otp.length)
    }

    /// Whether the given stage's completion condition is met.
    pub fn stage_done(&self, stage: u8) -> bool {
        let s = &self.s;
        match stage {
            1 => s.onboarding_complete,
            2 => s.pan_verified,
            3 => s.nominee_dedupe,
            4 => s.gold_finalized,
            5 => s.offer_confirmed,
            6 => s.sanction_sent,
            _ => false,
        }
    }

    // ---- upload-ingest middleware ------------------------------------------------
    //
    // The OCR/vision step (GPT or deterministic) returns raw extracted fields. This
    // middleware validates them, decides a status, stores what was *extracted* (not
    // the hidden truth) and writes the summary the stage agent receives.

    fn bump(&mut self, k: &'static str) -> u32 {
        let e = self.upload_attempts.entry(k).or_insert(0);
        let v = *e;
        *e += 1;
        v
    }

    /// Number of uploads of this kind so far (0 on the first).
    pub fn attempt(&mut self, kind: &'static str) -> u32 {
        self.bump(kind)
    }

    pub fn ingest_identity_from(&mut self, front: &Value, back: &Value, pan: Option<&Value>) -> String {
        let a_front = digits_of(front.get("aadhaar_number"));
        let a_back = digits_of(back.get("aadhaar_number"));
        let name_f = str_of(front.get("name")).unwrap_or_default();
        let pan_no = pan.and_then(|p| str_of(p.get("pan_number"))).map(|x| x.replace(' ', "").to_uppercase());
        let name_p = pan.and_then(|p| str_of(p.get("name")));
        let addr = str_of(back.get("address")).unwrap_or_default();
        let pin = digits_of(back.get("pincode"));
        let aadhaar_ok = a_front.len() == 12 && a_front == a_back && !a_front.starts_with(['0', '1']);
        let pan_ok = pan_no.as_deref().map(|p| Regex::new(r"^[A-Z]{5}[0-9]{4}[A-Z]$").unwrap().is_match(p)).unwrap_or(false);
        let stored_name = self.s.name.clone().unwrap_or_else(|| self.sc.customer.name.clone());
        let names_ok = norm_name(&name_f) == norm_name(&stored_name)
            && name_p.as_deref().map(|n| norm_name(n) == norm_name(&name_f)).unwrap_or(true);
        let (status, action) = if pan_no.is_none() {
            ("INCOMPLETE", "PAN card is missing or unreadable. Ask the user to upload a clear PAN image.")
        } else if !aadhaar_ok || !pan_ok || addr.is_empty() || pin.len() != 6 {
            ("UNREADABLE", "One or more identity fields could not be read reliably (Aadhaar number, PAN or address). Ask the user to re-upload clear images.")
        } else if !names_ok {
            ("MISMATCH", "The name on the documents does not match the customer name or each other. Ask the user to upload the correct documents.")
        } else {
            ("COMPLETE", "Both Aadhaar and PAN are confirmed. Call collect_identity_documents(trigger='trigger') immediately.")
        };
        self.s.identity_ready = status == "COMPLETE";
        if self.s.identity_ready {
            self.s.identity = Some(IdentityData { aadhaar: a_front.clone(), pan: pan_no.clone().unwrap_or_default(), name: name_f.clone(), address: addr.clone(), pincode: pin.clone() });
        }
        let addr_short: String = addr.chars().take(11).collect();
        format!(
            "[DOCUMENT SUMMARY] Aadhaar: {} PAN: {} Name: {} Address: {} Pincode: {} Status: {status} Action: {action}",
            if aadhaar_ok || a_front.len() == 12 { format!("Detected (masked: {})", mask_aadhaar(&a_front)) } else { "Not detected".into() },
            match &pan_no { Some(p) if pan_ok => format!("Detected (masked: {})", mask_pan(p)), Some(_) => "Unreadable".into(), None => "Not detected".into() },
            if name_f.is_empty() { "Not detected".into() } else { name_f },
            if addr.is_empty() { "Not detected".into() } else { format!("Detected (masked: {addr_short}...- {pin})") },
            pin
        )
    }

    pub fn ingest_address_proof_from(&mut self, doc: &Value) -> String {
        let name = str_of(doc.get("name")).unwrap_or_default();
        let addr = str_of(doc.get("address")).unwrap_or_default();
        let pin = digits_of(doc.get("pincode"));
        let stored_name = self.s.name.clone().unwrap_or_else(|| self.sc.customer.name.clone());
        let ok = !addr.is_empty() && pin.len() == 6 && norm_name(&name) == norm_name(&stored_name);
        self.s.address_proof_ready = ok;
        if ok {
            self.s.address_proof_addr = Some(addr.clone());
        }
        format!(
            "[DOCUMENT SUMMARY] Address proof: {} Name: {} Address: {} Pincode: {} Status: {} Action: {}",
            if addr.is_empty() { "Not detected" } else { "Detected" }, name, addr, pin,
            if ok { "COMPLETE" } else { "INCOMPLETE" },
            if ok { "Call confirm_address(action='override')." } else { "The proof is unreadable or the name does not match. Ask the user to upload a clearer address proof." }
        )
    }

    pub fn ingest_gold_from(&mut self, photo: &Value, cert: &Value) -> String {
        let kind = str_of(cert.get("ornament_type")).or_else(|| str_of(photo.get("ornament_type"))).map(|k| k.to_lowercase());
        let photo_kind = str_of(photo.get("ornament_type")).map(|k| k.to_lowercase());
        let (gross, ded, karat, scale) = (num_of(cert.get("gross_weight_g")), num_of(cert.get("stone_deduction_g")), num_of(cert.get("purity_karat")), num_of(photo.get("scale_reading_g")));
        let net = num_of(cert.get("net_weight_g"));
        let desc = str_of(cert.get("item_description")).unwrap_or_default();
        let tol = self.rules.gold.weight_match_tolerance_g;
        let (Some(kind), Some(gross), Some(ded), Some(karat), Some(scale)) = (kind.clone(), gross, ded, karat, scale) else {
            self.s.pending_gold = None;
            return format!(
                "[GOLD INGEST SUMMARY] Status: UNREADABLE Type: {} Gross wt: {} Purity: {} Deduction: {} Vision: type={} wt={} Doc: type={} wt={} purity={} Note: ocr_incomplete Action: Some fields could not be read. Ask the user to upload a clearer scale photo and certificate.",
                kind.clone().unwrap_or_else(|| "unknown".into()), fmt_opt(gross), fmt_opt(karat), fmt_opt(ded),
                photo_kind.clone().unwrap_or_else(|| "unknown".into()), fmt_opt(scale),
                str_of(cert.get("ornament_type")).unwrap_or_else(|| "unknown".into()), fmt_opt(gross), fmt_opt(karat)
            );
        };
        let mismatch = (scale - gross).abs() > tol;
        let inconsistent = net.map(|n| (gross - ded - n).abs() > 0.06).unwrap_or(false);
        let (status, note, action) = if mismatch {
            ("WEIGHT_MISMATCH", "match_fail", format!("Scale reading {scale} g does not match certificate {gross} g. Ask the user to upload a clear scale photo."))
        } else if inconsistent {
            ("CERT_INCONSISTENT", "cert_math_fail", "The certificate's gross, deduction and net weights do not add up. Ask the user for a correct certificate.".to_string())
        } else {
            ("PASS", "match_ok", "Call collect_gold_item(trigger='trigger') then value_gold(trigger='value').".to_string())
        };
        if status == "PASS" {
            let net_wt = gross - ded;
            let fine = net_wt * karat / 22.0;
            self.s.pending_gold = Some(GoldItem {
                kind: kind.clone(), description: if desc.is_empty() { kind.clone() } else { desc },
                gross_wt: gross, deduction: ded, karat, net_wt, fine_wt: fine, value: fine * self.rules.gold.rate_per_22k_gram,
            });
        } else {
            self.s.pending_gold = None;
        }
        format!(
            "[GOLD INGEST SUMMARY] Status: {status} Type: {kind} Gross wt: {gross} Purity: {karat:.1} Deduction: {ded} Vision: type={} wt={scale} \
             Doc: type={kind} wt={gross} purity={} Note: bedrock_ok,gemma_ok,regex_merge,ocr_ok,doc_stored,{note} Action: {action}",
            photo_kind.unwrap_or_else(|| kind.clone()), karat as i64
        )
    }

    pub fn ingest_payslips_from(&mut self, slips: &[Value]) -> String {
        let set: Vec<Payslip> = slips
            .iter()
            .map(|v| Payslip {
                month: parse_month(v.get("month")).unwrap_or_default(),
                net_pay: num_of(v.get("net_pay")).unwrap_or(0.0),
                name: str_of(v.get("employee_name")).unwrap_or_default(),
                fixture: String::new(),
            })
            .collect();
        let req = self.rules.income.payslips_required;
        let mut idxs: Vec<i32> = set.iter().filter_map(|p| month_idx(&p.month)).collect();
        idxs.sort();
        let parsed_all = idxs.len() == set.len();
        let consecutive = parsed_all && idxs.windows(2).all(|w| w[1] - w[0] == 1);
        let today = month_idx(&self.sc.today_month).unwrap_or(0);
        let newest = idxs.last().copied().unwrap_or(-1000);
        let date_ok = parsed_all && newest <= today && today - newest <= self.rules.income.max_payslip_age_months;
        let stored = self.s.name.clone().unwrap_or_else(|| self.sc.customer.name.clone());
        let name_ok = set.iter().all(|p| norm_name(&p.name) == norm_name(&stored));
        let count_ok = set.len() == req;
        let pass = count_ok && consecutive && date_ok && name_ok && set.iter().all(|p| p.net_pay > 0.0);
        let mut sorted = set.clone();
        sorted.sort_by_key(|p| month_idx(&p.month).unwrap_or(0));
        let months = sorted.iter().map(|p| month_name(&p.month)).collect::<Vec<_>>().join(", ");
        let mut why = vec![];
        if !count_ok { why.push(format!("{} of {} payslips", set.len(), req)); }
        if !consecutive { why.push("months not consecutive".into()); }
        if !date_ok { why.push("older than 6 months".into()); }
        if !name_ok { why.push("name mismatch".into()); }
        self.s.payslips_ready = if pass { Some(set.clone()) } else { None };
        format!(
            "[PAYSLIP SUMMARY] Payslips: {}/{} Months: {} Consecutive: {} Date range: {} Name match: {} Status: {} Action: {}",
            set.len(), req, months,
            if consecutive { "Yes" } else { "No" },
            if date_ok { "Valid" } else { "Expired" },
            if name_ok { "Yes" } else { "No" },
            if pass { "PASS" } else { "FAIL" },
            if pass { "Call collect_payslips(trigger='trigger').".to_string() }
            else { format!("Ask the user to upload corrected payslips ({}).", why.join("; ")) }
        )
    }

    /// Tools the given agent text talks about as done, but which have not succeeded yet.
    pub fn unbacked_claims(&self, text: &str) -> Vec<String> {
        CLAIMS
            .iter()
            .filter(|(pat, tool)| Regex::new(pat).unwrap().is_match(text) && !self.ok_tools.contains(*tool))
            .map(|(_, tool)| tool.to_string())
            .collect()
    }

    // ---- tool dispatch -----------------------------------------------------------

    pub fn call(&mut self, name: &str, args: &Value) -> Value {
        let out = if let Some(f) = self.injected_failure(name) { f } else { self.dispatch(name, args) };
        if out.get("success").and_then(|v| v.as_bool()) == Some(true) {
            self.ok_tools.insert(name.to_string());
            if name == "calculate_offer" && args.get("action").and_then(|v| v.as_str()) == Some("confirm") {
                self.ok_tools.insert("calculate_offer:confirm".into());
            }
        }
        self.log.push(CallRecord { stage: self.stage, name: name.to_string(), args: args.clone(), output: out.clone(), clock_s: self.s.clock_s });
        out
    }

    fn injected_failure(&mut self, name: &str) -> Option<Value> {
        let left = self.failures_left.get_mut(name)?;
        if *left == 0 {
            return None;
        }
        *left -= 1;
        let f = self.sc.tool_failures.iter().find(|f| f.tool == name)?;
        Some(json!({
            "success": false,
            "error": f.message,
            "error_type": f.error_type,
            "message": f.message,
        }))
    }

    fn dispatch(&mut self, name: &str, a: &Value) -> Value {
        match name {
            "store_user_detail" => self.store_user_detail(a),
            "send_otp" => self.send_otp(a),
            "verify_otp" => self.verify_otp(a),
            "get_collection_status" => self.get_collection_status(),
            "collect_identity_documents" => self.collect_identity_documents(),
            "generate_aadhar_otp" => self.generate_aadhar_otp(),
            "verify_aadhar_otp" => self.verify_aadhar_otp(a),
            "store_numbers" => self.store_numbers(),
            "confirm_address" => self.confirm_address(a),
            "run_dedupe" => self.run_dedupe(a),
            "store_nominee" => self.store_nominee(a),
            "collect_gold_item" => self.collect_gold_item(),
            "value_gold" => self.value_gold(),
            "check_pledge" => self.check_pledge(),
            "finalize_gold" => self.finalize_gold(),
            "get_offers" => self.get_offers(),
            "select_scheme" => self.select_scheme(a),
            "calculate_offer" => self.calculate_offer(a),
            "capture_income" => self.capture_income(a),
            "collect_payslips" => self.collect_payslips(),
            "get_bureau_score" => self.get_bureau_score(),
            "check_loan_eligibility" => self.check_loan_eligibility(),
            "run_pd_and_gdr" => self.run_pd_and_gdr(a),
            "check_approval_status" => self.check_approval_status(),
            "acknowledge_kfs" => self.acknowledge_kfs(a),
            "generate_esign_url" => self.generate_esign_url(),
            "check_esign_status" => self.check_esign_status(),
            "allocate_and_disburse" => self.allocate_and_disburse(a),
            "send_sanction_letter_email" => self.send_sanction_letter_email(),
            other => err("UNKNOWN_TOOL", format!("Unknown tool {other}")),
        }
    }

    // ---- stage 1 -----------------------------------------------------------------

    fn store_user_detail(&mut self, a: &Value) -> Value {
        let (Some(field), Some(value)) = (s_arg(a, "field_name"), s_arg(a, "value")) else {
            return err("MISSING_ARGUMENT", "field_name and value are required.");
        };
        match field {
            "mobile" => {
                if !Regex::new(r"^[6-9][0-9]{9}$").unwrap().is_match(value) {
                    return err("INVALID_MOBILE", "Mobile number must be exactly 10 digits starting with 6-9.");
                }
                self.s.mobile = Some(value.to_string());
                self.s.mobile_verified = false;
                ok(format!("Mobile stored as {}. Call send_otp next.", mask_mobile(value)), json!({}))
            }
            "name" => {
                if value.chars().filter(|c| c.is_alphabetic()).count() < 2 {
                    return err("INVALID_NAME", "Name must contain at least two letters.");
                }
                self.s.name = Some(value.to_string());
                ok(format!("Name stored for {value}."), json!({}))
            }
            "email" => {
                if !Regex::new(r"^[^@\s]+@[^@\s]+\.[^@\s]+$").unwrap().is_match(value) {
                    return err("INVALID_EMAIL", "That does not look like a valid email address.");
                }
                self.s.email = Some(value.to_string());
                self.s.email_verified = false;
                ok("Email stored (masked). Call send_otp for email.", json!({}))
            }
            _ => err("INVALID_FIELD", "field_name must be mobile, name or email."),
        }
    }

    fn send_otp(&mut self, a: &Value) -> Value {
        let Some(kind) = s_arg(a, "otp_type") else { return err("MISSING_ARGUMENT", "otp_type is required.") };
        let (stored, masked, chan) = match kind {
            "mobile" => (self.s.mobile.clone(), self.s.mobile.as_deref().map(mask_mobile), "SMS"),
            "email" => (self.s.email.clone(), self.s.email.as_deref().map(mask_email), "Email"),
            _ => return err("INVALID_OTP_TYPE", "otp_type must be mobile or email."),
        };
        let (Some(stored), Some(masked)) = (stored, masked) else {
            return err("FIELD_NOT_STORED", format!("Store the {kind} with store_user_detail before sending an OTP."));
        };
        if let Some(rcpt) = s_arg(a, "recipient") {
            if rcpt != stored {
                return err("RECIPIENT_MISMATCH", format!("Recipient does not match the stored {kind}."));
            }
        }
        let code = self.new_otp();
        self.otp.insert(kind.to_string(), code.clone());
        self.otp_attempts.insert(kind.to_string(), 0);
        self.inbox.push(format!("[DEVICE] {chan} to {masked}: Your Gold Loan OTP is {code}. Do not share it."));
        ok(format!("OTP sent to {masked}."), json!({"otp_type": kind}))
    }

    fn verify_otp(&mut self, a: &Value) -> Value {
        let (Some(kind), Some(otp)) = (s_arg(a, "otp_type"), s_arg(a, "otp")) else {
            return err("MISSING_ARGUMENT", "otp and otp_type are required.");
        };
        let Some(expected) = self.otp.get(kind).cloned() else {
            return err("OTP_NOT_SENT", format!("No OTP has been sent for {kind}. Call send_otp first."));
        };
        let attempts = self.otp_attempts.entry(kind.to_string()).or_insert(0);
        if *attempts >= self.rules.otp.max_attempts {
            return err("OTP_ATTEMPTS_EXCEEDED", "Too many wrong attempts. Resend the OTP.");
        }
        if otp != expected {
            *attempts += 1;
            let left = self.rules.otp.max_attempts - *attempts;
            return json!({"success": false, "error_code": "INVALID_OTP", "attempts_left": left,
                          "message": format!("OTP does not match. {left} attempt(s) left.")});
        }
        self.otp.remove(kind);
        match kind {
            "mobile" => self.s.mobile_verified = true,
            _ => self.s.email_verified = true,
        }
        let label = if kind == "mobile" { "Mobile" } else { "Email" };
        json!({"success": true, "otp_type": kind, "message": format!("{label} verified successfully.")})
    }

    fn get_collection_status(&mut self) -> Value {
        let s = &self.s;
        let mut missing = vec![];
        if s.mobile.is_none() { missing.push("mobile"); } else if !s.mobile_verified { missing.push("mobile_verification"); }
        if s.name.is_none() { missing.push("name"); }
        if s.email.is_none() { missing.push("email"); } else if !s.email_verified { missing.push("email_verification"); }
        if missing.is_empty() {
            self.s.onboarding_complete = true;
            ok("All onboarding parameters collected.", json!({"missing": []}))
        } else {
            json!({"success": false, "error_code": "INCOMPLETE_ONBOARDING", "missing": missing,
                   "message": format!("Missing: {}.", missing.join(", "))})
        }
    }

    // ---- stage 2 -----------------------------------------------------------------

    fn collect_identity_documents(&mut self) -> Value {
        if !self.s.identity_ready {
            return err("NO_IDENTITY_DOCUMENTS", "No complete Aadhaar+PAN upload found. Ask the user to upload both documents.");
        }
        self.s.identity_stored = true;
        ok("Identity documents stored. Proceed to Aadhaar OTP eKYC verification.", json!({}))
    }

    fn generate_aadhar_otp(&mut self) -> Value {
        if !self.s.identity_stored {
            return err("IDENTITY_NOT_STORED", "Store the identity documents first.");
        }
        let code = self.new_otp();
        self.aadhaar_otp = Some(code.clone());
        self.aadhaar_attempts = 0;
        self.s.aadhaar_otp_sent = true;
        let masked = mask_aadhaar(&self.s.identity.as_ref().map(|i| i.aadhaar.clone()).unwrap_or_default());
        self.inbox.push(format!("[DEVICE] SMS from UIDAI to the Aadhaar-linked mobile: Your Aadhaar OTP is {code}."));
        ok("OTP sent successfully", json!({"masked_aadhaar": masked}))
    }

    fn verify_aadhar_otp(&mut self, a: &Value) -> Value {
        let Some(otp) = s_arg(a, "otp") else { return err("MISSING_ARGUMENT", "otp is required.") };
        let Some(expected) = self.aadhaar_otp.clone() else {
            return err("OTP_NOT_SENT", "No Aadhaar OTP has been sent. Call generate_aadhar_otp first.");
        };
        if self.aadhaar_attempts >= self.rules.otp.max_attempts {
            return err("OTP_ATTEMPTS_EXCEEDED", "Too many wrong attempts. Resend the OTP.");
        }
        if otp != expected {
            self.aadhaar_attempts += 1;
            let left = self.rules.otp.max_attempts - self.aadhaar_attempts;
            return json!({"success": false, "error_code": "INVALID_OTP", "attempts_left": left,
                          "message": format!("OTP does not match. {left} attempt(s) left.")});
        }
        self.aadhaar_otp = None;
        self.s.aadhaar_verified = true;
        ok("Aadhaar eKYC verified. Ask consent to verify the PAN-Aadhaar link, then call store_numbers.", json!({}))
    }

    fn store_numbers(&mut self) -> Value {
        if !self.s.aadhaar_verified {
            return err("AADHAAR_NOT_VERIFIED", "Verify the Aadhaar OTP first.");
        }
        if self.sc.flags.pan_not_linked {
            self.s.outcome = Some("rejected".into());
            return err("PAN_NOT_LINKED", "PAN is not linked to Aadhaar. The application cannot proceed until they are linked.");
        }
        self.s.pan_verified = true;
        let masked = mask_pan(&self.s.identity.as_ref().map(|i| i.pan.clone()).unwrap_or_default());
        ok(format!("PAN {masked} verified and linked to Aadhaar. KYC complete. Proceeding to Next Step."),
           json!({"proceed": true, "masked_pan": masked}))
    }

    // ---- stage 3 -----------------------------------------------------------------

    fn confirm_address(&mut self, a: &Value) -> Value {
        match s_arg(a, "action") {
            Some("use_aadhaar") => {
                if !self.s.identity_stored { return err("IDENTITY_NOT_STORED", "Identity documents are not stored yet."); }
                let addr = self.s.identity.as_ref().map(|i| i.address.clone()).unwrap_or_default();
                self.s.address = Some(addr.clone());
                ok(format!("Address confirmed from Aadhaar. This is the address present on aadhaar: {addr}"), json!({"address": addr}))
            }
            Some("override") => {
                if !self.s.address_proof_ready { return err("NO_ADDRESS_PROOF", "No address proof upload found."); }
                let addr = self.s.address_proof_addr.clone().unwrap_or_default();
                self.s.address = Some(addr.clone());
                ok(format!("Address confirmed from uploaded proof: {addr}"), json!({"address": addr}))
            }
            Some("set_appointment") => {
                if self.s.address.is_none() { return err("ADDRESS_NOT_CONFIRMED", "Confirm the address first."); }
                let (Some(id), Some(lang)) = (s_arg(a, "appointment_id"), s_arg(a, "preferred_language")) else {
                    return err("MISSING_ARGUMENT", "appointment_id and preferred_language are required.");
                };
                if !Regex::new(&self.rules.appointments.valid_id_pattern).unwrap().is_match(id) {
                    return err("INVALID_APPOINTMENT", format!("Appointment id '{id}' is not valid."));
                }
                if !self.rules.languages.iter().any(|l| l.eq_ignore_ascii_case(lang)) {
                    return err("INVALID_LANGUAGE", format!("Language '{lang}' is not supported."));
                }
                self.s.appointment_id = Some(id.to_string());
                self.s.language = Some(lang.to_string());
                ok("Appointment and language saved. Call run_dedupe for borrower.", json!({}))
            }
            _ => err("INVALID_ACTION", "action must be use_aadhaar, override or set_appointment."),
        }
    }

    fn run_dedupe(&mut self, a: &Value) -> Value {
        match s_arg(a, "role") {
            Some("borrower") => {
                if self.s.appointment_id.is_none() { return err("APPOINTMENT_NOT_SAVED", "Save the appointment and language first."); }
                if self.sc.flags.borrower_duplicate {
                    self.s.outcome = Some("rejected".into());
                    return err("DUPLICATE_FOUND", "An existing customer record matches this borrower (same PAN). The application cannot continue.");
                }
                self.s.borrower_dedupe = true;
                ok("Borrower dedupe clear. Collect nominee name, phone, relation.", json!({}))
            }
            Some("nominee") => {
                let Some(n) = self.s.nominee.clone() else { return err("NOMINEE_NOT_STORED", "Store the nominee first."); };
                if self.sc.flags.nominee_duplicate_first && !self.nominee_dup_shown && n.phone == self.sc.profile.nominee.phone {
                    self.nominee_dup_shown = true;
                    return err("DUPLICATE_FOUND", "This nominee matches an existing record. Ask for a different nominee.");
                }
                self.s.nominee_dedupe = true;
                ok("Nominee dedupe clear. Profile can proceed.", json!({}))
            }
            _ => err("INVALID_ROLE", "role must be borrower or nominee."),
        }
    }

    fn store_nominee(&mut self, a: &Value) -> Value {
        let (Some(name), Some(phone), Some(rel)) = (s_arg(a, "name"), s_arg(a, "phone"), s_arg(a, "relation")) else {
            return err("MISSING_ARGUMENT", "name, phone and relation are required.");
        };
        if name.chars().filter(|c| c.is_alphabetic()).count() < 2 { return err("INVALID_NAME", "Nominee name is not valid."); }
        if !Regex::new(r"^[6-9][0-9]{9}$").unwrap().is_match(phone) { return err("INVALID_PHONE", "Nominee phone must be 10 digits starting with 6-9."); }
        let rel_l = rel.to_lowercase();
        if !self.rules.nominee_relations.contains(&rel_l) { return err("INVALID_RELATION", "Relation must be spouse, child, parent, sibling or other."); }
        self.s.nominee = Some(Nominee { name: name.to_string(), phone: phone.to_string(), relation: rel_l });
        self.s.nominee_dedupe = false;
        ok("Nominee stored (phone masked). Call run_dedupe(role='nominee').", json!({}))
    }

    // ---- stage 4 -----------------------------------------------------------------

    fn collect_gold_item(&mut self) -> Value {
        let Some(item) = self.s.pending_gold.take() else {
            return err("NO_GOLD_INGEST", "No passing gold upload found. Ask the user to upload the photo on the scale and the certificate.");
        };
        if item.karat < self.rules.gold.min_karat {
            return err("KARAT_TOO_LOW", format!("Purity {}K is below the minimum {}K.", item.karat, self.rules.gold.min_karat));
        }
        if self.s.gold_items.len() >= self.rules.gold.max_items {
            return err("ITEM_LIMIT_REACHED", format!("At most {} ornaments per application.", self.rules.gold.max_items));
        }
        self.s.gold_items.push(item);
        self.s.gold_valued = false;
        ok(format!("Gold item collected ({} total). Call value_gold(trigger='value').", self.s.gold_items.len()), json!({}))
    }

    fn value_gold(&mut self) -> Value {
        let Some(last) = self.s.gold_items.last().cloned() else {
            return err("NO_GOLD_ITEMS", "Collect a gold item first.");
        };
        let mut total = 0.0;
        for it in &mut self.s.gold_items {
            it.value = r2(it.fine_wt * self.rules.gold.rate_per_22k_gram);
            total += it.value;
        }
        let total = r2(total);
        let (max, ltv) = self.rules.max_loan(total);
        self.s.total_gold_value = total;
        self.s.max_loan = max;
        self.s.ltv_pct = ltv;
        self.s.gold_valued = true;
        let item_value = r2(last.fine_wt * self.rules.gold.rate_per_22k_gram);
        ok(
            format!("Item valued. Fine wt {:.4}g, item value Rs {item_value:.2}. Total gold value Rs {total:.2}. Max eligible loan Rs {max:.2}.", last.fine_wt),
            json!({"fine_weight_g": (last.fine_wt * 1e4).round() / 1e4, "item_value": item_value,
                   "total_gold_valuation": total, "max_eligible_loan": max, "ltv_cap_pct": ltv}),
        )
    }

    fn check_pledge(&mut self) -> Value {
        if !self.s.gold_valued { return err("GOLD_NOT_VALUED", "Value the gold first."); }
        if self.sc.flags.pledged {
            self.s.outcome = Some("rejected".into());
            return err("PLEDGE_FOUND", "This gold is already pledged with another lender.");
        }
        self.s.pledge_checked = true;
        ok("Pledge check clear. Call finalize_gold.", json!({}))
    }

    fn finalize_gold(&mut self) -> Value {
        if !self.s.pledge_checked { return err("PLEDGE_NOT_CHECKED", "Run check_pledge first."); }
        self.s.gold_finalized = true;
        ok(format!("Gold stage complete. Items={}, value=Rs {:.2}, max loan=Rs {:.2}.", self.s.gold_items.len(), self.s.total_gold_value, self.s.max_loan), json!({}))
    }

    // ---- stage 5 -----------------------------------------------------------------

    fn get_offers(&mut self) -> Value {
        if !self.s.gold_finalized { return err("GOLD_NOT_FINALIZED", "Finish the gold stage first."); }
        let offers: Vec<Value> = self.rules.schemes.iter().map(|s| json!({
            "offer_id": s.offer_id, "scheme_type": s.scheme_type, "roi": s.roi,
            "tenure_months": s.tenure_months, "repayment": s.repayment, "processing_fee_pct": s.processing_fee_pct})).collect();
        let msg = std::iter::once("Available schemes:".to_string())
            .chain(self.rules.schemes.iter().map(|s| format!("{}|{}|ROI {:.1}%|{}m|{}", s.offer_id, s.scheme_type, s.roi, s.tenure_months, s.repayment)))
            .collect::<Vec<_>>().join("\n");
        ok(msg, json!({"offers": offers}))
    }

    fn select_scheme(&mut self, a: &Value) -> Value {
        let Some(id) = s_arg(a, "offer_id") else { return err("MISSING_ARGUMENT", "offer_id is required.") };
        let Some(sch) = self.rules.scheme(id).cloned() else {
            return err("INVALID_OFFER", format!("Offer '{id}' is not available."));
        };
        self.s.selected_offer = Some(sch.offer_id.clone());
        self.s.roi_override = None;
        self.s.offer_confirmed = false;
        ok(
            format!("Scheme {} selected (ROI {:.1}%, {}m, {}). Max eligible Rs {:.2}.", sch.offer_id, sch.roi, sch.tenure_months, sch.repayment, self.s.max_loan),
            json!({"offer_id": sch.offer_id}),
        )
    }

    fn plan(&self, loan: f64) -> Value {
        let sch = self.rules.scheme(self.s.selected_offer.as_deref().unwrap_or("")).cloned().unwrap();
        let roi = self.s.roi_override.unwrap_or(sch.roi);
        let n = sch.tenure_months as f64;
        let r = roi / 1200.0;
        let (emi_key, emi, total_interest) = if sch.repayment.eq_ignore_ascii_case("Bullet") {
            ("monthly_interest", r2(loan * r), loan * r * n)
        } else {
            let e = loan * r / (1.0 - (1.0 + r).powf(-n));
            ("monthly_emi", r2(e), e * n - loan)
        };
        let fee = loan * sch.processing_fee_pct / 100.0;
        let apr = (total_interest + fee) / loan / (n / 12.0) * 100.0;
        let mut m = Map::new();
        m.insert("offer_id".into(), json!(sch.offer_id));
        m.insert("loan_amount".into(), json!(r2(loan)));
        m.insert("roi".into(), json!(roi));
        m.insert("tenure_months".into(), json!(sch.tenure_months));
        m.insert("repayment".into(), json!(sch.repayment));
        m.insert(emi_key.into(), json!(emi));
        m.insert("apr".into(), json!((apr * 1000.0).round() / 1000.0));
        m.insert("ltv".into(), json!(self.s.ltv_pct));
        m.insert("processing_fee".into(), json!(r2(fee)));
        Value::Object(m)
    }

    fn calculate_offer(&mut self, a: &Value) -> Value {
        let Some(action) = s_arg(a, "action") else { return err("MISSING_ARGUMENT", "action is required.") };
        let Some(sel) = self.s.selected_offer.clone() else { return err("NO_OFFER_SELECTED", "Select a scheme first."); };
        let sch = self.rules.scheme(&sel).cloned().unwrap();
        if let Some(t) = s_arg(a, "tenure_months") {
            if t.parse::<u32>().ok() != Some(sch.tenure_months) {
                return err("INVALID_TENURE", format!("Tenure {t} months is not available for {sel} ({}m).", sch.tenure_months));
            }
        }
        match action {
            "quote" => {
                let mut p = self.plan(self.s.max_loan);
                let msg = format!("Plan for {sel}: loan Rs {:.2} at {:.1}% for {}m.", self.s.max_loan, self.s.roi_override.unwrap_or(sch.roi), sch.tenure_months);
                let o = p.as_object_mut().unwrap();
                o.insert("success".into(), json!(true));
                o.insert("message".into(), json!(msg));
                p
            }
            "negotiate" => {
                if !self.rules.roi_negotiation.enabled { return err("NEGOTIATION_DISABLED", "ROI negotiation is not available."); }
                let Some(req) = a.get("requested_roi").and_then(|v| v.as_f64()) else { return err("MISSING_ARGUMENT", "requested_roi is required."); };
                let floor = sch.roi - self.rules.roi_negotiation.max_discount_pct_points;
                if req < floor - 1e-9 {
                    return json!({"success": false, "error_code": "NEGOTIATION_LIMIT", "counter_roi": floor,
                                  "message": format!("Requested ROI {req:.2}% is below the floor. Best available is {floor:.2}%.")});
                }
                self.s.roi_override = Some(req.min(sch.roi));
                let mut p = self.plan(self.s.max_loan);
                let o = p.as_object_mut().unwrap();
                o.insert("success".into(), json!(true));
                o.insert("message".into(), json!(format!("ROI revised to {:.2}%.", req.min(sch.roi))));
                p
            }
            "confirm" => {
                if self.s.max_loan > self.rules.income.income_required_above_loan && !self.s.payslips_ok {
                    return err("INCOME_REQUIRED", format!("Loan above Rs {:.0} requires income capture. Call capture_income(employment_type) then collect_payslips.", self.rules.income.income_required_above_loan));
                }
                if self.s.bureau_score.is_none() {
                    return err("BUREAU_REQUIRED", "Call get_bureau_score then check_loan_eligibility, then confirm again.");
                }
                if !self.s.eligibility_ok {
                    return err("ELIGIBILITY_REQUIRED", "Call check_loan_eligibility, then confirm again.");
                }
                self.s.offer_confirmed = true;
                self.s.loan_amount = self.s.eligible_amount;
                ok(format!("Offer confirmed at full eligible amount Rs {:.2}. Proceed to closing.", self.s.eligible_amount),
                   json!({"loan_amount": self.s.eligible_amount}))
            }
            _ => err("INVALID_ACTION", "action must be quote, negotiate or confirm."),
        }
    }

    fn capture_income(&mut self, a: &Value) -> Value {
        match s_arg(a, "employment_type") {
            Some("salaried") => {
                self.s.employment_type = Some("salaried".into());
                ok("Employment type salaried saved. Ask for the last 3 consecutive monthly payslips.", json!({"employment_type": "salaried"}))
            }
            Some("self_employed") => {
                if !self.rules.income.self_employed_supported {
                    self.s.outcome = Some("handoff".into());
                    return err("UNSUPPORTED_EMPLOYMENT_TYPE", "Self-employed applicants must be handled at the branch (manual underwriting).");
                }
                self.s.employment_type = Some("self_employed".into());
                ok("Employment type self_employed saved.", json!({"employment_type": "self_employed"}))
            }
            _ => err("INVALID_EMPLOYMENT_TYPE", "employment_type must be salaried or self_employed."),
        }
    }

    fn collect_payslips(&mut self) -> Value {
        if self.s.employment_type.as_deref() != Some("salaried") {
            return err("EMPLOYMENT_NOT_CAPTURED", "Capture the employment type first.");
        }
        let Some(set) = self.s.payslips_ready.clone() else {
            return err("NO_PAYSLIPS", "No passing payslip upload found. Ask for the last 3 consecutive payslips.");
        };
        let avg = set.iter().map(|p| p.net_pay).sum::<f64>() / set.len() as f64;
        self.s.avg_net_pay = avg;
        self.s.payslips_ok = true;
        ok(format!("Payslips collected. Employment=salaried. Avg net pay Rs {avg:.1}. Call calculate_offer(action='confirm')."),
           json!({"employment_type": "salaried", "payslip_count": set.len(), "avg_net_pay": avg}))
    }

    fn get_bureau_score(&mut self) -> Value {
        if self.s.selected_offer.is_none() { return err("NO_OFFER_SELECTED", "Select a scheme first."); }
        let score = self.sc.bureau.cibil;
        self.s.bureau_score = Some(score);
        let tier = self.rules.tier_for(score).to_string();
        let eligible = score >= self.rules.bureau.min_score_for_loan;
        if !eligible { self.s.outcome = Some("rejected".into()); }
        ok(
            if eligible { format!("CIBIL {score} ({tier}). Call check_loan_eligibility.") }
            else { format!("CIBIL {score} ({tier}) is below the minimum {}.", self.rules.bureau.min_score_for_loan) },
            json!({"eligible_for_loan": eligible, "cibil_score": score, "score_tier": tier}),
        )
    }

    fn check_loan_eligibility(&mut self) -> Value {
        let Some(score) = self.s.bureau_score else { return err("BUREAU_REQUIRED", "Call get_bureau_score first."); };
        if score < self.rules.bureau.min_score_for_loan {
            self.s.outcome = Some("rejected".into());
            return err("NOT_ELIGIBLE", format!("CIBIL {score} is below the minimum {}.", self.rules.bureau.min_score_for_loan));
        }
        if self.rules.income.foir_check_enabled && self.s.avg_net_pay > 0.0 {
            let emi = self.plan(self.s.max_loan);
            let e = emi.get("monthly_emi").or_else(|| emi.get("monthly_interest")).and_then(|v| v.as_f64()).unwrap_or(0.0);
            if e / self.s.avg_net_pay * 100.0 > self.rules.income.foir_max_pct {
                self.s.outcome = Some("rejected".into());
                return err("FOIR_EXCEEDED", "Monthly obligation exceeds the allowed share of income.");
            }
        }
        self.s.eligibility_ok = true;
        self.s.eligible_amount = self.s.max_loan;
        ok(format!("Eligibility OK for Rs {:.2} (max Rs {:.2}). Call calculate_offer(action='confirm').", self.s.max_loan, self.s.max_loan), json!({}))
    }

    // ---- stage 6 -----------------------------------------------------------------

    fn run_pd_and_gdr(&mut self, a: &Value) -> Value {
        match s_arg(a, "action") {
            Some("pd") => {
                if !self.s.offer_confirmed { return err("OFFER_NOT_CONFIRMED", "Confirm the offer first."); }
                self.s.pd_done = true;
                ok("PD recorded. Call run_pd_and_gdr(action='gdr') to issue pledge slip.", json!({}))
            }
            Some("gdr") => {
                if !self.s.pd_done { return err("PD_NOT_DONE", "Record the personal discussion first."); }
                let g = format!("GDR{}", self.epoch());
                self.s.gdr = Some(g.clone());
                self.s.gdr_at = Some(self.s.clock_s);
                ok(format!("{g} issued. Approval can be checked after the {}s waiting period.", self.rules.closing.cooling_off_seconds),
                   json!({"gdr_number": g, "cooling_off_seconds": self.rules.closing.cooling_off_seconds}))
            }
            _ => err("INVALID_ACTION", "action must be pd or gdr."),
        }
    }

    fn check_approval_status(&mut self) -> Value {
        let Some(at) = self.s.gdr_at else { return err("GDR_NOT_ISSUED", "Issue the GDR first."); };
        let waited = self.s.clock_s - at;
        if waited < self.rules.closing.cooling_off_seconds {
            let rem = self.rules.closing.cooling_off_seconds - waited;
            return json!({"success": false, "error_code": "APPROVAL_PENDING", "remaining_seconds": rem,
                          "message": format!("Approval pending. Cooling-off period has {rem}s remaining.")});
        }
        if self.sc.flags.approval_rejected {
            self.s.outcome = Some("rejected".into());
            return err("APPROVAL_REJECTED", "The application was not approved by the credit team.");
        }
        self.s.approved = true;
        ok("Approved after waiting period. Acknowledge KFS next.", json!({}))
    }

    fn acknowledge_kfs(&mut self, a: &Value) -> Value {
        if !self.s.approved { return err("NOT_APPROVED", "Approval is required before the KFS."); }
        match s_arg(a, "action") {
            Some("accept") => { self.s.kfs_accepted = Some(true); ok("KFS acknowledged. Generate eSign URL.", json!({})) }
            Some("decline") => {
                self.s.kfs_accepted = Some(false);
                self.s.outcome = Some("declined".into());
                ok("KFS declined. Application closed.", json!({}))
            }
            _ => err("INVALID_ACTION", "action must be accept or decline."),
        }
    }

    fn generate_esign_url(&mut self) -> Value {
        if self.s.kfs_accepted != Some(true) { return err("KFS_NOT_ACCEPTED", "The KFS must be accepted first."); }
        let url = format!("http://esign.mock.local:8086/consent_page_for_ekyc?client_id={}", self.esign_ref);
        self.s.esign_url = Some(url.clone());
        ok(format!("Please open this link to complete your e-signature: {url}"), json!({"esign_url": url}))
    }

    fn check_esign_status(&mut self) -> Value {
        if self.s.esign_url.is_none() { return err("ESIGN_NOT_GENERATED", "Generate the e-sign link first."); }
        self.esign_checks += 1;
        if self.sc.flags.esign_not_done_first && self.esign_checks == 1 {
            return json!({"success": false, "esign_completed": false, "error_code": "ESIGN_PENDING",
                          "message": "The customer has not completed the e-signature yet."});
        }
        self.s.esign_done = true;
        ok("Document signed successfully. Collect disbursement type and account (if electronic), then allocate.", json!({"esign_completed": true}))
    }

    fn allocate_and_disburse(&mut self, a: &Value) -> Value {
        if !self.s.esign_done { return err("ESIGN_NOT_DONE", "The e-sign must be completed first."); }
        let mode = s_arg(a, "disbursement_mode").unwrap_or("").to_uppercase();
        if !self.rules.closing.disbursement_modes.contains(&mode) {
            return err("INVALID_MODE", "disbursement_mode must be NEFT, IMPS or CASH.");
        }
        let electronic = mode != "CASH";
        let acct = s_arg(a, "account_number").unwrap_or("");
        if electronic {
            if !Regex::new(r"^[0-9]{9,18}$").unwrap().is_match(acct) { return err("INVALID_ACCOUNT", "Account number must be 9 to 18 digits."); }
            if let Some(ifsc) = s_arg(a, "ifsc") {
                if !ifsc.is_empty() && !Regex::new(r"^[A-Z]{4}0[A-Z0-9]{6}$").unwrap().is_match(ifsc) { return err("INVALID_IFSC", "IFSC must look like ABCD0123456."); }
            }
        }
        match s_arg(a, "action") {
            Some("allocate") => {
                let p = format!("{}_{}", self.rng.gen_range(1..4), self.rng.gen_range(1..6));
                self.s.packet_id = Some(p.clone());
                self.s.disb_mode = Some(mode);
                if electronic { self.s.disb_account = Some(acct.to_string()); }
                ok(format!("Packet {p} allocated. Call disburse next."), json!({"packet_id": p}))
            }
            Some("disburse") => {
                if self.s.packet_id.is_none() { return err("PACKET_NOT_ALLOCATED", "Allocate a packet first."); }
                let txn = format!("TXN{}", self.epoch());
                self.s.txn_id = Some(txn.clone());
                self.s.disb_mode = Some(mode.clone());
                if electronic { self.s.disb_account = Some(acct.to_string()); }
                let masked = if electronic { mask_mobile(acct) } else { "N/A".into() };
                ok(format!("{mode} disbursement recorded (soft allocation). Txn {txn}."),
                   json!({"txn_id": txn, "disbursement_mode": mode, "disbursed_account_number_masked": masked, "loan_amount": self.s.loan_amount}))
            }
            _ => err("INVALID_ACTION", "action must be allocate or disburse."),
        }
    }

    fn send_sanction_letter_email(&mut self) -> Value {
        let (Some(txn), Some(packet), Some(mode)) = (self.s.txn_id.clone(), self.s.packet_id.clone(), self.s.disb_mode.clone()) else {
            return err("NOT_DISBURSED", "Disburse first.");
        };
        let email = self.s.email.clone().unwrap_or_default();
        let masked_acct = self.s.disb_account.as_deref().map(mask_mobile).unwrap_or_else(|| "N/A".into());
        self.s.sanction_sent = true;
        self.s.sanction_ref = Some(self.esign_ref.clone());
        self.s.outcome = Some("completed".into());
        ok(
            format!("Gold loan sanction letter emailed to {}. Ref: {}. Packet {packet}. Txn {txn}. Mode {mode}. Account {masked_acct}.", mask_email(&email), self.esign_ref),
            json!({"email_sent": true, "disbursement_mode": mode, "disbursed_account_number_masked": masked_acct, "loan_type": "gold_loan"}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOLDEN: &str = include_str!("../../../goldloan-datagen/scenarios/golden_demo.json");

    fn be() -> Backend {
        Backend::new(Rules::load(None).unwrap(), serde_json::from_str(GOLDEN).unwrap())
    }

    fn upload_gold(b: &mut Backend) -> String {
        let sc = b.sc.clone();
        let (idx, a) = (b.s.gold_items.len(), b.attempt("gold"));
        let d = crate::docs::gold_docs(&sc, idx, a).unwrap();
        b.ingest_gold_from(&d[0].fields, &d[1].fields)
    }

    fn upload_payslips(b: &mut Backend) -> String {
        let sc = b.sc.clone();
        let d = crate::docs::payslip_docs(&sc, &sc.income.payslips);
        let f: Vec<Value> = d.iter().map(|x| x.fields.clone()).collect();
        b.ingest_payslips_from(&f)
    }

    /// Drive the backend up to and including gold finalisation, exactly as in the demo.
    fn through_gold(b: &mut Backend) {
        b.s.identity_ready = true;
        b.s.identity_stored = true;
        b.s.name = Some("Venu Gopal Reddy".into());
        let sum = upload_gold(b);
        assert!(sum.contains("Status: PASS"));
        assert!(b.call("collect_gold_item", &json!({"trigger": "trigger"}))["success"].as_bool().unwrap());
        b.call("value_gold", &json!({"trigger": "value"}));
        b.call("check_pledge", &json!({"trigger": "check"}));
        b.call("finalize_gold", &json!({"trigger": "finalize"}));
    }

    #[test]
    fn valuation_matches_the_demo_exactly() {
        let mut b = be();
        b.s.identity_stored = true;
        upload_gold(&mut b);
        b.call("collect_gold_item", &json!({"trigger": "trigger"}));
        let v = b.call("value_gold", &json!({"trigger": "value"}));
        assert_eq!(v["fine_weight_g"], json!(83.1636));
        assert_eq!(v["item_value"], json!(498981.82));
        assert_eq!(v["max_eligible_loan"], json!(399185.46));
        assert_eq!(v["ltv_cap_pct"], json!(80.0));
    }

    #[test]
    fn emi_and_apr_match_the_demo() {
        let mut b = be();
        through_gold(&mut b);
        b.call("get_offers", &json!({"trigger": "trigger"}));
        b.call("select_scheme", &json!({"offer_id": "GL_REV_EMI_12"}));
        let q = b.call("calculate_offer", &json!({"action": "quote", "tenure_months": "12"}));
        assert_eq!(q["loan_amount"], json!(399185.46));
        assert_eq!(q["monthly_emi"], json!(35280.67));
        assert_eq!(q["apr"], json!(7.058));
        assert_eq!(q["roi"], json!(11.0));
    }

    #[test]
    fn confirm_gates_fire_in_the_demos_order() {
        let mut b = be();
        through_gold(&mut b);
        b.call("select_scheme", &json!({"offer_id": "GL_REV_EMI_12"}));
        let c = json!({"action": "confirm", "tenure_months": "12"});
        assert_eq!(b.call("calculate_offer", &c)["error_code"], "INCOME_REQUIRED");
        b.call("capture_income", &json!({"employment_type": "salaried"}));
        let sum = upload_payslips(&mut b);
        assert!(sum.contains("Status: PASS") && sum.contains("Payslips: 3/3"));
        let p = b.call("collect_payslips", &json!({"trigger": "trigger"}));
        assert_eq!(p["avg_net_pay"], json!(48900.0));
        assert_eq!(b.call("calculate_offer", &c)["error_code"], "BUREAU_REQUIRED");
        let br = b.call("get_bureau_score", &json!({"trigger": "trigger"}));
        assert_eq!((br["cibil_score"].as_u64(), br["score_tier"].as_str()), (Some(742), Some("Good")));
        assert_eq!(b.call("calculate_offer", &c)["error_code"], "ELIGIBILITY_REQUIRED");
        assert!(b.call("check_loan_eligibility", &json!({"trigger": "trigger"}))["success"].as_bool().unwrap());
        let ok = b.call("calculate_offer", &c);
        assert_eq!(ok["loan_amount"], json!(399185.46));
    }

    fn identity_docs_for(b: &mut Backend, attempt: u32) -> String {
        let sc = b.sc.clone();
        let d = crate::docs::identity_docs(&sc, attempt);
        let by = |k: &str| d.iter().find(|x| x.kind == k).map(|x| x.fields.clone());
        b.s.name = Some(sc.customer.name.clone());
        b.ingest_identity_from(&by("aadhaar_front").unwrap(), &by("aadhaar_back").unwrap(), by("pan").as_ref())
    }

    #[test]
    fn identity_middleware_matches_demo_summary_and_catches_bad_scans() {
        let mut b = be();
        let s = identity_docs_for(&mut b, 0);
        assert!(s.contains("Aadhaar: Detected (masked: XXXX-XXXX-3628)") && s.contains("PAN: Detected (masked: BDAXX132XA)") && s.contains("Status: COMPLETE"), "{s}");
        assert_eq!(b.s.identity.as_ref().unwrap().pan, "BDAPD1327A");

        // noisy first scan: PAN letter read as 0 and an Aadhaar digit misread -> not accepted
        let mut sc: Scenario = serde_json::from_str(GOLDEN).unwrap();
        sc.flags.ocr_noise_first = true;
        let mut b = Backend::new(Rules::load(None).unwrap(), sc);
        let s = identity_docs_for(&mut b, 0);
        assert!(s.contains("Status: UNREADABLE") && !b.s.identity_ready, "{s}");
        assert_eq!(b.call("collect_identity_documents", &json!({"trigger": "trigger"}))["error_code"], "NO_IDENTITY_DOCUMENTS");
        let s = identity_docs_for(&mut b, 1);
        assert!(s.contains("Status: COMPLETE"), "{s}");

        // missing PAN on first upload
        let mut sc: Scenario = serde_json::from_str(GOLDEN).unwrap();
        sc.flags.identity_upload_incomplete_first = true;
        let mut b = Backend::new(Rules::load(None).unwrap(), sc);
        assert!(identity_docs_for(&mut b, 0).contains("Status: INCOMPLETE"));
    }

    #[test]
    fn gold_and_payslip_middleware_tolerate_messy_llm_json() {
        let mut b = be();
        b.s.name = Some("Venu Gopal Reddy".into());
        // numbers as strings with units, Title-case type: what an LLM extractor plausibly returns
        let s = b.ingest_gold_from(
            &json!({"ornament_type": "Necklace", "scale_reading_g": "93.58 g"}),
            &json!({"ornament_type": "necklace", "item_description": "Hallmarked gold necklace", "gross_weight_g": "93.58", "stone_deduction_g": 2.1, "net_weight_g": "91.48", "purity_karat": "20"}),
        );
        assert!(s.contains("Status: PASS"), "{s}");
        // weights disagree -> mismatch, nothing stored
        let s = b.ingest_gold_from(
            &json!({"ornament_type": "necklace", "scale_reading_g": 97.9}),
            &json!({"ornament_type": "necklace", "gross_weight_g": 93.58, "stone_deduction_g": 2.1, "net_weight_g": 91.48, "purity_karat": 20}),
        );
        assert!(s.contains("WEIGHT_MISMATCH") && b.s.pending_gold.is_none());
        // unreadable field
        assert!(b.ingest_gold_from(&json!({"ornament_type": "necklace", "scale_reading_g": null}), &json!({})).contains("Status: UNREADABLE"));
        // payslip months in several spellings
        let s = b.ingest_payslips_from(&[
            json!({"employee_name": "VENU GOPAL REDDY", "month": "April 2026", "net_pay": "48,900"}),
            json!({"employee_name": "Venu Gopal Reddy", "month": "2026-05", "net_pay": 48900}),
            json!({"employee_name": "Venu Gopal Reddy", "month": "Jun 2026", "net_pay": 48900.0}),
        ]);
        assert!(s.contains("Status: PASS") && s.contains("Payslips: 3/3"), "{s}");
    }

    #[test]
    fn masking_matches_the_demo() {
        assert_eq!(mask_mobile("9876543210"), "XXXXXX3210");
        assert_eq!(mask_pan("BDAPD1327A"), "BDAXX132XA");
        assert_eq!(mask_aadhaar("8316 4683 3628"), "XXXX-XXXX-3628");
        assert_eq!(mask_mobile("12345679452"), "XXXXXXX9452");
    }

    #[test]
    fn otp_locks_after_max_attempts() {
        let mut b = be();
        b.call("store_user_detail", &json!({"field_name": "mobile", "value": "9876543210"}));
        b.call("send_otp", &json!({"otp_type": "mobile", "recipient": "9876543210"}));
        for left in [2, 1, 0] {
            let r = b.call("verify_otp", &json!({"otp": "000000", "otp_type": "mobile"}));
            assert_eq!(r["error_code"], "INVALID_OTP");
            assert_eq!(r["attempts_left"], json!(left));
        }
        assert_eq!(b.call("verify_otp", &json!({"otp": "000000", "otp_type": "mobile"}))["error_code"], "OTP_ATTEMPTS_EXCEEDED");
    }

    #[test]
    fn cooling_off_is_enforced_on_the_virtual_clock() {
        let mut b = be();
        b.s.offer_confirmed = true;
        b.call("run_pd_and_gdr", &json!({"action": "pd"}));
        b.call("run_pd_and_gdr", &json!({"action": "gdr"}));
        b.advance(8);
        let early = b.call("check_approval_status", &json!({"trigger": "trigger"}));
        assert_eq!(early["error_code"], "APPROVAL_PENDING");
        assert_eq!(early["remaining_seconds"], json!(52));
        b.advance(52);
        assert!(b.call("check_approval_status", &json!({"trigger": "trigger"}))["success"].as_bool().unwrap());
    }

    #[test]
    fn same_seed_gives_the_same_otps() {
        let otp = |seed: u64| {
            let mut sc: Scenario = serde_json::from_str(GOLDEN).unwrap();
            sc.seed = seed;
            let mut b = Backend::new(Rules::load(None).unwrap(), sc);
            b.call("store_user_detail", &json!({"field_name": "mobile", "value": "9876543210"}));
            b.call("send_otp", &json!({"otp_type": "mobile"}));
            b.inbox.pop().unwrap()
        };
        assert_eq!(otp(5), otp(5));
        assert_ne!(otp(5), otp(6));
    }
}
