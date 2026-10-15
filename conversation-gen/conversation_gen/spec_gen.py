"""spec_gen.py — combinatorial generator for ConversationSpec files.

No extra model calls: cartesian-combine a small set of persona archetypes
with topic seeds per archetype, write one ConversationSpec JSON per
combination into artifacts/conversation_task_bank/. A modest seed list here
already produces dozens-to-hundreds of specs.
"""

from __future__ import annotations

import argparse
import json
import re
from dataclasses import dataclass
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[1]
TASK_BANK_DIR = REPO_ROOT.parent / "artifacts" / "conversation_task_bank"

ASKER_MODEL = "soket/qwen3-8-27b"
RESPONDER_MODEL = "soket/gpt"

ASKER_SYSTEM_PROMPT = (
    "You are {persona_desc}. You have a real, specific problem and want practical "
    "advice. Ask natural follow-up questions based on what the expert tells you — "
    "mention your specific conditions and constraints as they come up, the way a "
    "real person would in conversation, not all at once up front."
)

RESPONDER_SYSTEM_PROMPT = (
    "You are {expert_desc}. Give specific, actionable advice, not generic "
    "platitudes. When current information would help (current best practices, "
    "prices, regulations, product names), use web_search rather than guessing. "
    "Ask clarifying questions when the asker's description is ambiguous."
)


@dataclass
class PersonaArchetype:
    name: str
    persona_desc: str  # goes into the asker's system prompt
    expert_name: str
    expert_desc: str  # goes into the responder's system prompt
    topics: list[str]


ARCHETYPES: list[PersonaArchetype] = [
    PersonaArchetype(
        name="Farmer",
        persona_desc="a small-scale vegetable farmer",
        expert_name="Agronomy Expert",
        expert_desc="an agricultural extension expert",
        topics=[
            "My tomato plants have curling yellow leaves and I'm seeing tiny white insects underneath. What's going on and what should I do?",
            "It hasn't rained in three weeks and my irrigation bill is climbing. How should I be scheduling watering for the rest of the season?",
            "Grain prices have been dropping and I'm not sure whether to sell now or store this year's harvest. How should I think about this?",
            "I want to add a cover crop this fall but I've never done it before. Where do I even start?",
            "My soil test came back showing low nitrogen and slightly acidic pH. What's the right way to fix that before next planting?",
        ],
    ),
    PersonaArchetype(
        name="Student",
        persona_desc="a university student working on a research paper",
        expert_name="Writing Coach",
        expert_desc="an academic writing coach",
        topics=[
            "I have to write a literature review for my thesis and I don't know how to organize 40 sources into something coherent. Where do I start?",
            "My advisor said my research question is 'too broad.' How do I actually narrow it down?",
            "I keep getting told my writing is 'too informal' for academic papers but I don't know what that means concretely.",
            "I'm three weeks from my thesis deadline and I still don't have a clear methodology section. What do I do?",
        ],
    ),
    PersonaArchetype(
        name="Small Business Owner",
        persona_desc="the owner of a small independent coffee shop",
        expert_name="Business Advisor",
        expert_desc="a small business advisor",
        topics=[
            "My rent just went up 20% and I'm trying to figure out if I need to raise prices or cut costs elsewhere first.",
            "I want to start doing local delivery but I don't know how to price it without losing money.",
            "A competitor just opened two blocks away with lower prices. How worried should I be and what should I actually do?",
            "I'm thinking about hiring my first employee but I'm nervous about the fixed costs. How do I know if I'm ready?",
        ],
    ),
    PersonaArchetype(
        name="New Homeowner",
        persona_desc="someone who just bought their first house",
        expert_name="Home Improvement Expert",
        expert_desc="a home improvement and maintenance expert",
        topics=[
            "There's a musty smell in my basement after it rained last week. Is this something I need to worry about?",
            "My energy bills are way higher than the previous owner said they'd be. What should I check first?",
            "I want to redo my kitchen but I don't know what actually needs a permit versus what I can just do myself.",
            "The inspector flagged 'minor foundation settling' before I bought the house. How do I know if that's actually fine?",
        ],
    ),
    PersonaArchetype(
        name="New Pet Owner",
        persona_desc="someone who just adopted their first dog",
        expert_name="Veterinary Behaviorist",
        expert_desc="a veterinary behaviorist",
        topics=[
            "My new dog barks and scratches at the door every time I leave the house. Is this separation anxiety and what do I do?",
            "I'm not sure how much to feed him — the bag says one thing but he seems hungry all the time.",
            "He's great with me but growls at other dogs on walks. How do I fix this safely?",
        ],
    ),
    PersonaArchetype(
        name="Job Seeker",
        persona_desc="someone re-entering the job market after a career break",
        expert_name="Career Coach",
        expert_desc="a career coach",
        topics=[
            "I've been out of the workforce for three years and I don't know how to explain the gap without it working against me.",
            "I keep getting to final interviews and then not getting offers. What am I doing wrong?",
            "I'm not sure if I should stay in my old field or pivot entirely. How do I even decide that?",
        ],
    ),
    PersonaArchetype(
        name="Home Cook",
        persona_desc="someone learning to cook seriously for the first time",
        expert_name="Culinary Instructor",
        expert_desc="a professional culinary instructor",
        topics=[
            "Every time I sear meat it either sticks to the pan or ends up gray instead of browned. What am I doing wrong?",
            "I want to meal prep for the week but everything tastes the same by Wednesday. How do people avoid that?",
            "My bread always comes out dense no matter what recipe I follow. What's actually going on?",
        ],
    ),
]


def slugify(text: str) -> str:
    slug = re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")
    return slug[:60]


def build_spec(
    archetype: PersonaArchetype, topic: str, index: int, *, rounds_per_persona: int
) -> dict:
    conv_id = f"{slugify(archetype.name)}_{index:03d}_{slugify(topic)}"
    return {
        "id": conv_id,
        "topic": topic,
        "rounds_per_persona": rounds_per_persona,
        "personas": [
            {
                "name": archetype.name,
                "model": ASKER_MODEL,
                "system_prompt": ASKER_SYSTEM_PROMPT.format(persona_desc=archetype.persona_desc),
                "allow_web_search": False,
            },
            {
                "name": archetype.expert_name,
                "model": RESPONDER_MODEL,
                "system_prompt": RESPONDER_SYSTEM_PROMPT.format(expert_desc=archetype.expert_desc),
                "allow_web_search": True,
            },
        ],
    }


def generate(
    *, out_dir: Path = TASK_BANK_DIR, rounds_per_persona: int = 8, archetypes: list[PersonaArchetype] | None = None
) -> list[Path]:
    archetypes = archetypes if archetypes is not None else ARCHETYPES
    out_dir.mkdir(parents=True, exist_ok=True)
    written = []
    for archetype in archetypes:
        for i, topic in enumerate(archetype.topics, start=1):
            spec = build_spec(archetype, topic, i, rounds_per_persona=rounds_per_persona)
            path = out_dir / f"{spec['id']}.json"
            path.write_text(json.dumps(spec, indent=2, ensure_ascii=False), encoding="utf-8")
            written.append(path)
    return written


def main() -> int:
    parser = argparse.ArgumentParser(description="Generate ConversationSpec files from persona/topic seeds")
    parser.add_argument("--out", type=Path, default=TASK_BANK_DIR)
    parser.add_argument("--rounds-per-persona", type=int, default=8)
    args = parser.parse_args()

    written = generate(out_dir=args.out, rounds_per_persona=args.rounds_per_persona)
    print(f"Wrote {len(written)} conversation specs to {args.out}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
