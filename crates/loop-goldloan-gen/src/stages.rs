//! Stage table (from `goldloan-datagen/spec/stages.json`) and stage prompts.

use serde::Deserialize;

const STAGES_JSON: &str = include_str!("../../../goldloan-datagen/spec/stages.json");
const COMMON: &str = include_str!("../../../goldloan-datagen/spec/prompts/_common.md");
const P1: &str = include_str!("../../../goldloan-datagen/spec/prompts/stage1_onboarding.md");
const P2: &str = include_str!("../../../goldloan-datagen/spec/prompts/stage2_identity.md");
const P3: &str = include_str!("../../../goldloan-datagen/spec/prompts/stage3_profile.md");
const P4: &str = include_str!("../../../goldloan-datagen/spec/prompts/stage4_gold.md");
const P5: &str = include_str!("../../../goldloan-datagen/spec/prompts/stage5_offer.md");
const P6: &str = include_str!("../../../goldloan-datagen/spec/prompts/stage6_closing.md");
pub const HUMAN_SIM: &str = include_str!("../../../goldloan-datagen/spec/prompts/human_sim.md");

#[derive(Debug, Clone, Deserialize)]
pub struct Stage {
    pub id: u8,
    pub name: String,
    pub agent_label: String,
    pub welcome: String,
}

#[derive(Deserialize)]
struct StagesFile {
    stages: Vec<Stage>,
}

pub fn stages() -> Vec<Stage> {
    serde_json::from_str::<StagesFile>(STAGES_JSON).expect("stages.json").stages
}

pub fn stage(id: u8) -> Stage {
    stages().into_iter().find(|s| s.id == id).expect("stage id")
}

pub fn system_prompt(id: u8) -> String {
    let body = match id {
        1 => P1,
        2 => P2,
        3 => P3,
        4 => P4,
        5 => P5,
        _ => P6,
    };
    format!("{COMMON}\n\n{body}")
}
