"""Honest shadow graduation: independent held-out labels, not Noul confidence."""
from __future__ import annotations

import json
import math
import re

from .jev import fingerprint
from .policy import CATEGORIES, MODEL, REVIEW_THRESHOLD
from .scan import sha


def lower_bound(successes: int, total: int) -> float:
    """One-sided 95% Wilson lower confidence bound (z = 1.644854)."""
    if not total:
        return 0.0
    z = 1.6448536269514722
    p = successes / total
    return (p + z*z/(2*total) - z*math.sqrt(p*(1-p)/total + z*z/(4*total*total))) / (1+z*z/total)


def dedupe_key(text: str) -> str:
    # Literal/number variations of one synthetic template do not buy samples.
    text = re.sub(r"[\"'][^\"']*[\"']", "<literal>", text.casefold())
    text = re.sub(r"\d+", "<n>", text)
    return sha(re.sub(r"\s+", "", text))


def evaluate(gold: list[dict], observations: list[dict], tuning_groups: list[str]) -> dict:
    failures = []
    if not gold:
        failures.append("empty_gold")
    by_id = {}
    for item in observations:
        if item.get("id") in by_id:
            raise ValueError("Duplicate observation IDs")
        by_id[item.get("id")] = item
    groups, keys, ids, families = set(tuning_groups), set(), set(), set()
    measured = []
    excluded = 0
    for row in gold:
        required = {"id", "text", "labels", "source_group", "family", "split", "reviewed_by", "independent", "gold_origin"}
        if set(row) != required or set(row["labels"]) != set(CATEGORIES) or any(type(v) is not bool for v in row["labels"].values()):
            raise ValueError("Invalid gold row schema")
        if row["id"] in ids:
            raise ValueError("Duplicate gold IDs")
        ids.add(row["id"])
        if row["split"] != "holdout" or row["source_group"] in groups or row["independent"] is not True or not row["reviewed_by"]:
            failures.append("unreviewed_or_tuning_contamination")
            continue
        if row["gold_origin"] not in {"human_real", "human_synthetic"} or not row["source_group"] or not row["family"]:
            raise ValueError("Independent source/family provenance required")
        key = dedupe_key(row["text"])
        if key in keys or row["family"] in families or len(row["text"].strip()) < 8:
            excluded += 1
            continue
        keys.add(key)
        families.add(row["family"])
        observation = by_id.get(row["id"], {})
        if (observation.get("status") != "ok" or observation.get("mode") != "live" or
                observation.get("model") != MODEL or observation.get("policy") != fingerprint() or
                observation.get("line_sha256") != sha(row["text"])):
            failures.append("missing_error_mock_or_stale_observation")
            continue
        probabilities = observation.get("probabilities")
        if not isinstance(probabilities, dict) or set(probabilities) != set(CATEGORIES) or any(
            type(v) not in {int, float} or not math.isfinite(v) or not 0 <= v <= 1 for v in probabilities.values()
        ):
            failures.append("invalid_probabilities")
            continue
        measured.append((row, observation))
    correct = 0
    false_negatives = 0
    positives = {k: 0 for k in CATEGORIES}
    recalled = {k: 0 for k in CATEGORIES}
    for row, observation in measured:
        predicted = any(p >= REVIEW_THRESHOLD for p in observation["probabilities"].values())
        actual = any(row["labels"].values())
        correct += predicted == actual
        false_negatives += actual and not predicted
        for category in CATEGORIES:
            if row["labels"][category]:
                positives[category] += 1
                recalled[category] += observation["probabilities"][category] >= REVIEW_THRESHOLD
    n = len(measured)
    if n < 500:
        failures.append("fewer_than_500_independent_nontrivial_samples")
    if lower_bound(correct, n) < 0.99:
        failures.append("accuracy_95pct_lower_bound_below_99pct")
    for category in CATEGORIES:
        if positives[category] < 300 or lower_bound(recalled[category], positives[category]) < 0.99:
            failures.append("insufficient_recall_evidence_" + category)
    if false_negatives:
        failures.append("critical_false_negative")
    return {"schema": 1, "eligible_for_additive_routing": not failures, "mode": "shadow",
            "publication_authority": "none", "policy": fingerprint(), "model": MODEL,
            "samples": n, "duplicates_or_trivial_excluded": excluded,
            "accuracy": correct/n if n else None, "accuracy_lower_95_one_sided": lower_bound(correct, n),
            "false_negatives": false_negatives,
            "per_category": {k: {"positives": positives[k], "recalled": recalled[k],
                                  "recall_lower_95_one_sided": lower_bound(recalled[k], positives[k])} for k in CATEGORIES},
            "failures": sorted(set(failures)),
            "gold_digest": sha(json.dumps(gold, sort_keys=True)),
            "observations_digest": sha(json.dumps(observations, sort_keys=True)),
            "notice": "Independent label quality is an external obligation. No automatic promotion is performed."}
