//! Reply helpers shared by the scripted agent, the LLM agent and the orchestrator.

use regex::Regex;
use serde_json::Value;

pub struct AgentOut {
    pub text: String,
    /// The model's original text when `text` was repaired (see `collapse_repeats`).
    pub raw_text: Option<String>,
    pub options: Option<Vec<String>>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
}

/// Split `text <<BUTTONS:A/B>>` into (text, options). Accepts 2 or 3 angle brackets.
pub fn parse_reply(raw: &str) -> (String, Option<Vec<String>>) {
    let re = Regex::new(r"<{2,3}\s*BUTTONS\s*:\s*([^>]*?)\s*>{2,3}").unwrap();
    let opts = re.captures(raw).map(|c| {
        c[1].split('/').map(|s| s.trim().to_uppercase()).filter(|s| !s.is_empty()).collect::<Vec<_>>()
    });
    let text = re.replace_all(raw, "").trim().to_string();
    (text, opts.filter(|o| !o.is_empty()))
}

/// Some models repeat their last sentence (or the whole reply) verbatim, with
/// nothing or only whitespace in between ("...full name.Please enter ... full
/// name."). That is a mechanical glitch, so it is safe to repair: drop the
/// second copy. Anything less exact (paraphrases, glued extra sentences) is
/// NOT touched here — the verifier rejects those traces instead.
pub fn collapse_repeats(s: &str) -> String {
    let t: Vec<char> = s.trim().chars().collect();
    let n = t.len();
    for l in (12..=n / 2).rev() {
        for sep in 0..=2usize {
            if n < 2 * l + sep {
                continue;
            }
            let tail = &t[n - l..];
            let prev = &t[n - 2 * l - sep..n - l - sep];
            let gap = &t[n - l - sep..n - l];
            if tail == prev && gap.iter().all(|c| c.is_whitespace()) {
                return t[..n - l].iter().collect::<String>().trim_end().to_string();
            }
        }
    }
    t.iter().collect()
}

/// A complete reply followed by a glued, redundant tail ("...payslips.Please provide the pays...").
/// Keeps the head up to the glue point when the head is a clean, complete message and the tail is
/// shorter than it. Only ever removes text; returns None when the shape does not match.
pub fn strip_glued_tail(t: &str) -> Option<String> {
    let s = t.trim();
    let re = Regex::new(r"[a-z0-9)][.?!]([A-Z][a-z])").unwrap();
    let m = re.captures(s)?;
    let cut = m.get(1)?.start(); // first char of the glued tail
    let (head, tail) = (s[..cut].trim(), s[cut..].trim());
    (head.chars().count() >= 25 && tail.chars().count() <= head.chars().count() && !looks_garbled(head)).then(|| head.to_string())
}

/// Cheap check for model-output glitches seen with the GPT agents: blank-line runs,
/// leaked reasoning/ellipses, glued sentences, or a fragment cut off mid-phrase.
/// (The verifier applies the same rules, so a reply that passes here passes there.)
pub fn looks_garbled(t: &str) -> bool {
    let s = t.trim();
    if s.is_empty() {
        return true;
    }
    if Regex::new(r"\n{3,}|[ \t]{6,}").unwrap().is_match(s) {
        return true;
    }
    if s.contains('…') || s.contains("...") || s.contains("The answer:") || s.contains("(Note:") {
        return true;
    }
    if Regex::new(r"[a-z0-9)][.?!][A-Z][a-z]").unwrap().is_match(s) {
        return true;
    }
    let last = s.chars().last().unwrap();
    s.chars().count() < 20 && !s.contains('\n') && !matches!(last, '.' | '?' | '!' | ')' | '%')
}

pub fn btn(text: &str, opts: &[&str]) -> String {
    format!("{text} <<BUTTONS:{}>>", opts.join("/"))
}

pub fn succ(v: &Value) -> bool {
    v.get("success").and_then(|x| x.as_bool()).unwrap_or(false)
}

pub fn code(v: &Value) -> &str {
    v.get("error_code").and_then(|x| x.as_str()).unwrap_or("")
}

pub fn msg(v: &Value) -> &str {
    v.get("message").and_then(|x| x.as_str()).unwrap_or("")
}

pub fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0)
}

pub fn strv<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_whole_and_tail_repeats() {
        assert_eq!(collapse_repeats("Please enter the customer's full name.Please enter the customer's full name."), "Please enter the customer's full name.");
        assert_eq!(
            collapse_repeats("Aadhaar Card Exists.\nDo you give consent to verify the PAN-Aadhaar link?Do you give consent to verify the PAN-Aadhaar link?"),
            "Aadhaar Card Exists.\nDo you give consent to verify the PAN-Aadhaar link?"
        );
        assert_eq!(collapse_repeats("Same sentence here.\n\nSame sentence here."), "Same sentence here.");
    }

    #[test]
    fn leaves_distinct_text_alone() {
        let t = "OTP sent successfully for Aadhaar XXXX-XXXX-3628. Please enter the OTP received by the customer.Please provide the OTP you received.";
        assert_eq!(collapse_repeats(t), t);
        assert_eq!(collapse_repeats("ha ha ha ha"), "ha ha ha ha");
    }

    #[test]
    fn detects_the_glitches_seen_with_gpt() {
        assert!(looks_garbled("All      \n\n\n\nThe answer: \n\n\n\n\n\nThe \n\n\n\n   ...\n\nAll onboarding details collected."));
        assert!(looks_garbled("Please enter"));
        assert!(looks_garbled("Please ...\n\nPlease enter the 6-digit OTP sent to vi****@example.com."));
        assert!(looks_garbled("Identity documents stored. Proceed to Aadhaar OTP eKYC? OTP sent successfully for Aadhaar XXXX-XXXX-0616. Please enter the OTP received by the customer.Please provide the OTP."));
        assert!(!looks_garbled("Welcome to Stage 1: Gold Loan Onboarding.\nPlease enter customer's 10-digit mobile number"));
        assert!(!looks_garbled("Please open this link to complete your e-signature: http://esign.mock.local:8086/consent_page_for_ekyc?client_id=eKYC_URL_REF175264"));
        assert!(!looks_garbled("Approved.\nPlease ask the customer to acknowledge the Key Fact Statement (KFS)."));
    }

    #[test]
    fn strips_a_glued_redundant_tail_but_only_when_the_head_is_clean() {
        let t = "OTP sent successfully for Aadhaar XXXX-XXXX-3628. Please enter the OTP received by the customer.Please provide the OTP you received.";
        assert_eq!(strip_glued_tail(t).unwrap(), "OTP sent successfully for Aadhaar XXXX-XXXX-3628. Please enter the OTP received by the customer.");
        assert!(strip_glued_tail("Fine.\nAll good here.").is_none()); // no glue
        assert!(strip_glued_tail("Hi.There").is_none()); // head too short to trust
    }

    #[test]
    fn parses_buttons_two_or_three_brackets() {
        let (t, o) = parse_reply("Ready?  \n<<<BUTTONS:PROCEED/EXIT>>>");
        assert_eq!(t, "Ready?");
        assert_eq!(o.unwrap(), vec!["PROCEED", "EXIT"]);
        let (_, o) = parse_reply("x <<BUTTONS:YES/NO/EXIT>>");
        assert_eq!(o.unwrap().len(), 3);
    }
}
