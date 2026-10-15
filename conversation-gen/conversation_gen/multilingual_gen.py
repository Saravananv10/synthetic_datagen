"""multilingual_gen.py — generate the full language x domain task bank.

42 languages x 28 domains x N conversations each, written as one JSONL shard
per (domain, language) pair (NOT one file per conversation — at full scale
that's 11.76M individual files, which no filesystem/tool handles well; a
JSONL shard per combo keeps this to 1,176 files total). Each shard is
consumable directly by the Rust engines via `--spec <file>.jsonl`.

Uniqueness without a model call per spec: each spec is built by deterministic
pseudo-random sampling (seeded on language+domain+index) across independent
axes — topic template, issue, modifier, place/staple/currency (region-
sensitive domains only), and persona names. The combined sample space per
domain is comfortably larger than any requested count, so collisions across
10,000 draws are rare and inconsequential (as in any real synthetic dataset
at this scale).
"""

from __future__ import annotations

import argparse
import json
import random
import time
from pathlib import Path

from conversation_gen.domains import DOMAINS, Domain
from conversation_gen.languages import LANGUAGES, Language

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUT = REPO_ROOT / "artifacts" / "multilingual_task_bank"

# Confirmed-healthy Soket models this session. Only 3 distinct ones were
# reachable when this was built (see happy-launching-flame.md) — reused
# across roles rather than inventing untested model ids.
STRONG = "soket/gpt"
FAST = "soket/qwen3-8-27b"
FAST_ALT = "soket/gemma-4-31b"

PAIR_MODELS = [FAST, STRONG]  # [asker, responder]
GROUP_MODELS = [STRONG, FAST, FAST_ALT, FAST]  # 4 roles, matches earlier live-validated pattern

PAIR_ROUNDS = 10  # x2 personas = 20 total turns
GROUP_ROUNDS = 20  # x4 personas = 80 total turns


def build_spec(lang: Language, domain: Domain, index: int) -> dict:
    rng = random.Random(f"{lang.slug}:{domain.slug}:{index}")

    fmt = {
        "issue": rng.choice(domain.issues),
        "modifier": rng.choice(domain.modifiers),
    }
    if domain.region_sensitive:
        fmt.update(
            place=rng.choice(lang.places),
            staple=rng.choice(lang.staples),
            currency=lang.currency,
            regulator=lang.regulator,
            region=lang.region,
        )

    template = rng.choice(domain.topic_templates)
    topic = f"{template.format(**fmt)}\n\n{lang.instruction}"

    # `person_names` is authored as a strict [male, female, male, female, ...]
    # sequence per language (see languages.py) — sample indices, not names, so
    # each pick keeps its gender via index parity. Without this, the model has
    # no gender signal beyond the name itself and was observed defaulting to
    # male-gendered terms of address (e.g. Tamil "அண்ணா") for female personas
    # in languages with grammatical/honorific gender.
    name_idxs = rng.sample(range(len(lang.person_names)), k=len(domain.roles))
    picks = [(lang.person_names[i], "male" if i % 2 == 0 else "female") for i in name_idxs]
    roster_note = " ".join(f"{n} is {g}." for n, g in picks)
    models = PAIR_MODELS if domain.shape == "pair" else GROUP_MODELS

    personas = []
    for i in range(len(domain.roles)):
        desc = domain.role_descs[i].format(**fmt)
        name, gender = picks[i]
        gender_note = (
            f"Your name is {name} and you are {gender}. {roster_note} If {lang.name} "
            f"grammatically marks gender (verb forms, adjectives, or terms of address such as "
            f"brother/sister-style honorifics), use forms consistent with each person's actual "
            f"gender as given above — never default to male forms for a female persona or vice versa."
        )
        persona = {
            "name": name,
            "model": models[i],
            "system_prompt": f"{desc} {gender_note} {lang.instruction}",
            "allow_web_search": domain.search_roles[i],
        }
        if domain.shape == "group":
            persona["team"] = None
        personas.append(persona)

    spec = {
        "id": f"{lang.slug}__{domain.slug}__{index:05d}",
        "topic": topic,
        "rounds_per_persona": PAIR_ROUNDS if domain.shape == "pair" else GROUP_ROUNDS,
        "personas": personas,
    }
    if domain.shape == "group":
        spec["turn_order"] = "round_robin"
    return spec


def write_shard(lang: Language, domain: Domain, count: int, out_dir: Path, *, force: bool) -> Path:
    shard_dir = out_dir / domain.slug
    shard_dir.mkdir(parents=True, exist_ok=True)
    path = shard_dir / f"{lang.slug}.jsonl"
    if path.is_file() and not force:
        return path
    tmp = path.with_suffix(".jsonl.tmp")
    with tmp.open("w", encoding="utf-8") as fh:
        for i in range(count):
            spec = build_spec(lang, domain, i)
            fh.write(json.dumps(spec, ensure_ascii=False) + "\n")
    tmp.replace(path)
    return path


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Generate the multilingual x multi-domain task bank")
    parser.add_argument("--count", type=int, default=10000, help="Conversations per (language, domain) pair")
    parser.add_argument("--languages", default="all", help="Comma-separated language slugs, or 'all'")
    parser.add_argument("--domains", default="all", help="Comma-separated domain slugs, or 'all'")
    parser.add_argument("--out", type=Path, default=DEFAULT_OUT)
    parser.add_argument("--force", action="store_true", help="Regenerate shards that already exist")
    args = parser.parse_args(argv)

    langs = LANGUAGES if args.languages == "all" else [
        l for l in LANGUAGES if l.slug in set(args.languages.split(","))
    ]
    doms = DOMAINS if args.domains == "all" else [
        d for d in DOMAINS if d.slug in set(args.domains.split(","))
    ]

    total_combos = len(langs) * len(doms)
    total_specs = total_combos * args.count
    print(
        f"Generating {total_specs:,} specs across {total_combos:,} shards "
        f"({len(langs)} languages x {len(doms)} domains x {args.count:,} each) -> {args.out}"
    )

    start = time.monotonic()
    done = 0
    for domain in doms:
        for lang in langs:
            write_shard(lang, domain, args.count, args.out, force=args.force)
            done += 1
            if done % 25 == 0 or done == total_combos:
                elapsed = time.monotonic() - start
                rate = done / elapsed if elapsed > 0 else 0
                eta = (total_combos - done) / rate if rate > 0 else 0
                print(f"  {done:,}/{total_combos:,} shards ({done * args.count:,} specs) "
                      f"— {elapsed:.0f}s elapsed, ~{eta:.0f}s remaining")

    print(f"Done: {total_combos:,} shards, {total_specs:,} specs, {time.monotonic() - start:.0f}s total")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
