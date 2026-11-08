#!/usr/bin/env python3
"""Synthetic script checks only; never invokes Cargo or product binaries."""
from pathlib import Path
import hashlib
import importlib.util
import json
import subprocess
import tempfile
from unittest import mock
HERE=Path(__file__).resolve().parent

def module(name, path):
    spec=importlib.util.spec_from_file_location(name, path); value=importlib.util.module_from_spec(spec);spec.loader.exec_module(value);return value

run=module('gate_run', HERE/'run_stage.py');ready=module('ready20', HERE/'readiness20.py');inv=module('source_inventory', HERE/'inventory.py')
checks=[]
def mark(name): checks.append(name)

assert inv.cfg('all(feature = "internal-tests", any(feature = "vector-persist", target_os = "windows"))', {'internal-tests','vector-persist'})
assert not inv.cfg('all(feature = "internal-tests", feature = "vector-persist")', {'internal-tests'})
mark('compound cfg feature tier')
text='     Running tests/a.rs (/tmp/a)\nrunning 1 test\ntest x ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n     Running tests/b.rs (/tmp/b)\ntest result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
results=run.parse_results(text);assert results['a']['passed']==1 and results['b']['passed']==0
expected={'targets':[{'target':key,'tiers':{'default':{'minimum_passed':1 if key=='a' else 0}}} for key in ['a','b']]}
assert not run.test_problems({'all_targets':True,'tier':'default'},['cargo','test'],results,text,expected)
expected['targets'][1]['tiers']['default']['minimum_passed']=1
assert run.test_problems({'all_targets':True,'tier':'default'},['cargo','test'],results,text,expected)
assert run.test_problems({'required_targets':['b']},['cargo','test','--test','b'],results,text)
assert not run.test_problems({'required_targets':['b']},['cargo','test','--test','a'],results,text)
assert not run.test_problems({'minimum_passed':1},['cargo','test','--no-run'],{},'')
assert run.test_problems({'minimum_passed':1},['cargo','test'],{},'')
mark('nonzero expected, missing target, cfg-empty, command selection, no-run')
assert ready.complete('test exact_case ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;', 'exact_case')
assert not ready.complete('test other ... ok\ntest result: ok. 1 passed; 0 failed; 0 ignored;', 'exact_case')
assert not ready.complete('test exact_case ... ignored\ntest result: ok. 0 passed; 0 failed; 1 ignored;', 'exact_case')
assert not ready.complete('test exact_case ... FAILED\ntest result: FAILED. 0 passed; 1 failed; 0 ignored;', 'exact_case')
mark('readiness exact named one-pass, no ignored/empty/failed cells')
artifact={'reason':'compiler-artifact','executable':'/tmp/gate-test-a','target':{'name':'a','kind':['test']},'profile':{'test':True}}
assert ready.artifacts(json.dumps(artifact))=={'a':Path('/tmp/gate-test-a')}
artifact['profile']['test']=False;assert not ready.artifacts(json.dumps(artifact))
mark('Cargo artifact binds test kind/profile/executable')
with tempfile.TemporaryDirectory() as tmp:
    tree=Path(tmp);subprocess.run(['git','init','--quiet',str(tree)],check=True)
    (tree/'a.rs').write_text('fn main() {}\n');identity={'a.rs':hashlib.sha256((tree/'a.rs').read_bytes()).hexdigest()};run.verify_source(tree,identity)
    (tree/'new.rs').write_text('new source\n')
    try:run.verify_source(tree,identity)
    except SystemExit:pass
    else:raise AssertionError('added source escaped identity guard')
    (tree/'new.rs').unlink();(tree/'a.rs').write_text('changed\n')
    try:run.verify_source(tree,identity)
    except SystemExit:pass
    else:raise AssertionError('changed source escaped identity guard')
mark('whole source file-set and content identity fail closed')
with tempfile.TemporaryDirectory() as tmp:
    path=Path(tmp)/'manifest.json';path.write_text('{"first":1}\n')
    captured,identity=run.read_manifest(path)
    assert captured=={'first':1} and run.finish_manifest(identity)
    path.write_text('{"first":2}\n')
    assert captured=={'first':1} and not run.finish_manifest(identity)
    path.unlink();assert not run.finish_manifest(identity)
mark('manifest inputs parsed once, exact byte hash change/deletion refused')
with tempfile.TemporaryDirectory() as tmp:
    root=Path(tmp);source=root/'source.json';source.write_text('{}\n')
    expected=root/'expected.json'
    demand={'targets':[{'target':'a','tiers':{'default':{'minimum_passed':1}}}]}
    expected.write_text(json.dumps(demand))
    (root/'commands.json').write_text(json.dumps({'synthetic':{'commands':[['cargo','test']],'all_targets':True,'tier':'default'}}))
    class FakeProcess:
        returncode=0
        def __init__(self, argv, **kwargs):
            kwargs['stdout'].write('Running tests/a.rs (/not/an/executable)\ntest result: ok. 0 passed; 0 failed; 0 ignored;\n')
        def wait(self, **kwargs):
            demand['targets'][0]['tiers']['default']['minimum_passed']=0
            expected.write_text(json.dumps(demand))
            return 0
    arguments=['run_stage.py','synthetic','--tree',str(root),'--target',str(root/'target'),'--out',str(root/'out'),'--source-manifest',str(source),'--expected-manifest',str(expected),'--execute']
    with mock.patch.object(run,'HERE',root),mock.patch.object(run,'verify_source'),mock.patch.object(run.subprocess,'Popen',FakeProcess),mock.patch.object(run.subprocess,'check_output',return_value='synthetic version only'),mock.patch('sys.argv',arguments),mock.patch.dict('os.environ',{'NANO_FINAL_GATE_BOUNDED':'1','CARGO_PROFILE_TEST_DEBUG':'2','CARGO_PROFILE_DEV_DEBUG':'2'}):
        try:run.main()
        except SystemExit as error:assert error.code==1
        else:raise AssertionError('changed/lowered expected manifest accepted')
    receipt=json.loads((root/'out/receipt.json').read_text())
    assert receipt['failed'] and receipt['manifest_inputs']['source']['unchanged']
    assert not receipt['manifest_inputs']['expected']['unchanged']
    assert 'empty required target: a' in receipt['commands'][0]['completeness_problems']
    assert receipt['environment']['CARGO_PROFILE_TEST_DEBUG']==receipt['environment']['CARGO_PROFILE_DEV_DEBUG']=='0'
    assert receipt['expected_manifest_sha256']==receipt['manifest_inputs']['expected']['sha256_before']
mark('mocked runner keeps pinned demands, records changed input and fixed debug0; no command launched')
assert run.command_features(['cargo','test','--features','internal-tests,vector-persist','--','--features','ignored'],{})=={'explicit':['internal-tests','vector-persist'],'source':'Cargo argv','default_features':True,'all_features':False}
assert run.command_features(['cargo','check','--no-default-features','--features=fips,encryption'],{})['default_features'] is False
assert run.command_features(['cargo','tree','--all-features'],{})['all_features'] is True
assert run.command_features(['cargo','test'],{'features':['internal-tests']})['explicit']==[]
mark('mixed-command features inferred from actual Cargo argv before test separator')
commands=json.loads((HERE/'commands.json').read_text())
assert commands['persistent-publication']['minimum_by_target']=={'persistent_mv_atomicity':7,'vector_row_overlay_recovery':10,'mv_uncertain_publication':7,'fast_side_storage_atomicity':7}
assert commands['pq-live-codec']['minimum_passed_by_command']=={'0':6,'1':1}
assert commands['readiness20']['commands'][1][1]==str(HERE/'readiness20.py')
assert '--release' in commands['codegraph-field']['commands'][0]
assert commands['public-perf-smoke']['env']=={'PERF_GATE_REPS':'3','PERF_GATE_THRESHOLD':'2.5','HELIOS_PERF_NATIVE_PROOF':'1'}
assert commands['public-perf-smoke']['public_perf_native_proof']=='nano-public-perf-native-v1'
assert len(commands['persistent-publication']['required_markers'])==10
for name in ['core-default','core-internal','core-optins','core-embed-optins']:
    assert commands[name]['minimum_by_target']=={'correlated_exists_index':4,'once_subquery_context':2}
mark('exact audited test minima, all10 overlay receipts, local readiness and fixed release/smoke flags')
with tempfile.TemporaryDirectory() as tmp:
    tree=Path(tmp)/'tree';results=tree/'benches/public/results/synthetic';results.mkdir(parents=True)
    workloads={str(index):{'ops_per_sec':100} for index in range(12)}
    (tree/'benches/public/ci_baseline.json').write_text(json.dumps({'workloads':workloads}))
    (results/'ci_measured.json').write_text(json.dumps({'workloads':workloads}))
    (results/'ci_raw.txt').write_text('synthetic only; no executable launched\n')
    target=Path(tmp)/'target';binary=target/'perf/deps/tps_workloads-synthetic';binary.parent.mkdir(parents=True)
    binary.write_text('synthetic identity only\n');binary.chmod(0o700)
    output=Path(tmp)/'output';output.mkdir()
    # A plausible table and executable glob are insufficient in V8.
    # Complete synthetic native receipt positives live in the producer package.
    try:run.public_smoke_artifacts(tree,target,output,'results dir: '+str(results)+'\n',{})
    except (ValueError,OSError):pass
    else:raise AssertionError('public table without compiler-owned native completion accepted')
mark('public smoke exact native completion required; table/executable glob alone refused')
report={'status':'SYNTHETIC SELF-CHECKS ONLY; no Cargo/product/server runs','passed':len(checks),'checks':checks}
(HERE/'selfcheck-receipt.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
