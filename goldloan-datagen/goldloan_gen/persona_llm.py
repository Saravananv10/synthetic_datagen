"""GPT-written personas: replace the rule-based names/addresses/backgrounds with richer ones.

The generator's scenarios already work with random pools; this step asks GPT to invent a
believable fictional applicant for each scenario (region, occupation, temperament, voice) and
rewrites the scenario consistently (name, father, email, address, nominee, payslip names,
employer, persona card). Numeric identifiers (mobile, Aadhaar, PAN, account) stay machine-made.
Anything GPT returns that fails validation is ignored and the rule-based value is kept.

Uses the OpenAI-compatible endpoint (base URL from conversation-gen/.env's OPENAI_BASE_URL;
API key from goldloan-datagen/.env's GOLDLOAN_SOKET_API_KEY if set - the key scoped to this
project alone - else falls back to conversation-gen/.env's OPENAI_API_KEY).
"""
from __future__ import annotations

import json
import os
import re
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

SHARED_ENV = Path(__file__).resolve().parents[2] / "conversation-gen" / ".env"
PROJECT_ENV = Path(__file__).resolve().parents[1] / ".env"

PROMPT = """You are helping build a realistic synthetic dataset for testing a gold-loan chatbot.
Invent ONE fictional Indian person applying for a gold loan.

Sketch: {sketch}
Lives in: {city}, {state}.
Employment: {employment}.
Their {n_items} ornament(s): {ornaments}.

Return ONLY a JSON object with these keys:
  first_name, last_name, father_name (full name), email_local (lowercase letters, digits, dots only),
  age (integer 24-62), occupation (short), employer (a FICTIONAL company / shop / hospital / school name,
  or the name of their own small business), address_line (house or flat number plus street/locality; NO city,
  state or pincode), nominee_name (full name of a relative), temperament (one short phrase describing how they
  behave in a support chat), background (1-2 sentences: their situation and why they want the loan),
  voice (one sentence: how they type in a chat form - language mix, punctuation, typos).
Names must feel typical for the region. Do NOT use real people, real companies, or any phone/ID numbers."""


def _load_env() -> dict:
    # Precedence, weakest to strongest: shared .env file < project .env file < real shell env.
    env: dict = {}
    for path in (SHARED_ENV, PROJECT_ENV):
        if path.exists():
            for line in path.read_text().splitlines():
                if "=" in line and not line.strip().startswith("#"):
                    k, v = line.split("=", 1)
                    env[k.strip()] = v.strip()
    if env.get("GOLDLOAN_SOKET_API_KEY"):
        env["OPENAI_API_KEY"] = env["GOLDLOAN_SOKET_API_KEY"]
    env.update(os.environ)  # a real shell env var always wins over either file
    if os.environ.get("GOLDLOAN_SOKET_API_KEY"):
        env["OPENAI_API_KEY"] = os.environ["GOLDLOAN_SOKET_API_KEY"]
    return env


def chat(model: str, prompt: str, *, max_tokens: int = 1500, temperature: float = 0.9, retries: int = 6) -> str:
    env = _load_env()
    body = json.dumps({"model": model, "messages": [{"role": "user", "content": prompt}], "max_tokens": max_tokens,
                       "temperature": temperature}).encode()
    last = None
    for i in range(retries):
        try:
            req = urllib.request.Request(env["OPENAI_BASE_URL"].rstrip("/") + "/chat/completions", data=body,
                                         headers={"Authorization": f"Bearer {env['OPENAI_API_KEY']}", "Content-Type": "application/json"})
            with urllib.request.urlopen(req, timeout=180) as r:
                return json.load(r)["choices"][0]["message"].get("content") or ""
        except Exception as e:  # noqa: BLE001 - gateway 502s are common; back off and retry
            last = e
            time.sleep(min(2 ** i, 30))
    raise RuntimeError(f"persona model failed: {last}")


def _json(text: str) -> dict | None:
    a, b = text.find("{"), text.rfind("}")
    if a < 0 or b <= a:
        return None
    try:
        v = json.loads(text[a:b + 1])
        return v if isinstance(v, dict) else None
    except json.JSONDecodeError:
        return None


NAME = re.compile(r"^[A-Za-z][A-Za-z .'-]{1,38}$")


def _clean(p: dict) -> dict | None:
    """Validate GPT's profile; return only the fields that are safe to apply."""
    out = {}
    for k in ("first_name", "last_name"):
        if isinstance(p.get(k), str) and NAME.match(p[k].strip()) and " " not in p[k].strip():
            out[k] = p[k].strip().title()
    if len(out) < 2:
        return None
    for k in ("father_name", "nominee_name"):
        if isinstance(p.get(k), str) and NAME.match(p[k].strip()):
            out[k] = " ".join(w.capitalize() for w in p[k].split())
    if isinstance(p.get("email_local"), str) and re.fullmatch(r"[a-z][a-z0-9.]{2,28}", p["email_local"].strip().lower()):
        out["email_local"] = p["email_local"].strip().lower()
    if isinstance(p.get("address_line"), str) and 8 <= len(p["address_line"].strip()) <= 90 and not re.search(r"\d{6}", p["address_line"]):
        out["address_line"] = re.sub(r"\s+", " ", p["address_line"].strip().strip(","))
    for k, lo, hi in (("occupation", 3, 60), ("employer", 3, 60), ("temperament", 3, 80), ("background", 10, 300), ("voice", 5, 200)):
        if isinstance(p.get(k), str) and lo <= len(p[k].strip()) <= hi:
            out[k] = p[k].strip()
    if isinstance(p.get("age"), int) and 24 <= p["age"] <= 62:
        out["age"] = p["age"]
    return out


def apply_profile(sc: dict, prof: dict) -> None:
    c = sc["customer"]
    old = c["name"]
    name = f"{prof['first_name']} {prof['last_name']}"
    c["name"] = name
    if "father_name" in prof:
        c["father"] = prof["father_name"]
    if "email_local" in prof:
        c["email"] = f"{prof['email_local']}@example.com"
    if "address_line" in prof:
        m = re.search(r",\s*([^,]+),\s*([^,]+?)\s*-\s*(\d{6})$", c["address"])
        if m:
            c["address"] = f"{prof['address_line']}, {m[1]}, {m[2]} - {m[3]}"
    for p in sc["income"].get("payslips", []) + sc["income"].get("payslips_bad", []):
        if p["name"] == old:
            p["name"] = name
    if "nominee_name" in prof:
        sc["profile"]["nominee"]["name"] = prof["nominee_name"]
    if sc["profile"].get("nominee_alt"):
        sc["profile"]["nominee_alt"]["name"] = f"{prof['first_name']}'s relative".replace("'s relative", "") + " " + prof["last_name"]
    if "employer" in prof and sc["income"]["employment_type"] == "salaried":
        sc["income"]["employer"] = prof["employer"]
    persona = sc.setdefault("persona", {})
    for k in ("age", "occupation", "temperament", "background", "voice"):
        if k in prof:
            persona[k] = prof[k]
    sc["tags"] = sorted(set(sc.get("tags", []) + ["llm_persona"]))


def enrich(scenarios: list[dict], model: str = "gpt", workers: int = 4, cache: Path | None = None) -> int:
    """Rewrite scenarios in place using GPT-written profiles. Returns how many were enriched."""
    if cache:
        cache.mkdir(parents=True, exist_ok=True)

    def one(sc: dict) -> bool:
        cf = cache / f"{sc['id']}.json" if cache else None
        prof = None
        if cf and cf.exists():
            prof = json.loads(cf.read_text())
        else:
            m = re.search(r",\s*([^,]+),\s*([^,]+?)\s*-\s*\d{6}$", sc["customer"]["address"])
            items = [f"{i['description']} ({i['karat']}K, {i['gross_wt']} g)" for i in sc["gold"]["items"]]
            prompt = PROMPT.format(
                sketch=f"a {sc['persona']['occupation']}, aged about {sc['persona']['age']}",
                city=m[1] if m else "an Indian city", state=m[2] if m else "India",
                employment="salaried" if sc["income"]["employment_type"] == "salaried" else "self-employed (runs their own small business)",
                n_items=len(items), ornaments="; ".join(items))
            try:
                prof = _clean(_json(chat(model, prompt)) or {})
            except RuntimeError:
                prof = None
            if cf and prof:
                cf.write_text(json.dumps(prof, indent=2))
        if not prof:
            return False
        apply_profile(sc, prof)
        return True

    with ThreadPoolExecutor(max_workers=max(1, workers)) as ex:
        return sum(ex.map(one, scenarios))
