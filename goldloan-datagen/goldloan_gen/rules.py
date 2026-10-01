"""Business rules (Python side): same numbers as the Rust simulator, one source of truth."""
import json
from pathlib import Path

SPEC = Path(__file__).resolve().parent.parent / "spec"
RULES = json.loads((SPEC / "business_rules.json").read_text())


def max_loan(value: float, rules: dict = RULES) -> tuple[float, float]:
    """Mirror of Rules::max_loan in crates/loop-goldloan-gen/src/rules.rs."""
    prev_cap = 0.0
    for t in rules["gold"]["ltv_tiers"]:
        l = value * t["ltv_pct"] / 100
        cap = t["up_to_loan"]
        if cap is not None and l > cap:
            prev_cap = cap
            continue
        return round(max(l, prev_cap) * 100 + 1e-9) / 100, t["ltv_pct"]
    return 0.0, 0.0
