//! Business rules, loaded from `goldloan-datagen/spec/business_rules.json`
//! (embedded at build time; override with `--rules`). Fields ending in
//! `_provenance` in the JSON are documentation and ignored here.

use serde::Deserialize;

pub const DEFAULT_RULES_JSON: &str = include_str!("../../../goldloan-datagen/spec/business_rules.json");

#[derive(Debug, Clone, Deserialize)]
pub struct Rules {
    pub gold: GoldRules,
    pub schemes: Vec<Scheme>,
    pub roi_negotiation: RoiNegotiation,
    pub income: IncomeRules,
    pub bureau: BureauRules,
    pub otp: OtpRules,
    pub closing: ClosingRules,
    pub appointments: Appointments,
    pub languages: Vec<String>,
    pub nominee_relations: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GoldRules {
    pub rate_per_22k_gram: f64,
    pub ltv_tiers: Vec<LtvTier>,
    pub min_karat: f64,
    pub max_items: usize,
    pub weight_match_tolerance_g: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LtvTier {
    pub up_to_loan: Option<f64>,
    pub ltv_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Scheme {
    pub offer_id: String,
    pub scheme_type: String,
    pub roi: f64,
    pub tenure_months: u32,
    pub repayment: String,
    pub processing_fee_pct: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RoiNegotiation {
    pub enabled: bool,
    pub max_discount_pct_points: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IncomeRules {
    pub income_required_above_loan: f64,
    pub payslips_required: usize,
    pub max_payslip_age_months: i32,
    pub foir_check_enabled: bool,
    pub foir_max_pct: f64,
    pub self_employed_supported: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BureauRules {
    pub min_score_for_loan: u32,
    pub tiers: Vec<BureauTier>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BureauTier {
    pub min: u32,
    pub tier: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OtpRules {
    pub length: usize,
    pub max_attempts: u32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClosingRules {
    pub cooling_off_seconds: u64,
    pub disbursement_modes: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Appointments {
    pub valid_id_pattern: String,
}

impl Rules {
    pub fn load(path: Option<&std::path::Path>) -> anyhow::Result<Rules> {
        let raw = match path {
            Some(p) => std::fs::read_to_string(p)?,
            None => DEFAULT_RULES_JSON.to_string(),
        };
        Ok(serde_json::from_str(&raw)?)
    }

    /// Maximum loan for a given collateral value under the tiered LTV caps.
    /// Returns (loan, nominal LTV % of the slab used). A slab whose LTV would
    /// exceed its own loan cap is skipped; the loan then plateaus at the
    /// previous slab's cap so it never falls as collateral value rises.
    pub fn max_loan(&self, value: f64) -> (f64, f64) {
        let mut prev_cap = 0.0_f64;
        for t in &self.gold.ltv_tiers {
            let l = value * t.ltv_pct / 100.0;
            match t.up_to_loan {
                Some(cap) if l > cap => {
                    prev_cap = cap;
                    continue;
                }
                _ => return ((l.max(prev_cap) * 100.0).round() / 100.0, t.ltv_pct),
            }
        }
        (0.0, 0.0)
    }

    pub fn tier_for(&self, score: u32) -> &str {
        self.bureau
            .tiers
            .iter()
            .filter(|t| score >= t.min)
            .max_by_key(|t| t.min)
            .map(|t| t.tier.as_str())
            .unwrap_or("Poor")
    }

    pub fn scheme(&self, id: &str) -> Option<&Scheme> {
        self.schemes.iter().find(|s| s.offer_id.eq_ignore_ascii_case(id.trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiered_ltv_matches_rbi_slabs_and_the_demo() {
        let r = Rules::load(None).unwrap();
        assert_eq!(r.max_loan(498_981.82), (399_185.46, 80.0)); // the demo
        assert_eq!(r.max_loan(200_000.0), (170_000.0, 85.0)); // small loan: 85%
        assert_eq!(r.max_loan(700_000.0), (525_000.0, 75.0)); // > 5 lakh: 75%
    }

    #[test]
    fn loan_plateaus_between_slabs_and_never_decreases() {
        let r = Rules::load(None).unwrap();
        assert_eq!(r.max_loan(300_000.0), (250_000.0, 80.0)); // 85% would exceed 2.5L, 80% falls below it
        let mut last = 0.0;
        for v in (100_000..900_000).step_by(2_500) {
            let (l, _) = r.max_loan(v as f64);
            assert!(l >= last, "loan fell at value {v}: {l} < {last}");
            last = l;
        }
    }
}
