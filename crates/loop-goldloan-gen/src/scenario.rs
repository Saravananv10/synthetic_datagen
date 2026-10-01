//! A scenario = the ground truth about one customer + the scripted deviations
//! ("flags") the human simulator must exhibit. Everything the backend needs to
//! answer tool calls deterministically lives here (the agent never sees it).

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scenario {
    pub id: String,
    #[serde(default = "d_seed")]
    pub seed: u64,
    /// Epoch seconds at session start (drives GDR / txn ids and the virtual clock).
    #[serde(default = "d_epoch")]
    pub start_epoch: u64,
    /// "today" for payslip-age checks, `YYYY-MM`.
    #[serde(default = "d_month")]
    pub today_month: String,
    /// "operator" (loan officer typing for the customer) or "customer".
    #[serde(default = "d_role")]
    pub human_role: String,
    pub customer: Customer,
    pub profile: Profile,
    pub gold: GoldPlan,
    pub income: IncomePlan,
    pub bureau: BureauPlan,
    pub choices: Choices,
    #[serde(default)]
    pub flags: Flags,
    #[serde(default)]
    pub tool_failures: Vec<ToolFailure>,
    /// Free-text style hint for the LLM human ("terse", "chatty", "hinglish typos"...).
    #[serde(default)]
    pub style: String,
    /// Which coverage variant produced this scenario (metadata only).
    #[serde(default)]
    pub tags: Vec<String>,
    /// Who the simulated human is (drives the LLM human's voice and behaviour).
    #[serde(default)]
    pub persona: Option<PersonaCard>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct PersonaCard {
    pub age: u32,
    pub occupation: String,
    /// e.g. "hurried", "anxious first-time borrower", "chatty", "sceptical"
    pub temperament: String,
    /// One or two sentences of background the human can draw on.
    pub background: String,
    /// How they type: language mix, punctuation, typos.
    pub voice: String,
}

fn d_seed() -> u64 { 7 }
fn d_employer() -> String { "Acme Technologies Pvt Ltd".into() }
fn d_epoch() -> u64 { 1_790_147_000 }
fn d_month() -> String { "2026-09".into() }
fn d_role() -> String { "operator".into() }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Customer {
    pub mobile: String,
    pub name: String,
    pub email: String,
    pub aadhaar: String, // "8316 4683 3628"
    pub pan: String,     // "BDAPD1327A"
    pub address: String,
    pub pincode: String,
    #[serde(default)]
    pub dob: String,
    /// Father's name as printed on the PAN card (derived from the surname when empty).
    #[serde(default)]
    pub father: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Nominee {
    pub name: String,
    pub phone: String,
    pub relation: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub appointment_id: String,
    pub language: String,
    pub nominee: Nominee,
    /// Given by the human if the first nominee turns out to be a duplicate.
    #[serde(default)]
    pub nominee_alt: Option<Nominee>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldItemPlan {
    #[serde(rename = "type")]
    pub kind: String,
    pub description: String,
    pub gross_wt: f64,
    pub stone_deduction: f64,
    pub karat: f64,
    /// What the scale photo shows (differs from `gross_wt` for mismatch scenarios).
    pub scale_reading: f64,
    #[serde(default)]
    pub photo_fixture: String,
    #[serde(default)]
    pub doc_fixture: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldPlan {
    pub items: Vec<GoldItemPlan>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Payslip {
    pub month: String, // YYYY-MM
    pub net_pay: f64,
    pub name: String,
    #[serde(default)]
    pub fixture: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IncomePlan {
    /// "salaried" | "self_employed"
    pub employment_type: String,
    /// Employer printed on payslips (fictional).
    #[serde(default = "d_employer")]
    pub employer: String,
    #[serde(default)]
    pub payslips: Vec<Payslip>,
    /// Shown instead of `payslips` on the first upload attempt when `flags.payslips_bad_first`.
    #[serde(default)]
    pub payslips_bad: Vec<Payslip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BureauPlan {
    pub cibil: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choices {
    pub offer_id: String,
    /// Offer id picked first when `flags.change_scheme_once` (then `offer_id` is chosen).
    #[serde(default)]
    pub first_offer_id: Option<String>,
    pub disbursement_mode: String,
    #[serde(default)]
    pub account_number: String,
    #[serde(default)]
    pub ifsc: String,
    /// ROI the human asks for when `flags.negotiate_roi` is set.
    #[serde(default)]
    pub requested_roi: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Flags {
    pub mobile_invalid_first: bool,
    pub email_invalid_first: bool,
    pub wrong_mobile_otp_first: bool,
    pub wrong_email_otp_first: bool,
    pub wrong_aadhaar_otp_first: bool,
    pub identity_upload_incomplete_first: bool,
    pub pan_not_linked: bool,
    pub override_address: bool,
    pub invalid_appointment_first: bool,
    pub borrower_duplicate: bool,
    pub nominee_duplicate_first: bool,
    pub gold_weight_mismatch_first: bool,
    pub pledged: bool,
    pub change_scheme_once: bool,
    pub negotiate_roi: bool,
    pub payslips_bad_first: bool,
    /// First identity upload is scanned badly (PAN number unreadable, Aadhaar digit misread).
    pub ocr_noise_first: bool,
    pub approval_rejected: bool,
    pub check_approval_early: bool,
    pub esign_not_done_first: bool,
    pub decline_kfs: bool,
    /// Human types EXIT at the first opportunity in this stage (1..=6).
    pub exit_at_stage: Option<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolFailure {
    pub tool: String,
    #[serde(default = "one")]
    pub times: u32,
    pub message: String,
    #[serde(default = "rt")]
    pub error_type: String,
}
fn one() -> u32 { 1 }
fn rt() -> String { "RuntimeError".into() }

pub fn load_scenarios(path: &Path) -> anyhow::Result<Vec<Scenario>> {
    if path.is_dir() {
        let mut files: Vec<_> = std::fs::read_dir(path)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
            .collect();
        files.sort();
        files.iter().map(|p| load_one(p)).collect()
    } else {
        Ok(vec![load_one(path)?])
    }
}

fn load_one(p: &Path) -> anyhow::Result<Scenario> {
    let raw = std::fs::read_to_string(p)?;
    serde_json::from_str(&raw).map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))
}

impl Scenario {
    /// Structural checks so an unusable scenario fails fast instead of looping.
    pub fn validate(&self, rules: &crate::rules::Rules) -> anyhow::Result<()> {
        let mut errs: Vec<String> = vec![];
        if self.gold.items.is_empty() {
            errs.push("gold.items is empty".into());
        }
        let mode = self.choices.disbursement_mode.to_uppercase();
        if !rules.closing.disbursement_modes.contains(&mode) {
            errs.push(format!("unknown disbursement_mode {mode}"));
        }
        if mode != "CASH" && self.choices.account_number.is_empty() {
            errs.push("electronic disbursement needs choices.account_number".into());
        }
        if rules.scheme(&self.choices.offer_id).is_none() {
            errs.push(format!("unknown offer_id {}", self.choices.offer_id));
        }
        if self.flags.change_scheme_once {
            match &self.choices.first_offer_id {
                Some(f) if rules.scheme(f).is_some() && !f.eq_ignore_ascii_case(&self.choices.offer_id) => {}
                _ => errs.push("change_scheme_once needs a valid first_offer_id different from offer_id".into()),
            }
        }
        if self.flags.negotiate_roi && self.choices.requested_roi.is_none() {
            errs.push("negotiate_roi needs choices.requested_roi".into());
        }
        if self.flags.nominee_duplicate_first && self.profile.nominee_alt.is_none() {
            errs.push("nominee_duplicate_first needs profile.nominee_alt".into());
        }
        if self.income.employment_type == "salaried" && self.income.payslips.len() < rules.income.payslips_required {
            errs.push(format!("salaried scenario needs {} payslips", rules.income.payslips_required));
        }
        if errs.is_empty() { Ok(()) } else { anyhow::bail!("scenario {}: {}", self.id, errs.join("; ")) }
    }
}
