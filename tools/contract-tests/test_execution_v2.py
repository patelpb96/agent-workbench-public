"""V2 schema/generator shape gates, not real-provider or production migration acceptance."""
import copy
import json
import unittest
from pathlib import Path
from jsonschema import Draft202012Validator, FormatChecker, ValidationError
from generate_interfaces import rust_type, pascal

ROOT = Path(__file__).resolve().parents[2]
SCHEMA = json.loads((ROOT/'contracts/execution-v2.schema.json').read_text(encoding='utf-8'))
ID = '00000000-0000-4000-8000-000000000001'

def check(name,value):
    root = dict(SCHEMA)
    root.pop('oneOf',None)
    root['$ref']='#/$defs/'+name
    Draft202012Validator(root,format_checker=FormatChecker()).validate(value)

def start():
    return dict(contract_version=2,workspace_id=ID,session_id=ID,request_id=ID,prompt='Hello',profile_id='codex_text_v1',confirmed=True)

class ExecutionV2Tests(unittest.TestCase):
    def test_schema_is_valid_and_distinct_from_frozen_v1(self):
        Draft202012Validator.check_schema(SCHEMA)
        original=json.loads((ROOT/'contracts/execution.schema.json').read_text(encoding='utf-8'))
        self.assertEqual(original['$defs']['bootstrap_request']['properties']['contract_version']['const'],1)
        self.assertEqual(SCHEMA['$defs']['bootstrap_request']['properties']['contract_version']['const'],2)
        self.assertNotEqual(original['$id'],SCHEMA['$id'])
        self.assertIn('queued_run',original['$defs'])
        self.assertNotIn('queued_run',SCHEMA['$defs'])

    def test_codex_start_required_unknown_and_fixed_profile_matrix(self):
        value=start(); check('start_codex_run_request',value)
        for key in value:
            missing=copy.deepcopy(value); del missing[key]
            with self.subTest(missing=key),self.assertRaises(ValidationError): check('start_codex_run_request',missing)
        for key in ['path','cwd','args','env','model_id','thread_id','resume','scenario','provider_id']:
            with self.subTest(extra=key),self.assertRaises(ValidationError): check('start_codex_run_request',{**value,key:'x'})
        for changes in [dict(contract_version=1),dict(confirmed=False),dict(confirmed=1),dict(profile_id='unrestricted'),dict(prompt='')]:
            with self.subTest(changes=changes),self.assertRaises(ValidationError): check('start_codex_run_request',{**value,**changes})

    def test_codegen_preserves_bool_constants_and_legal_model_enum_identifier(self):
        self.assertEqual(rust_type({'const':True}),'bool')
        self.assertEqual(rust_type({'const':2}),'u64')
        self.assertEqual(pascal('gpt-6.1-sol'),'Gpt61Sol')
        self.assertEqual(pascal('run_state_changed'),'RunStateChanged')

    def test_upgrade_bootstrap_has_no_implicit_workspace_or_live_authority(self):
        check('bootstrap_response',dict(contract_version=2,storage_state='upgrade_required',upgrade_id=ID,snapshot=None))
        check('confirm_upgrade_request',dict(contract_version=2,upgrade_id=ID,request_id=ID,confirmed=True))
        with self.assertRaises(ValidationError): check('confirm_upgrade_request',dict(contract_version=2,upgrade_id=ID,request_id=ID,confirmed=False))

    def test_required_unknown_usage_counters_remain_explicit_null_not_zero(self):
        value=dict(input_tokens=None,output_tokens=None,estimated=False,cached_input_tokens=None,cache_write_input_tokens=None,reasoning_output_tokens=None)
        check('usage',value)
        for key in value:
            missing=copy.deepcopy(value); del missing[key]
            with self.subTest(key=key),self.assertRaises(ValidationError): check('usage',missing)
        for n in [-1,9007199254740992,1.5]:
            with self.subTest(n=n),self.assertRaises(ValidationError): check('usage',{**value,'input_tokens':n})

    def test_distinct_command_names_limits_and_no_illegal_cancel_success(self):
        self.assertTrue(all(n.endswith('_v2') for n in SCHEMA['$defs']['command_name']['enum']))
        self.assertIn('start_codex_run_v2',SCHEMA['$defs']['command_name']['enum'])
        self.assertNotIn('succeeded',SCHEMA['x-run-transitions']['cancel_requested'])
        self.assertEqual(SCHEMA['x-limits']['codex_prompt_bytes'],1024)
        self.assertEqual(SCHEMA['x-limits']['prompt_bytes'],65536)
        self.assertEqual(SCHEMA['x-limits']['codex_completion_chunk_bytes'],2048)
        self.assertEqual(SCHEMA['x-notification'],'execution_changed_v2')

    def test_codex_text_v2_context_shape_and_limits(self):
        self.assertEqual(SCHEMA['$defs']['codex_profile_id']['enum'],['codex_text_v1','codex_text_v2'])
        self.assertEqual(SCHEMA['x-limits']['codex_context_runs'],6)
        self.assertEqual(SCHEMA['x-limits']['codex_context_bytes'],8192)
        self.assertEqual(SCHEMA['x-profile-v2']['id'],'codex_text_v2')
        # Old requests stay valid byte-for-byte; context is optional and bounded to 1..=6 ids.
        check('start_codex_run_request',start())
        with_context={**start(),'profile_id':'codex_text_v2','context_run_ids':[ID]}
        check('start_codex_run_request',with_context)
        for bad in ([],[ID]*7,['not-an-id']):
            with self.subTest(ids=bad),self.assertRaises(ValidationError):
                check('start_codex_run_request',{**with_context,'context_run_ids':bad})
        # The profile and the context list come as a pair, in the schema itself.
        with self.assertRaises(ValidationError): check('start_codex_run_request',{**start(),'context_run_ids':[ID]})
        with self.assertRaises(ValidationError): check('start_codex_run_request',{**start(),'profile_id':'codex_text_v2'})
        preview=dict(contract_version=2,workspace_id=ID,session_id=ID,prompt='hi',context_run_ids=[ID])
        check('preview_codex_context_request',preview)
        with self.assertRaises(ValidationError): check('preview_codex_context_request',{**preview,'confirmed':True})
        with self.assertRaises(ValidationError): check('preview_codex_context_request',{k:v for k,v in preview.items() if k!='context_run_ids'})
        answer=dict(contract_version=2,workspace_id=ID,text='x',bytes=1,carried=0,trimmed=False)
        check('codex_context_preview',answer)
        with self.assertRaises(ValidationError): check('codex_context_preview',{**answer,'bytes':8193})

if __name__=='__main__': unittest.main()
