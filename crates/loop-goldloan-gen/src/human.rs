//! The "human" side. `Scripted` is a deterministic policy that answers exactly
//! what the agent asked (used for golden traces / tests); `Llm` role-plays the
//! same persona card with a language model.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use loop_agent::harness::{
    create_session_repository, create_sqlite_session_store, AgentHarness, AgentHarnessOptions, AgentHarnessResources,
    ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_ai::Models;
use regex::Regex;

use crate::langfuse::{Langfuse, ModelCall};
use crate::lf_capture::Recorder;
use crate::model_util::{assistant_text, provider_error, resolve_model_spec};
use crate::scenario::Scenario;
use crate::stages::HUMAN_SIM;

pub struct HumanView<'a> {
    pub stage: u8,
    pub turn_in_stage: u32,
    pub agent_text: &'a str,
    pub options: Option<&'a [String]>,
    pub device: Vec<String>,
    pub items_collected: usize,
}

pub enum Human {
    Scripted(ScriptedHuman),
    Llm(LlmHuman),
}

impl Human {
    pub async fn reply(&mut self, v: &HumanView<'_>) -> anyhow::Result<String> {
        match self {
            Human::Scripted(h) => Ok(h.reply(v)),
            Human::Llm(h) => h.reply(v).await,
        }
    }
}

// ---------------------------------------------------------------------------

pub struct ScriptedHuman {
    sc: Scenario,
    used: HashSet<&'static str>,
    otp_by_chan: HashMap<&'static str, String>,
    expect_chan: &'static str,
    nominee_dup_seen: bool,
}

impl ScriptedHuman {
    pub fn new(sc: &Scenario) -> Self {
        ScriptedHuman { sc: sc.clone(), used: HashSet::new(), otp_by_chan: HashMap::new(), expect_chan: "mobile", nominee_dup_seen: false }
    }

    fn once(&mut self, k: &'static str) -> bool {
        self.used.insert(k)
    }

    fn reply(&mut self, v: &HumanView<'_>) -> String {
        let f = self.sc.flags.clone();
        let t = v.agent_text.to_lowercase();
        let opts: Vec<&str> = v.options.map(|o| o.iter().map(|s| s.as_str()).collect()).unwrap_or_default();
        let has = |o: &str| opts.contains(&o);

        if f.exit_at_stage == Some(v.stage) && v.turn_in_stage >= 2 && self.once("exit") {
            return "EXIT".into();
        }
        for d in &v.device {
            if let Some(c) = Regex::new(r"OTP is (\d+)").unwrap().captures(d) {
                let chan = if d.contains("Aadhaar") { "aadhaar" } else if d.contains("Email to") { "email" } else { "mobile" };
                self.otp_by_chan.insert(chan, c[1].to_string());
                self.expect_chan = chan;
            }
        }
        if t.contains("locked") && has("RESEND OTP") {
            return "RESEND OTP".into();
        }
        if t.contains("otp") && t.contains("enter") && !t.contains("locked") {
            if let Some(code) = self.otp_by_chan.get(self.expect_chan).cloned() {
                let wrong = match self.expect_chan {
                    "mobile" => f.wrong_mobile_otp_first,
                    "email" => f.wrong_email_otp_first,
                    _ => f.wrong_aadhaar_otp_first,
                };
                let key: &'static str = match self.expect_chan { "mobile" => "wrong_m", "email" => "wrong_e", _ => "wrong_a" };
                if wrong && self.once(key) {
                    let last = code.chars().last().unwrap().to_digit(10).unwrap();
                    return format!("{}{}", &code[..code.len() - 1], (last + 1) % 10);
                }
                return code;
            }
        }
        let c = self.sc.customer.clone();
        let p = self.sc.profile.clone();
        // --- free-text prompts (checked before buttons: some come with a PROCEED button) ---
        if t.contains("nominee") && t.contains("full name") {
            if t.contains("different") || t.contains("matches an existing") {
                self.nominee_dup_seen = true;
            }
            return match (&p.nominee_alt, self.nominee_dup_seen) {
                (Some(a), true) => a.name.clone(),
                _ => p.nominee.name.clone(),
            };
        }
        if t.contains("nominee") && t.contains("phone") {
            return match (&p.nominee_alt, self.nominee_dup_seen) {
                (Some(a), true) => a.phone.clone(),
                _ => p.nominee.phone.clone(),
            };
        }
        if t.contains("nominee") && t.contains("relation") {
            let r = match (&p.nominee_alt, self.nominee_dup_seen) {
                (Some(a), true) => a.relation.clone(),
                _ => p.nominee.relation.clone(),
            };
            return cap(&r);
        }
        if t.contains("disbursement type") {
            return self.sc.choices.disbursement_mode.clone();
        }
        if t.contains("account number") && t.contains("transfer") {
            return self.sc.choices.account_number.clone();
        }
        if t.contains("customer's full name") {
            return c.name.clone();
        }
        if t.contains("10-digit mobile number") {
            if f.mobile_invalid_first && self.once("bad_mobile") {
                return c.mobile[..c.mobile.len() - 1].to_string();
            }
            return c.mobile.clone();
        }
        if t.contains("email address") {
            if f.email_invalid_first && self.once("bad_email") {
                return c.email.replace('@', "");
            }
            return c.email.clone();
        }
        if t.contains("appointment id") {
            if f.invalid_appointment_first && self.once("bad_appt") {
                return "APX".into();
            }
            return p.appointment_id.clone();
        }
        if t.contains("preferred language") {
            return p.language.clone();
        }
        if t.contains("different scheme") && t.contains("lower roi") {
            if f.negotiate_roi {
                if let Some(r) = self.sc.choices.requested_roi {
                    return format!("Can we get the ROI down to {r}%?");
                }
            }
            return self.sc.choices.offer_id.clone();
        }
        if t.contains("which offer") {
            if f.change_scheme_once && self.once("first_pick") {
                return self.sc.choices.first_offer_id.clone().unwrap_or_else(|| "GL_FULL_EMI_12".into());
            }
            return self.sc.choices.offer_id.clone();
        }
        if v.options.is_none() && t.contains("upload") {
            return "UPLOAD".into();
        }
        // --- buttons ---
        if has("USE AADHAAR") {
            return if f.override_address { "OVERRIDE".into() } else { "USE AADHAAR".into() };
        }
        if has("CONFIRM") && has("CHANGE") {
            if f.change_scheme_once && self.once("change") {
                return "CHANGE".into();
            }
            if f.negotiate_roi && self.once("neg") {
                return "CHANGE".into();
            }
            return "CONFIRM".into();
        }
        if has("SALARIED") {
            return if self.sc.income.employment_type == "salaried" { "SALARIED".into() } else { "SELF-EMPLOYED".into() };
        }
        if has("YES") && has("NO") {
            return if v.items_collected < self.sc.gold.items.len() { "YES".into() } else { "NO".into() };
        }
        if has("ACCEPT") {
            return if f.decline_kfs { "DECLINE".into() } else { "ACCEPT".into() };
        }
        for b in ["PROCEED", "CHECK APPROVAL", "DONE", "YES", "RESEND OTP"] {
            if has(b) {
                return b.into();
            }
        }
        if let Some(first) = opts.first() {
            return first.to_string();
        }
        "Proceed".into()
    }
}

fn cap(s: &str) -> String {
    let mut ch = s.chars();
    match ch.next() {
        Some(f) => f.to_uppercase().collect::<String>() + ch.as_str(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------

pub struct LlmHuman {
    harness: AgentHarness,
    model_spec: String,
    system_prompt: String,
    lf: Langfuse,
    lf_session: String,
    rec: Option<Recorder>,
}

pub fn persona_card(sc: &Scenario) -> String {
    let c = &sc.customer;
    let p = &sc.profile;
    let mut s = format!(
        "- Customer name: {}\n- Mobile: {}\n- Email: {}\n- Aadhaar: {}\n- PAN: {}\n- Address: {}\n- Date of birth: {}\n- Appointment ID: {}\n- Preferred language: {}\n- Nominee: {} (phone {}, relation {})\n",
        c.name, c.mobile, c.email, c.aadhaar, c.pan, c.address, c.dob, p.appointment_id, p.language, p.nominee.name, p.nominee.phone, p.nominee.relation
    );
    if let Some(a) = &p.nominee_alt {
        s += &format!("- Backup nominee (only if told the first nominee matches an existing record): {} (phone {}, relation {})\n", a.name, a.phone, a.relation);
    }
    for (i, g) in sc.gold.items.iter().enumerate() {
        s += &format!("- Ornament {}: {}, {}K, gross {} g\n", i + 1, g.description, g.karat, g.gross_wt);
    }
    if sc.gold.items.len() > 1 {
        s += &format!(
            "- You are pledging ALL {n} of the ornaments above IN THIS ONE APPLICATION, not just one. When asked \"Does the customer have another ornament?\", answer YES and upload the next one, {n} times total, and only answer NO after all {n} have been uploaded. Do not stop early.\n",
            n = sc.gold.items.len()
        );
    }
    s += &format!("- Employment: {}\n", sc.income.employment_type);
    s += &format!("- Scheme you want: {}\n", sc.choices.offer_id);
    s += &format!("- Disbursement: {}", sc.choices.disbursement_mode);
    if !sc.choices.account_number.is_empty() {
        s += &format!(", account number {}, IFSC {}", sc.choices.account_number, sc.choices.ifsc);
    }
    s += "\n\n# Documents you have physically ready (in your bag / on your phone), right now\n";
    s += "You are NOT missing any of these. Do not say you don't have them, don't have them handy, or need to go get them.\n";
    s += "- Your PAN card and Aadhaar card (front and back).\n";
    if sc.flags.override_address {
        s += "- A recent address proof (electricity bill).\n";
    }
    for (i, g) in sc.gold.items.iter().enumerate() {
        s += &format!("- Ornament {}'s photo on the weighing scale, and its purchase bill / valuation certificate.\n", i + 1);
    }
    if sc.income.employment_type == "salaried" {
        s += &format!("- Your last {} consecutive monthly payslips.\n", sc.income.payslips.len().max(3));
    }
    s
}

pub fn persona_text(sc: &Scenario) -> String {
    match &sc.persona {
        Some(p) if !p.occupation.is_empty() => format!(
            "- Age {}, {}.\n- Temperament: {}.\n- Background: {}\n- How you type: {}",
            p.age, p.occupation, p.temperament, p.background, p.voice
        ),
        _ => "- Ordinary, cooperative, businesslike.".to_string(),
    }
}

pub fn deviations(sc: &Scenario) -> String {
    let f = &sc.flags;
    let mut v: Vec<String> = vec![];
    if f.mobile_invalid_first { v.push("The first time you are asked for the mobile number, type only 9 digits (leave off the last one). After the assistant objects, type the full number.".into()); }
    if f.email_invalid_first { v.push("The first time you are asked for the email, type it without the @ sign. After the assistant objects, type it correctly.".into()); }
    if f.wrong_mobile_otp_first { v.push("The first time you enter the mobile OTP, change its last digit (wrong OTP). Enter the correct one when asked again.".into()); }
    if f.wrong_email_otp_first { v.push("The first time you enter the email OTP, change its last digit. Enter the correct one when asked again.".into()); }
    if f.wrong_aadhaar_otp_first { v.push("The first time you enter the Aadhaar OTP, change its last digit. Enter the correct one when asked again.".into()); }
    if f.override_address { v.push("When offered USE AADHAAR / OVERRIDE for the address, choose OVERRIDE and then upload.".into()); }
    if f.invalid_appointment_first { v.push("The first time you are asked for the appointment ID, type APX. Give the real one after the assistant objects.".into()); }
    if f.change_scheme_once { v.push("First name a different scheme than the one you want; on the plan screen choose CHANGE and then name your real scheme.".into()); }
    if f.negotiate_roi { if let Some(r) = sc.choices.requested_roi { v.push(format!("On the plan screen choose CHANGE and ask for a lower ROI of {r}%; accept whatever counter offer is made by choosing CONFIRM.")); } }
    if f.ocr_noise_first { v.push("(No action needed from you: the first scan of your identity documents may come out unreadable; just upload again when asked.)".into()); }
    if f.decline_kfs { v.push("When asked to accept or decline the Key Fact Statement, choose DECLINE.".into()); }
    if let Some(s) = f.exit_at_stage { v.push(format!("During stage {s}, after one or two exchanges, choose EXIT.")); }
    if v.is_empty() { "None. Behave normally and cooperatively.".into() } else { v.iter().map(|x| format!("- {x}")).collect::<Vec<_>>().join("\n") }
}

impl LlmHuman {
    pub async fn new(sc: &Scenario, models: Arc<Models>, model_spec: &str, state_dir: &std::path::Path, lf: Langfuse, lf_session: String) -> anyhow::Result<Self> {
        let model = resolve_model_spec(&models, model_spec).ok_or_else(|| anyhow::anyhow!("unknown human model: {model_spec}"))?;
        let store = create_sqlite_session_store(&state_dir.join("goldloan-human.db")).map_err(|e| anyhow::anyhow!("sqlite: {e}"))?;
        let repo = create_session_repository(store, None);
        let session = repo.create(None, Some(format!("human:{}", sc.id))).await.map_err(|e| anyhow::anyhow!("session: {e}"))?;
        let scratch = std::env::temp_dir().join(format!("goldloan-human-{}", sc.id));
        std::fs::create_dir_all(&scratch)?;
        let host_env: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(&scratch));
        let role = if sc.human_role == "customer" {
            "the customer, applying for a gold loan on your own through a chat form."
        } else {
            "a loan officer at a bank branch typing the customer's details into the gold-loan application while the customer sits beside you."
        };
        let style = if sc.style.is_empty() { "Terse and businesslike. Lowercase is fine. No greetings after the first message." } else { sc.style.as_str() };
        let prompt = HUMAN_SIM
            .replace("{{role_description}}", role)
            .replace("{{persona}}", &persona_text(sc))
            .replace("{{persona_card}}", &persona_card(sc))
            .replace("{{deviations}}", &deviations(sc))
            .replace("{{style}}", style);
        let system_prompt = prompt.clone();
        let harness = AgentHarness::new(AgentHarnessOptions {
            models,
            model: Some(model),
            session,
            host_env,
            tools: vec![],
            system_prompt: prompt,
            sandbox: SandboxMode::Disabled,
            resources: AgentHarnessResources { skills: Vec::new(), prompt_templates: Vec::new() },
        });
        let rec = lf.as_ref().map(|_| Recorder::attach(&harness));
        Ok(LlmHuman { harness, model_spec: model_spec.to_string(), system_prompt, lf, lf_session, rec })
    }

    async fn reply(&mut self, v: &HumanView<'_>) -> anyhow::Result<String> {
        let wall_start = chrono::Utc::now();
        let mut input = String::new();
        for d in &v.device {
            input += &format!("{d}\n");
        }
        input += &format!("The application assistant just wrote:\n\"\"\"\n{}\n\"\"\"\n", v.agent_text);
        if let Some(o) = v.options {
            input += &format!("Buttons on screen: {}\n", o.join(" | "));
        }
        input += "Type your next message (one line - only the text you would type, nothing else):";
        let history = match &self.rec {
            Some(r) => r.begin(&self.harness).await,
            None => vec![],
        };
        let mut last = String::new();
        for attempt in 1..=10u32 {
            match self.harness.prompt(if attempt == 1 { input.clone() } else { "(retry) Reply with your next message only.".to_string() }).await {
                Ok(m) => match provider_error(&m) {
                    None => {
                        let text = clean(&assistant_text(&m));
                        if !text.is_empty() {
                            if let (Some(c), Some(r)) = (&self.lf, &self.rec) {
                                let (steps, tool_runs) = r.finish(&self.system_prompt, history);
                                c.model_call(ModelCall {
                                    session_id: &self.lf_session,
                                    name: "human".into(),
                                    model: &self.model_spec,
                                    system: &self.system_prompt,
                                    prompt: &input,
                                    tools: serde_json::json!([]),
                                    output: serde_json::json!(text),
                                    steps,
                                    tool_runs,
                                    start: wall_start,
                                    end: chrono::Utc::now(),
                                    metadata: serde_json::json!({"stage": v.stage}),
                                });
                            }
                            return Ok(text);
                        }
                        last = "empty reply".into();
                    }
                    Some(e) => last = e,
                },
                Err(e) => last = e.to_string(),
            }
            tokio::time::sleep(std::time::Duration::from_secs((1u64 << (attempt - 1)).min(30))).await;
        }
        anyhow::bail!("human model failed: {last}")
    }
}

fn clean(s: &str) -> String {
    // Cut anything after a role tag the model may hallucinate ("...com[ASSISTANT]: ...").
    let cut = Regex::new(r"\[(ASSISTANT|USER|DEVICE|BUTTONS)[^\]]*\]|The application assistant just wrote").unwrap();
    let s = match cut.find(s) {
        Some(m) => &s[..m.start()],
        None => s,
    };
    let line = s.lines().map(|l| l.trim()).find(|l| !l.is_empty()).unwrap_or("");
    let line = line.trim_matches(|c| c == '"' || c == '`' || c == '\'');
    let out: String = line.trim_start_matches("[USER]:").trim_start_matches("USER:").trim().chars().take(240).collect();
    undouble(&out)
}

/// "PROCEEDProceed" / "YESYES" -> the first half (case-insensitive doubled token).
fn undouble(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() >= 4 && c.len() % 2 == 0 {
        let (a, b) = c.split_at(c.len() / 2);
        if a.iter().collect::<String>().eq_ignore_ascii_case(&b.iter().collect::<String>()) {
            return a.iter().collect();
        }
    }
    s.to_string()
}
