"""Offline regression/adversarial tests. Passing mocks is NOT Jev accuracy."""
from __future__ import annotations

import base64
import json
import math
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from . import evaluate, jev, luna, media, release, scan
from .policy import CATEGORIES, LUNA_MODEL, MODEL

PROFILE = {"identifiers": ["Zora Example", "aster-machine"], "phones": [], "email_allowed": True}


class ScannerTests(unittest.TestCase):
    def test_email_exception_does_not_clear_secret(self):
        self.assertEqual(scan.rules('Zora.Example@example.invalid', PROFILE), set())
        self.assertIn('identity', scan.rules('Zora Example <hello@example.invalid>', PROFILE))
        key = 'sk' + '-' + 'Ab9' * 12
        self.assertIn('secret', scan.rules(key + ' contact@example.invalid', PROFILE))

    def test_case_unicode_separator_and_nested_encoding(self):
        variants = ['ZORA EXAMPLE', 'Zora_Example', 'Ｚｏｒａ Ｅｘａｍｐｌｅ', 'Zora%20Example', r'Zora\u0020Example',
                    base64.b64encode(b'Zora Example').decode(), b'Zora Example'.hex()]
        for value in variants:
            with self.subTest(value=value):
                self.assertTrue(any(f['rule'] == 'identity' for f in scan.text_findings('sample', value, PROFILE)))
        nested = base64.b64encode(base64.b64encode(b'Zora Example')).decode()
        self.assertTrue(scan.text_findings('sample', nested, PROFILE))

    def test_split_line_identity(self):
        hits = scan.text_findings('sample', 'Zora Exa\nmple', PROFILE)
        self.assertTrue(any(f['rule'] == 'identity' and f['line'] == 0 for f in hits))

    def test_phone_ip_mac_and_home(self):
        phone = '+' + '1 (202)' + ' 555' + '-0199'
        self.assertIn('phone_pattern', scan.rules(phone, PROFILE))
        self.assertNotIn('phone_pattern', scan.rules('2026-10-26 12:40:33 version 1.64.1', PROFILE))
        self.assertNotIn('phone_pattern', scan.rules('M16.2 8.5A6.5 6.5 0 0 0 4.4 6.3', PROFILE))
        self.assertIn('private_ip', scan.rules('10' + '.23.42.9', PROFILE))
        self.assertEqual(scan.rules('127.0.0.1 0.0.0.0', PROFILE), set())
        self.assertIn('mac_address', scan.rules(':'.join(['a1','b2','c3','d4','e5','f6']), PROFILE))
        path = 'C:/' + 'Users/' + 'local-person/project'
        self.assertIn('home_path', scan.rules(path, PROFILE))
        self.assertNotIn('home_path', scan.rules('C:/Users/fixture-user/project', PROFILE))

    def test_no_unc_false_positive_on_integrity_or_escaped_path(self):
        self.assertNotIn('network_path', scan.rules('sha512-abc//abc/xyz==', PROFILE))
        self.assertNotIn('network_path', scan.rules(r'C:\\Temp\\a', PROFILE))
        self.assertIn('network_path', scan.rules('\\\\' + 'private-host' + '\\share', PROFILE))

    def test_unknown_binary_and_archive_cannot_be_approved(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root/'blob.bin').write_bytes(b'\0private payload')
            approval = {'blob.bin': {'sha256': scan.sha((root/'blob.bin').read_bytes()), 'reviewed': True, 'metadata_stripped': True, 'ocr_clean': True}}
            self.assertFalse(scan.inspect(root, PROFILE, approval)['clean'])

    def test_utf16_and_filename_are_scanned(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root/'Zora Example.txt').write_bytes('aster-machine'.encode('utf-16'))
            report = scan.inspect(root, PROFILE)
            self.assertFalse(report['clean'])
            self.assertTrue(any(f['rule'] == 'identity' for f in report['findings']))

    def test_profile_empty_inside_tree_and_bad_schema_fail(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            path = root/'profile.json'
            path.write_text(json.dumps(PROFILE))
            with self.assertRaises(ValueError): scan.load_profile(path, (root,))
            path.write_text(json.dumps({**PROFILE, 'identifiers': []}))
            with self.assertRaises(ValueError): scan.load_profile(path)

    def test_reports_have_no_matched_text(self):
        report = scan.text_findings('sample.txt', 'Zora Example', PROFILE)
        self.assertNotIn('Zora Example', json.dumps(report))


class JevTests(unittest.TestCase):
    def row(self, text='ordinary code', line=1):
        return {'id': 'row'+str(line), 'path': 'a.txt', 'line': line, 'text': text, 'context': [text],
                'file_sha256': 'f'*64, 'line_sha256': scan.sha(text), 'withheld': False}

    def response(self, payload, p=0.01):
        return {'model': MODEL, 'answers': {k: {'type': 'noul', 'noul': p} for k in payload['questions']},
                'usage': {'input_tokens': 100, 'output_tokens': 30}}

    def test_every_line_and_deterministic_order(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            (root/'b.txt').write_text('alpha\n\nbeta\n')
            (root/'a.txt').write_text('gamma\n')
            report, rows = jev.line_feed(root, PROFILE)
            self.assertEqual([(r['path'],r['line']) for r in rows], [('a.txt',1),('b.txt',1),('b.txt',2),('b.txt',3)])
            self.assertEqual(rows, jev.line_feed(root, PROFILE)[1])
            self.assertTrue(report['clean'])

    def test_line_chunking_never_silently_discards(self):
        rows = [self.row(line=i) for i in range(25)]
        chunks = list(jev.batches(rows))
        self.assertEqual([r['id'] for b in chunks for r in b], [r['id'] for r in rows])
        self.assertTrue(all(len(b)<=8 for b in chunks))

    def test_typed_probability_validation(self):
        rows = [self.row()]
        payload = jev.batch_payload(rows)
        for bad in (float('nan'), float('inf'), True, -0.1, 1.1, '0.3'):
            body = self.response(payload)
            body['answers']['l0_name']['noul'] = bad
            with self.assertRaises(ValueError): jev.parse_response(body, rows)
        body = self.response(payload)
        del body['answers']['l0_name']
        with self.assertRaises(ValueError): jev.parse_response(body, rows)

    def test_correct_boundary_and_no_authority(self):
        rows = [self.row()]
        payload = jev.batch_payload(rows)
        self.assertTrue(jev.parse_response(self.response(payload,0.15),rows)[0][0]['would_route'])
        self.assertFalse(jev.parse_response(self.response(payload,0.149),rows)[0][0]['would_route'])
        self.assertEqual(jev.parse_response(self.response(payload),rows)[0][0]['authority'],'none')

    def test_multiline_private_key_is_not_serialized(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            text = 'before\n-----BEGIN ' + 'PRIVATE KEY-----\nDO_NOT_SEND_KEY_BODY\n-----END ' + 'PRIVATE KEY-----\nafter'
            (root/'s.txt').write_text(text)
            _, rows = jev.line_feed(root, PROFILE)
            self.assertEqual(len(rows),len(text.splitlines()))
            self.assertTrue(all(r['withheld'] for r in rows))
            self.assertNotIn('DO_NOT_SEND_KEY_BODY',json.dumps(jev.batch_payload(rows)))

    def test_default_offline_never_opens_network(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)/'tree';root.mkdir();(root/'code.txt').write_text('ordinary code')
            with patch('urllib.request.OpenerDirector.open',side_effect=AssertionError('network')):
                result = jev.shadow(root,PROFILE,Path(d)/'run')
            self.assertEqual(result['mode'],'offline')
            self.assertEqual(result['lines'],1)
            self.assertEqual(result['unmeasured_or_error'],1)
            self.assertFalse(result['graduated'])

    def test_mock_transport_cannot_generate_live_evidence(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)/'tree';root.mkdir();(root/'code.txt').write_text('ordinary code')
            result=jev.shadow(root,PROFILE,Path(d)/'run',consent=True,budget=0.1,transport=self.response)
            row=json.loads((Path(d)/'run/observations.jsonl').read_text())
            self.assertEqual(row['mode'],'mock')
            self.assertEqual(result['mode'],'mock')

    def test_zero_budget_refuses_calls(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)/'tree';root.mkdir();(root/'code.txt').write_text('ordinary code')
            result=jev.shadow(root,PROFILE,Path(d)/'run',consent=True,budget=0,transport=lambda _:self.fail('called'))
            self.assertEqual(result['unmeasured_or_error'],1)

    def test_bad_model_and_usage_rejected(self):
        rows=[self.row()];p=jev.batch_payload(rows)
        for model in ('jev-latest',None):
            b=self.response(p);b['model']=model
            with self.assertRaises(ValueError): jev.parse_response(b,rows)
        b=self.response(p);b['usage']={}
        with self.assertRaises(ValueError): jev.parse_response(b,rows)


class EvaluationTests(unittest.TestCase):
    def test_empty_never_graduates(self):
        self.assertFalse(evaluate.evaluate([],[],[])['eligible_for_additive_routing'])

    def test_wilson_bound_is_not_point_accuracy(self):
        self.assertLess(evaluate.lower_bound(100,100),0.99)
        self.assertGreater(evaluate.lower_bound(500,500),0.99)
        self.assertEqual(evaluate.lower_bound(0,0),0)

    def sample(self):
        text='private personal fact A'
        gold={'id':'1','text':text,'labels':{k:k=='name' for k in CATEGORIES},'source_group':'held-out-file',
              'family':'unique-template','split':'holdout','reviewed_by':'independent reviewer','independent':True,'gold_origin':'human_synthetic'}
        obs={'id':'1','line_sha256':scan.sha(text),'model':MODEL,'policy':jev.fingerprint(),'status':'ok','mode':'live',
             'probabilities':{k:0.9 if k=='name' else 0.01 for k in CATEGORIES}}
        return gold,obs

    def test_missing_mock_stale_and_contamination_never_graduate(self):
        g,o=self.sample()
        self.assertEqual(evaluate.evaluate([g],[],[])['samples'],0)
        self.assertEqual(evaluate.evaluate([g],[{**o,'mode':'mock'}],[])['samples'],0)
        self.assertEqual(evaluate.evaluate([g],[{**o,'policy':'old'}],[])['samples'],0)
        self.assertEqual(evaluate.evaluate([g],[o],['held-out-file'])['samples'],0)

    def test_duplicate_template_does_not_buy_accuracy(self):
        g,o=self.sample()
        g2={**g,'id':'2','text':g['text']+' 123'}
        o2={**o,'id':'2','line_sha256':scan.sha(g2['text'])}
        result=evaluate.evaluate([g,g2],[o,o2],[])
        self.assertEqual(result['samples'],1)
        self.assertEqual(result['duplicates_or_trivial_excluded'],1)

    def test_critical_false_negative_counted(self):
        g,o=self.sample();o['probabilities']={k:0.01 for k in CATEGORIES}
        r=evaluate.evaluate([g],[o],[])
        self.assertEqual(r['false_negatives'],1)
        self.assertIn('critical_false_negative',r['failures'])

    def test_duplicate_observation_ids_are_an_error(self):
        g,o=self.sample()
        with self.assertRaises(ValueError): evaluate.evaluate([g],[o,o],[])


class LunaTests(unittest.TestCase):
    def proposal(self,path,data,new='example-user\n'):
        return {'model':LUNA_MODEL,'thinking':'max','files':[{'file_path':path,'pre_file_sha256':scan.sha(data),
                                                           'replacements':[{'start_line':1,'end_line':1,'new':new}]}]}

    def test_exact_hash_and_single_match(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);data=b'owner = "Zora Example"\n';(root/'a.txt').write_bytes(data)
            p=self.proposal('a.txt',data);c={'a.txt':{'pre_file_sha256':scan.sha(data),'numbered_lines':{'1':data.decode()}}}
            self.assertIn(b'example-user',luna.validate(root,p,c)['a.txt'])
            p['files'][0]['pre_file_sha256']='0'*64
            with self.assertRaises(ValueError): luna.validate(root,p,c)

    def test_ambiguous_path_escape_and_unreviewed_apply_fail(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);data=b'Zora Example Zora Example';(root/'a.txt').write_bytes(data)
            p=self.proposal('a.txt',data);c={'a.txt':{'pre_file_sha256':scan.sha(data),'numbered_lines':{'1':data.decode()}}}
            p['files'][0]['replacements'].append(dict(p['files'][0]['replacements'][0]))
            with self.assertRaises(ValueError): luna.validate(root,p,c)
            p['files'][0]['file_path']='../escape'
            with self.assertRaises(ValueError): luna.validate(root,p,c)
            with self.assertRaises(ValueError): luna.apply(root,p,{'response_sha256':'x'},reviewed_response_sha256='x',proposal_bytes=b'{}')

    def test_mojibake_is_rejected_but_clean_unicode_is_retained(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);data='café · label\n'.encode('utf-8');(root/'a.txt').write_bytes(data)
            c={'a.txt':{'pre_file_sha256':scan.sha(data),'numbered_lines':{'1':data.decode('utf-8')}}}
            p=self.proposal('a.txt',data,new='café · neutral\n')
            self.assertEqual(luna.validate(root,p,c)['a.txt'].decode('utf-8'),'café · neutral\n')
            p['files'][0]['replacements'][0]['new']='café · neutral\n'.encode('utf-8').decode('cp1252')
            with self.assertRaises(ValueError): luna.validate(root,p,c)

    def test_model_is_proposal_only_and_credentials_withheld(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d);key='sk'+'-'+'Ab9'*12;(root/'s.txt').write_text(key)
            with self.assertRaises(ValueError): luna.build_request(root,['s.txt'],PROFILE)


class ReleaseTests(unittest.TestCase):
    def new_source(self,root):
        root.mkdir();env=release.anonymous_env()
        release.git(root,'-c','init.templateDir=','init','--initial-branch=dev',env=env)
        release.git(root,'config','user.name','private owner')
        release.git(root,'config','user.email','private@example.invalid')
        for name,text in {'src/app.txt':'public code\n','.pi/private.json':'Zora Example','notes.md':'aster-machine'}.items():
            p=root/name;p.parent.mkdir(parents=True,exist_ok=True);p.write_text(text)
        release.git(root,'add','--all',env=env)
        release.git(root,'-c','core.hooksPath=','commit','--no-gpg-sign','-m','private source',env=env)

    def test_allowlist_cannot_include_private_runtime(self):
        policy={'include':['*'],'exclude':[]}
        for p in ('.pi/a.json','artifacts/log.txt','.env','src/secret.pem','.worktrees/a.txt'):
            self.assertFalse(release.allowed(p,policy))
        self.assertTrue(release.allowed('src/app.rs',policy))

    def test_path_escape_reserved_and_collision_inputs(self):
        for p in ('../x','/x','C:/x','a\\b','a/../b','.git/../x','NUL.txt','a./x','a//b'):
            with self.subTest(path=p),self.assertRaises(ValueError): release.safe_path(p)

    def test_export_pins_commit_and_never_copies_dirty_or_history(self):
        with tempfile.TemporaryDirectory() as d:
            source=Path(d)/'private';self.new_source(source)
            (source/'src/app.txt').write_text('Zora Example dirty source')
            target=Path(d)/'public'
            result=release.export(source,'HEAD',target,{'include':['src/*'],'exclude':[]})
            self.assertEqual((target/'src/app.txt').read_text(),'public code\n')
            self.assertFalse((target/'.git').exists())
            self.assertEqual(result['exported'],1)
            self.assertIn('Zora Example',(source/'src/app.txt').read_text())

    def test_export_refuses_existing_or_nested_target(self):
        with tempfile.TemporaryDirectory() as d:
            source=Path(d)/'private';self.new_source(source)
            with self.assertRaises(ValueError): release.export(source,'HEAD',source/'candidate',{'include':['src/*'],'exclude':[]})
            with self.assertRaises(ValueError): release.export(source,'HEAD',Path(d),{'include':['src/*'],'exclude':[]})

    def test_anonymous_root_and_reachable_history_are_verified(self):
        with tempfile.TemporaryDirectory() as d:
            root=Path(d)/'public';root.mkdir();(root/'code.txt').write_text('public code\n')
            head=release.init_public(root,PROFILE)
            result=release.verify_git(root,PROFILE)
            self.assertTrue(result['clean']);self.assertEqual(result['head'],head)
            self.assertEqual(release.git(root,'rev-list','--count','HEAD').strip(),b'1')
            release.git(root,'tag','unsafe-tag')
            with self.assertRaises(ValueError): release.verify_git(root,PROFILE)

    def test_update_carries_only_previous_public_history(self):
        with tempfile.TemporaryDirectory() as d:
            old=Path(d)/'old';old.mkdir();(old/'code.txt').write_text('public original\n')
            parent=release.init_public(old,PROFILE)
            new=Path(d)/'new';new.mkdir();(new/'code.txt').write_text('public update\n')
            head=release.init_public(new,PROFILE,previous=old)
            self.assertTrue(release.verify_git(new,PROFILE)['clean'])
            self.assertEqual(release.git(new,'rev-parse','HEAD^').decode().strip(),parent)
            self.assertNotEqual(head,parent)
            self.assertEqual(release.git(new,'rev-list','--count','HEAD').strip(),b'2')

    def test_push_requires_hash_bound_independent_test_approval_before_auth(self):
        root=Path('unused');head='1'*40;url=b'https://github.com/neutral-team/workbench.git\n'
        with patch.object(release,'git',return_value=url), patch.object(release,'verify_git',return_value={'clean':True,'head':head,'manifest':'tree'}), patch('subprocess.run',side_effect=AssertionError('auth/push attempted')):
            with self.assertRaises(ValueError): release.push_public(root,'neutral-team','workbench',PROFILE,{},head,True)
            with self.assertRaises(ValueError): release.push_public(root,'neutral-team','workbench',PROFILE,{},head,True,{'head':head,'manifest':'stale'})

    def test_push_off_by_default(self):
        with tempfile.TemporaryDirectory() as d:
            with patch('subprocess.run',side_effect=AssertionError('network')):
                with self.assertRaises(ValueError): release.push_public(Path(d),'anonymous-team','repo',PROFILE,{},'0'*40)


if __name__ == '__main__':
    unittest.main()
