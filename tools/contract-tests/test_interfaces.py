"""Wire/source drift and interface matrices; not runtime/store acceptance."""
import copy
import json
import subprocess
import sys
import unittest

from generate_fixtures import SOURCE, build_command_fixtures, build_fixtures, fixture_id, render
from test_contract import ROOT, SCHEMA, validate_shape, validate_layout
from jsonschema import ValidationError


class InterfaceTests(unittest.TestCase):
    def test_command_examples_and_required_unknown_field_matrices(self):
        fixture = json.loads((ROOT / 'contracts/fixtures/commands.json').read_text(encoding='utf-8'))
        for case in fixture['cases']:
            name, value = case['definition'], case['value']
            with self.subTest(definition=name):
                validate_shape(name, value)
                extra = {**value, 'unexpected': True}
                with self.assertRaises(ValidationError):
                    validate_shape(name, extra)
                for field in SCHEMA['$defs'][name]['required']:
                    missing = copy.deepcopy(value)
                    del missing[field]
                    with self.subTest(missing=field), self.assertRaises(ValidationError):
                        validate_shape(name, missing)

    def test_command_fixture_is_generated_without_drift(self):
        fixtures = build_fixtures(json.loads(SOURCE.read_text(encoding='utf-8')))
        layout = json.loads((ROOT / 'contracts/fixtures/layout.json').read_text(encoding='utf-8'))
        expected = render(build_command_fixtures(fixtures, layout))
        self.assertEqual(expected, (ROOT / 'contracts/fixtures/commands.json').read_text(encoding='utf-8'))

    def test_generated_native_and_frontend_types_have_no_drift(self):
        result = subprocess.run([sys.executable, str(ROOT / 'tools/contract-tests/generate_interfaces.py'), '--check'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_control_reserve_and_queue_limits_are_explicit(self):
        limits = SCHEMA['x-limits']
        scopes = SCHEMA['$defs']['truncation_scope']['enum']
        self.assertGreaterEqual(limits['reserved_events'], len(scopes) + 2)
        self.assertGreaterEqual(limits['control_pending_bytes'], limits['reserved_events'] * limits['nontext_event_bytes'])
        self.assertGreater(limits['control_mailbox'], 0)
        self.assertGreater(limits['command_mailbox'], limits['control_mailbox'])
        self.assertLessEqual(limits['queued_runs'], limits['workspace_runs'])
        self.assertGreaterEqual(limits['cancel_receipts'], limits['queued_runs'] + 1)
        self.assertNotIn('succeeded', SCHEMA['x-run-transitions']['cancel_requested'])

    def test_uuid_variant_matrix_matches_native_regression(self):
        for nibble in range(16):
            identity = f'00000000-0000-4000-{nibble:x}000-000000000001'
            with self.subTest(nibble=nibble):
                if 8 <= nibble <= 11:
                    validate_shape('id', identity)
                else:
                    with self.assertRaises(ValidationError):
                        validate_shape('id', identity)

    def test_layout_cross_kind_collision_and_self_reference_matrix(self):
        baseline = json.loads((ROOT / 'contracts/fixtures/layout.json').read_text(encoding='utf-8'))
        for parent_reference in [False, True]:
            layout = copy.deepcopy(baseline)
            panel = layout['panels'][0]
            layout['containers'] = [{'container_id': panel['panel_id'], 'geometry': panel['geometry']}]
            if parent_reference:
                panel['container_id'] = panel['panel_id']
            with self.subTest(parent_reference=parent_reference), self.assertRaises(ValueError):
                validate_layout(layout, layout['workspace_id'], {panel['session_id']})
        layout = copy.deepcopy(baseline)
        panel = layout['panels'][0]
        layout['containers'] = [{'container_id': fixture_id(8), 'geometry': panel['geometry']}]
        panel['container_id'] = fixture_id(8)
        validate_layout(layout, layout['workspace_id'], {panel['session_id']})

    def test_disabled_provider_cases_require_native_rejection(self):
        fixture = json.loads((ROOT / 'contracts/fixtures/commands.json').read_text(encoding='utf-8'))
        self.assertEqual({case['value']['provider_id'] for case in fixture['native_rejections']}, {'codex', 'claude'})
        for case in fixture['native_rejections']:
            validate_shape('start_fake_run_request', case['value'])
            self.assertEqual(case['code'], 'provider_execution_disabled')
        # Schema compatibility is not execution permission. Rust unit tests
        # consume these same examples and exercise the native admission helper.


if __name__ == '__main__':
    unittest.main()
