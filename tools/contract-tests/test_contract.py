"""Executable contract examples, NOT runtime/persistence acceptance tests.

Uses the already-pinned jsonschema in tools/ui-automation/.venv; no provider,
network, desktop, database, or agent subprocess is invoked.
"""

from __future__ import annotations

import copy
import json
import math
import unittest
from pathlib import Path

from jsonschema import Draft202012Validator, FormatChecker, ValidationError

from generate_fixtures import SOURCE, build_fixtures, render

ROOT = Path(__file__).resolve().parents[2]
SCHEMA = json.loads((ROOT / "contracts/execution.schema.json").read_text(encoding="utf-8"))
TRANSITIONS = SCHEMA["x-run-transitions"]
TERMINAL = {name for name, destinations in TRANSITIONS.items() if not destinations}
IDENTITY = ("workspace_id", "session_id", "run_id", "provider_id")


def finite_values(value):
    if isinstance(value, float) and not math.isfinite(value):
        raise ValueError("Non-finite JSON number.")
    if isinstance(value, dict):
        for item in value.values():
            finite_values(item)
    elif isinstance(value, list):
        for item in value:
            finite_values(item)


def validate_shape(name, value):
    finite_values(value)
    schema = {"$schema": SCHEMA["$schema"], "$defs": SCHEMA["$defs"], "$ref": f"#/$defs/{name}"}
    Draft202012Validator(schema, format_checker=FormatChecker()).validate(value)


def validate_stream(fixture):
    events, snapshot = fixture["events"], fixture["snapshot"]
    if not events:
        raise ValueError("Empty event stream.")
    state, seen = None, set()
    for expected_seq, event in enumerate(events, start=1):
        validate_shape("run_event", event)
        if event["seq"] != expected_seq or event["event_id"] in seen:
            raise ValueError("Event sequence gap/duplicate identity.")
        seen.add(event["event_id"])
        if any(event[key] != events[0][key] for key in IDENTITY):
            raise ValueError("Run identity changed.")
        if state in TERMINAL:
            raise ValueError("Event after terminal state.")
        payload, kind = event["payload"], event["kind"]
        if expected_seq == 1 and kind != "run_created":
            raise ValueError("First event must create a queued run.")
        if kind == "run_created":
            if state is not None:
                raise ValueError("Duplicate run creation.")
            state = payload["state"]
        elif kind in ("run_state_changed", "run_recovered"):
            previous = payload["from"] if kind == "run_state_changed" else payload["previous_state"]
            destination = payload["to"] if kind == "run_state_changed" else payload["state"]
            if previous != state or destination not in TRANSITIONS[state]:
                raise ValueError("Illegal or stale transition.")
            if event["provider_id"] == "fake" and destination == "waiting_approval":
                raise ValueError("Fake provider cannot create genuine approval state.")
            state = destination
        elif kind == "text_delta":
            if state not in {"running", "waiting_input", "cancel_requested"}:
                raise ValueError("Text emitted outside active execution.")
            if len(payload["text"].encode("utf-8")) > 16384:
                raise ValueError("Text exceeds UTF-8 byte limit.")
    validate_shape("run_snapshot", snapshot)
    if any(snapshot[key] != events[0][key] for key in IDENTITY):
        raise ValueError("Snapshot identity mismatch.")
    if snapshot["state"] != state or snapshot["last_seq"] != events[-1]["seq"]:
        raise ValueError("Snapshot/event mismatch.")
    if snapshot["created_at"] != events[0]["occurred_at"] or snapshot["updated_at"] != events[-1]["occurred_at"]:
        raise ValueError("Snapshot timestamps do not match committed stream.")


def validate_layout(layout, workspace_id, session_ids):
    validate_shape("layout", layout)
    if layout["workspace_id"] != workspace_id:
        raise ValueError("Cross-workspace layout.")
    panel_ids = [panel["panel_id"] for panel in layout["panels"]]
    container_ids = [container["container_id"] for container in layout["containers"]]
    all_ids = panel_ids + container_ids
    if len(set(all_ids)) != len(all_ids):
        raise ValueError("Duplicate layout identity.")
    sessions = [panel["session_id"] for panel in layout["panels"]]
    if len(set(sessions)) != len(sessions) or not set(sessions) <= set(session_ids):
        raise ValueError("Duplicate/dangling session panel.")
    for panel in layout["panels"]:
        if panel["container_id"] is not None and panel["container_id"] not in container_ids:
            raise ValueError("Dangling/cyclic container reference.")


class ContractTests(unittest.TestCase):
    def setUp(self):
        self.source = json.loads(SOURCE.read_text(encoding="utf-8"))
        self.fixtures = build_fixtures(self.source)
        self.success = copy.deepcopy(self.fixtures["fake-success"])
        self.layout = json.loads((ROOT / "contracts/fixtures/layout.json").read_text(encoding="utf-8"))

    def test_schema_is_valid_and_transition_matrix_covers_every_state(self):
        Draft202012Validator.check_schema(SCHEMA)
        validator = Draft202012Validator(SCHEMA, format_checker=FormatChecker())
        for value in [self.layout, self.success["snapshot"], self.success["events"][0]]:
            validator.validate(value)
        with self.assertRaises(ValidationError):
            validator.validate({"arbitrary": "unrecognized wire shape"})
        self.assertEqual(set(TRANSITIONS), set(SCHEMA["$defs"]["run_state"]["enum"]))
        self.assertEqual(TERMINAL, {"succeeded", "failed", "cancelled", "interrupted"})
        for state, destinations in TRANSITIONS.items():
            self.assertEqual(len(destinations), len(set(destinations)))
            self.assertLessEqual(set(destinations), set(TRANSITIONS))
            if state not in TERMINAL:
                self.assertIn("interrupted", destinations)

    def test_all_generated_fixtures_validate_and_have_unique_event_ids(self):
        seen = set()
        for name, fixture in self.fixtures.items():
            with self.subTest(name=name):
                validate_stream(fixture)
                ids = {event["event_id"] for event in fixture["events"]}
                self.assertFalse(seen & ids)
                seen |= ids

    def test_generation_is_reproducible_and_committed_files_have_no_drift(self):
        self.assertEqual(self.fixtures, build_fixtures(copy.deepcopy(self.source)))
        for name, fixture in self.fixtures.items():
            self.assertEqual((ROOT / "contracts/fixtures" / f"{name}.json").read_text(encoding="utf-8"), render(fixture))

    def test_generator_rejects_duplicate_or_unsafe_names(self):
        for name in ["../escape", "fake/escape", "fake-success"]:
            with self.subTest(name=name):
                source = copy.deepcopy(self.source)
                source["cases"][1]["name"] = name
                with self.assertRaises(ValueError):
                    build_fixtures(source)

    def test_envelope_mutation_matrix_is_rejected(self):
        mutations = [
            ("contract_version", 2), ("seq", 0), ("seq", 9007199254740992),
            ("event_id", "not-a-uuid"), ("occurred_at", "not-a-time"),
            ("source", "provider"), ("unexpected_secret", "must not be accepted"),
            ("kind", "execute_shell"),
        ]
        for key, value in mutations:
            with self.subTest(field=key, value=value):
                fixture = copy.deepcopy(self.success)
                fixture["events"][0][key] = value
                with self.assertRaises((ValueError, ValidationError)):
                    validate_stream(fixture)

    def test_sequence_identity_and_snapshot_mutation_matrix_is_rejected(self):
        def mutate(fixture, mode):
            if mode == "gap": fixture["events"][2]["seq"] = 4
            elif mode == "duplicate": fixture["events"][2]["event_id"] = fixture["events"][1]["event_id"]
            elif mode == "workspace": fixture["events"][2]["workspace_id"] = fixture["events"][2]["session_id"]
            elif mode == "stale_from": fixture["events"][2]["payload"]["from"] = "queued"
            elif mode == "snapshot_state": fixture["snapshot"]["state"] = "failed"
            elif mode == "snapshot_seq": fixture["snapshot"]["last_seq"] -= 1
            elif mode == "snapshot_time": fixture["snapshot"]["updated_at"] = fixture["snapshot"]["created_at"]
        for mode in ["gap", "duplicate", "workspace", "stale_from", "snapshot_state", "snapshot_seq", "snapshot_time"]:
            with self.subTest(mode=mode):
                fixture = copy.deepcopy(self.success)
                mutate(fixture, mode)
                with self.assertRaises(ValueError):
                    validate_stream(fixture)

    def test_illegal_transition_matrix_is_rejected(self):
        for destination in ["succeeded", "running", "waiting_approval"]:
            with self.subTest(destination=destination):
                fixture = copy.deepcopy(self.success)
                fixture["events"][1]["payload"]["to"] = destination
                with self.assertRaises(ValueError):
                    validate_stream(fixture)
        fixture = copy.deepcopy(self.success)
        fixture["events"][-1]["payload"]["to"] = "waiting_approval"
        with self.assertRaisesRegex(ValueError, "genuine approval"):
            validate_stream(fixture)
        fixture = copy.deepcopy(self.success)
        fixture["events"].append({**fixture["events"][3], "seq": 7, "event_id": "ffffffff-0000-4000-8000-000000000001"})
        with self.assertRaisesRegex(ValueError, "after terminal"):
            validate_stream(fixture)

    def test_utf8_byte_limit_is_not_confused_with_character_limit(self):
        self.success["events"][3]["payload"]["text"] = "😀" * 4097
        with self.assertRaisesRegex(ValueError, "UTF-8 byte limit"):
            validate_stream(self.success)

    def test_unknown_usage_stays_null_and_recovery_is_system_interruption(self):
        usage = self.success["events"][4]["payload"]
        self.assertIsNone(usage["input_tokens"])
        self.assertIsNone(usage["output_tokens"])
        recovered = self.fixtures["fake-recovered"]["events"][-1]
        self.assertEqual((recovered["source"], recovered["payload"]["state"]), ("system", "interrupted"))

    def test_layout_fixture_validates(self):
        validate_layout(self.layout, self.success["snapshot"]["workspace_id"], [self.success["snapshot"]["session_id"]])

    def test_layout_mutation_matrix_is_rejected(self):
        for mode in ["duplicate_panel", "duplicate_session", "dangling_container", "cross_workspace", "container_parent", "nonfinite", "secret"]:
            with self.subTest(mode=mode):
                layout = copy.deepcopy(self.layout)
                if mode == "duplicate_panel": layout["panels"].append(copy.deepcopy(layout["panels"][0]))
                elif mode == "duplicate_session":
                    panel = copy.deepcopy(layout["panels"][0]); panel["panel_id"] = "ffffffff-0000-4000-8000-000000000001"; layout["panels"].append(panel)
                elif mode == "dangling_container": layout["panels"][0]["container_id"] = layout["panels"][0]["panel_id"]
                elif mode == "cross_workspace": layout["workspace_id"] = layout["panels"][0]["panel_id"]
                elif mode == "container_parent": layout["containers"].append({"container_id": "ffffffff-0000-4000-8000-000000000001", "geometry": layout["panels"][0]["geometry"], "parent_id": layout["panels"][0]["panel_id"]})
                elif mode == "nonfinite": layout["panels"][0]["geometry"]["x"] = float("nan")
                elif mode == "secret": layout["api_key"] = "must not be accepted"
                with self.assertRaises((ValueError, ValidationError)):
                    validate_layout(layout, self.success["snapshot"]["workspace_id"], [self.success["snapshot"]["session_id"]])


if __name__ == "__main__":
    unittest.main()
