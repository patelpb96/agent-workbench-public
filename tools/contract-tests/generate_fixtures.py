"""Generate fixture-only envelopes deterministically; never run a provider.

Source: contracts/fake-scenarios.json. Generated UUIDs have v4-shaped bits for
wire parity but are deliberately predictable and MUST NOT be used at runtime.
No clock, random source, environment, credentials, or network is consulted.
"""

from __future__ import annotations

import argparse
import json
import re
from datetime import datetime, timedelta, timezone
from pathlib import Path
from uuid import UUID

ROOT = Path(__file__).resolve().parents[2]
SOURCE = ROOT / "contracts" / "fake-scenarios.json"
OUTPUT = ROOT / "contracts" / "fixtures"


def fixture_id(value: int) -> str:
    return str(UUID(int=value, version=4))


def timestamp(base: datetime, offset_ms: int) -> str:
    return (base + timedelta(milliseconds=offset_ms)).isoformat(timespec="milliseconds").replace("+00:00", "Z")


def build_fixtures(source: dict) -> dict[str, dict]:
    if source["fixture_version"] != 1:
        raise ValueError("Unsupported fixture source version.")
    base = datetime.fromisoformat(source["base_time"].replace("Z", "+00:00"))
    if base.tzinfo != timezone.utc:
        raise ValueError("Fixture base time must be UTC.")
    result = {}
    for case_index, case in enumerate(source["cases"], start=1):
        name = case["name"]
        if not re.fullmatch(r"fake-[a-z0-9-]+", name) or name in result:
            raise ValueError("Unsafe or duplicate fixture name.")
        if not case["steps"]:
            raise ValueError("Fixture must contain steps.")
        identity = {
            "contract_version": 1,
            "workspace_id": fixture_id(1),
            "session_id": fixture_id(2),
            "run_id": fixture_id(case_index + 2),
            "provider_id": "fake",
        }
        events, state = [], None
        for seq, step in enumerate(case["steps"], start=1):
            occurred_at = timestamp(base, step.get("at_ms", (seq - 1) * source["step_ms"]))
            event = {
                **identity,
                "event_id": fixture_id((case_index << 96) + seq),
                "source": "fixture",
                "seq": seq,
                "occurred_at": occurred_at,
            }
            if "state" in step:
                next_state = step["state"]
                if state is None:
                    event.update(kind="run_created", payload={"state": next_state})
                else:
                    payload = {"from": state, "to": next_state, "error": step.get("error")}
                    event.update(kind="run_state_changed", payload=payload)
                state = next_state
            elif step.get("recover") is True:
                event.update(kind="run_recovered", source="system", payload={"previous_state": state, "state": "interrupted"})
                state = "interrupted"
            else:
                event.update(kind=step["kind"], payload=step["payload"])
            events.append(event)
        usage = next((event["payload"] for event in reversed(events) if event["kind"] == "usage_updated"), {"input_tokens": None, "output_tokens": None, "estimated": False})
        terminal_error = events[-1]["payload"].get("error") if state == "failed" else None
        result[name] = {
            "scenario": case["scenario"],
            "events": events,
            "snapshot": {**identity, "state": state, "last_seq": len(events), "created_at": events[0]["occurred_at"], "updated_at": events[-1]["occurred_at"], "usage": usage, "terminal_error": terminal_error, "enqueue_order": case_index},
        }
    return result


def build_command_fixtures(fixtures: dict, layout: dict) -> dict:
    """Wire examples, not executed commands or an integration simulation."""
    success = fixtures["fake-success"]
    snapshot = success["snapshot"]
    workspace_id, session_id = snapshot["workspace_id"], snapshot["session_id"]
    session = {"contract_version": 1, "workspace_id": workspace_id, "session_id": session_id, "provider_id": "fake", "label": "SIMULATED session", "created_at": snapshot["created_at"]}
    workspace = {"contract_version": 1, "workspace": {"workspace_id": workspace_id, "label": "Simulated workspace", "created_at": snapshot["created_at"]}, "sessions": [session], "runs": [fixture["snapshot"] for fixture in fixtures.values()], "layout": layout, "layout_warning": None, "execution_available": True}
    queued = {**snapshot, "state": "queued", "last_seq": 1, "updated_at": snapshot["created_at"], "usage": {"input_tokens": None, "output_tokens": None, "estimated": False}}
    start = {"contract_version": 1, "workspace_id": workspace_id, "session_id": session_id, "request_id": fixture_id(1002), "provider_id": "fake", "prompt": "Simulated input only.", "scenario": "success"}
    cancelled = fixtures["fake-cancelled"]["snapshot"]
    cases = []
    def add(definition, value):
        cases.append({"definition": definition, "value": value})
    add("bootstrap_request", {"contract_version": 1})
    add("workspace_request", {"contract_version": 1, "workspace_id": workspace_id})
    add("workspace_snapshot", workspace)
    add("create_session_request", {"contract_version": 1, "workspace_id": workspace_id, "request_id": fixture_id(1001), "label": session["label"]})
    add("create_session_response", {"contract_version": 1, "session": session, "replayed": False})
    add("start_fake_run_request", start)
    add("start_fake_run_response", {"contract_version": 1, "run": queued, "replayed": False})
    add("cancel_run_request", {"contract_version": 1, "workspace_id": workspace_id, "run_id": cancelled["run_id"], "request_id": fixture_id(1003)})
    add("cancel_run_response", {"contract_version": 1, "run": cancelled, "replayed": False})
    add("event_page_request", {"contract_version": 1, "workspace_id": workspace_id, "run_id": snapshot["run_id"], "after_seq": 0, "through_seq": None, "limit": 200})
    add("event_page", {"contract_version": 1, "workspace_id": workspace_id, "run_id": snapshot["run_id"], "events": success["events"], "last_seq": snapshot["last_seq"], "through_seq": snapshot["last_seq"], "next_after_seq": snapshot["last_seq"], "has_more": False})
    add("save_layout_request", {"contract_version": 1, "workspace_id": workspace_id, "request_id": fixture_id(1004), "expected_revision": layout["revision"], "layout": layout})
    add("save_layout_response", {"contract_version": 1, "layout": {**layout, "revision": layout["revision"] + 1}, "replayed": False})
    add("execution_changed", {"contract_version": 1, "workspace_id": workspace_id})
    return {"cases": cases, "native_rejections": [{"value": {**start, "provider_id": provider}, "code": "provider_execution_disabled"} for provider in ["codex", "claude"]]}


def render(fixture: dict) -> str:
    return json.dumps(fixture, indent=2, ensure_ascii=False, allow_nan=False) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="Fail on drift; never write files.")
    args = parser.parse_args()
    fixtures = build_fixtures(json.loads(SOURCE.read_text(encoding="utf-8")))
    fixtures["commands"] = build_command_fixtures(fixtures, json.loads((OUTPUT / "layout.json").read_text(encoding="utf-8")))
    expected_paths = {OUTPUT / f"{name}.json" for name in fixtures}
    unexpected = set(OUTPUT.glob("fake-*.json")) - expected_paths
    if unexpected:
        raise ValueError(f"Unexpected generated fixtures; review/remove manually: {sorted(path.name for path in unexpected)}")
    drift = []
    for name, fixture in fixtures.items():
        path = OUTPUT / f"{name}.json"
        content = render(fixture)
        if args.check:
            if not path.is_file() or path.read_text(encoding="utf-8") != content:
                drift.append(path.name)
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("w", encoding="utf-8", newline="\n") as output:
                output.write(content)
    if drift:
        print("Generated fixture drift: " + ", ".join(drift))
        return 1
    print(f"{'Checked' if args.check else 'Generated'} {len(fixtures)} deterministic fixture files.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
