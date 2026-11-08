#!/usr/bin/env python3
"""Execute one prepared stage only when invoked by the fleet-bounded wrapper."""
import argparse
import hashlib
import importlib.util
import json
import math
import os
from pathlib import Path
import re
import signal
import shutil
import subprocess
import time

HERE = Path(__file__).resolve().parent

def hash_file(path):
    with path.open('rb') as stream:
        digest = hashlib.sha256()
        while block := stream.read(1024*1024): digest.update(block)
        return digest.hexdigest()

def read_manifest(path):
    path=path.resolve(); data=path.read_bytes()
    return json.loads(data), {'path':str(path), 'sha256_before':hashlib.sha256(data).hexdigest()}

def finish_manifest(identity):
    path=Path(identity['path'])
    identity['sha256_after']=hash_file(path) if path.is_file() else None
    identity['unchanged']=identity['sha256_after']==identity['sha256_before']
    return identity['unchanged']

def command_features(argv, stage):
    if not argv or argv[0]!='cargo':
        return {'explicit':stage.get('features', []), 'source':'stage harness metadata', 'default_features':None, 'all_features':None}
    flags=argv[:argv.index('--')] if '--' in argv else argv
    features=[]
    for index, flag in enumerate(flags):
        value=flags[index+1] if flag in {'--features','-F'} else flag.split('=',1)[1] if flag.startswith('--features=') else None
        if value:
            for name in value.replace(',', ' ').split():
                if name not in features: features.append(name)
    return {'explicit':features, 'source':'Cargo argv', 'default_features':'--no-default-features' not in flags, 'all_features':'--all-features' in flags}

def public_smoke_artifacts(tree, target_dir, out, text, source):
    roots=re.findall(r'^results dir: (.+)$', text, re.M)
    if len(roots)!=1: raise ValueError('public smoke must identify exactly one results directory')
    root=Path(roots[0]).resolve()
    if root.parent!=(tree/'benches/public/results').resolve(): raise ValueError('unexpected public smoke results directory')
    measured=json.loads((root/'ci_measured.json').read_text())['workloads']
    baseline=json.loads((tree/'benches/public/ci_baseline.json').read_text())['workloads']
    if len(baseline)!=12 or set(measured)!=set(baseline): raise ValueError('public smoke needs all12 exact baseline labels')
    helper=tree/'benches/public/native_ci_proof.py'
    if hash_file(helper)!=source.get('benches/public/native_ci_proof.py'): raise ValueError('native proof helper source identity differs')
    spec=importlib.util.spec_from_file_location('public_native_proof_consumer',helper)
    proof=importlib.util.module_from_spec(spec)
    # A consumer import must not create a new untracked __pycache__ source file.
    import sys
    previous=sys.dont_write_bytecode; sys.dont_write_bytecode=True
    try: spec.loader.exec_module(proof)
    finally: sys.dont_write_bytecode=previous
    native=proof.validate_complete(root/'native-proof',tree,target_dir,source_capture=source)
    proof.validate_results(root/'native-proof',measured,(root/'ci_raw.txt').read_bytes(),baseline)
    copied=out/'public-smoke-results'; copied.mkdir(exist_ok=False)
    artifacts=[]
    for name in ['ci_measured.json','ci_raw.txt']:
        target=copied/name; shutil.copy2(root/name,target)
        artifacts.append({'path':str(target.resolve()), 'sha256':hash_file(target)})
    shutil.copytree(root/'native-proof',copied/'native-proof')
    for path in sorted((copied/'native-proof').rglob('*')):
        if path.is_file(): artifacts.append({'path':str(path.resolve()), 'sha256':hash_file(path)})
    baseline_path=tree/'benches/public/ci_baseline.json'
    return {'source_results_directory':str(root), 'artifacts':artifacts, 'labels':sorted(measured),
            'baseline':{'path':str(baseline_path.resolve()), 'sha256':hash_file(baseline_path)},
            'native_proof':native, 'repetitions':3, 'threshold':2.5}

def verify_source(tree, expected):
    bad = [rel for rel, digest in expected.items()
           if not (tree/rel).is_file() or hash_file(tree/rel) != digest]
    paths = subprocess.check_output(['git', 'ls-files', '-z'], cwd=tree).decode().split('\0')
    paths += subprocess.check_output(['git', 'ls-files', '--others', '--exclude-standard', '-z'], cwd=tree).decode().split('\0')
    actual = {rel for rel in paths if rel and (tree/rel).is_file()}
    bad.extend(sorted(actual.symmetric_difference(expected)))
    if bad: raise SystemExit('source identity changed: '+', '.join(bad[:12]))

def parse_results(log):
    result = {}; current = None
    for line in log.splitlines():
        match = re.search(r'Running (tests/[^ ]+\.rs|unittests src/[^ ]+) \(', line)
        if match:
            current = Path(match.group(1)).stem if match.group(1).startswith('tests/') else match.group(1)
        match = re.search(r'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;', line)
        if match:
            result[current or 'unidentified'] = {'status': match.group(1), 'passed': int(match.group(2)),
                                               'failed': int(match.group(3)), 'ignored': int(match.group(4))}
    return result

def test_problems(stage, argv, results, text, expected=None):
    problems=[]
    for marker in stage.get('required_markers', []):
        if marker not in text: problems.append('required proof marker missing: '+marker)
    for item in results.values():
        if item['failed'] or item['status'] != 'ok': problems.append('failed Rust test result')
    if stage.get('all_targets'):
        if expected is None: raise ValueError('full tier needs independent expected source inventory')
        for target in expected['targets']:
            demand=target['tiers'][stage['tier']]['minimum_passed']
            actual=results.get(target['target'])
            if actual is None: problems.append('target not executed: '+target['target'])
            elif demand is None: problems.append('manual cfg review: '+target['target'])
            elif demand and actual['passed'] < demand: problems.append('empty required target: '+target['target'])
    for target, minimum in stage.get('minimum_by_target', {}).items():
        if results.get(target, {}).get('passed',0) < minimum:
            problems.append('too few required tests: '+target)
    selected={argv[i+1] for i, item in enumerate(argv[:-1]) if item == '--test'}
    for target in stage.get('required_targets', []):
        if (not selected or target in selected) and results.get(target, {}).get('passed', 0) < 1:
            problems.append('missing/empty selected target: '+target)
    if 'test' in argv and '--no-run' not in argv and stage.get('minimum_passed') and sum(item['passed'] for item in results.values()) < stage['minimum_passed']:
        problems.append('required nonzero test count absent')
    return problems

def diagnostic_problems(text, returncode=None):
    finished=[]; lint_errors=0; problems=[]
    for line in text.splitlines():
        try: item=json.loads(line)
        except json.JSONDecodeError: continue
        if item.get('reason') == 'build-finished':
            finished.append(item.get('success'))
            continue
        if item.get('reason') != 'compiler-message': continue
        message=item.get('message', {})
        if message.get('level') != 'error': continue
        code=(message.get('code') or {}).get('code', '')
        words=message.get('message', '')
        if words.startswith('aborting due to '): continue
        if code.startswith('clippy::') or re.fullmatch(r'[a-z_][a-z_0-9]*', code):
            lint_errors += 1
        else:
            problems.append('hard compiler failure in diagnostic capture: '+(code or words))
    if not finished:
        problems.append('missing Cargo JSON build-finished receipt')
    elif finished[-1] is not True and not lint_errors:
        problems.append('failed build has no complete comparable lint evidence')
    if returncode and not lint_errors:
        problems.append('nonzero diagnostic command without comparable lint findings')
    if returncode == 0 and finished and finished[-1] is not True:
        problems.append('command/build-finished success mismatch')
    return problems

def main():
    parser=argparse.ArgumentParser(); parser.add_argument('stage'); parser.add_argument('--tree', type=Path, required=True)
    parser.add_argument('--target', type=Path, required=True); parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--source-manifest', type=Path, required=True)
    parser.add_argument('--expected-manifest', type=Path, required=True)
    parser.add_argument('--execute', action='store_true'); args=parser.parse_args()
    stages=json.loads((HERE/'commands.json').read_text()); stage=stages[args.stage]
    if not args.execute:
        print(json.dumps(stage, indent=2)); return
    if os.environ.get('NANO_FINAL_GATE_BOUNDED') != '1': raise SystemExit('use run-one.sh for fleet lock and memory scope')
    args.out.mkdir(parents=True, exist_ok=False)
    args.target=args.target.resolve()
    source,source_identity=read_manifest(args.source_manifest)
    expected,expected_identity=read_manifest(args.expected_manifest)
    verify_source(args.tree, source)
    env=os.environ.copy(); env.update(stage.get('env', {})); env['CARGO_TARGET_DIR']=str(args.target.resolve())
    env['CARGO_TERM_COLOR']='never'
    env['CARGO_PROFILE_TEST_DEBUG']='0'
    env['CARGO_PROFILE_DEV_DEBUG']='0'
    env['NANO_FINAL_GATE_TREE']=str(args.tree.resolve())
    env['NANO_FINAL_GATE_OUT']=str(args.out.resolve())
    for required in stage.get('required_env', []):
        if not env.get(required): raise SystemExit('missing required environment '+required)
    if stage.get('load_below') and os.getloadavg()[0] >= stage['load_below']:
        raise SystemExit('load gate not met; no timing run started')
    tool_preconditions={}
    for tool in stage.get('required_tools', []):
        path=shutil.which(tool, path=env.get('PATH'))
        if not path: raise SystemExit('missing required runtime tool '+tool+'; fixture skips are not acceptance')
        command=[path,'version'] if tool=='openssl' else [path,'--version']
        tool_preconditions[tool]={'path':path,'sha256':hash_file(Path(path).resolve()),'version':subprocess.check_output(command,env=env,text=True).strip()}
    receipts=[]; failed=False
    for index, argv in enumerate(stage['commands']):
        before=time.time(); log=args.out/f'{index:02d}.log'
        toolchain=next((part for part in argv if part.startswith('+')), None) if argv and argv[0]=='cargo' else None
        versions={}
        if (argv and argv[0]=='cargo') or stage.get('public_perf_smoke'):
            for tool, flag in [('rustc','-vV'),('cargo','--version')]:
                command=[tool]+([toolchain] if toolchain else [])+[flag]
                versions[tool]=subprocess.check_output(command, cwd=args.tree, env=env, text=True).strip()
        with log.open('w') as stream:
            process=subprocess.Popen(argv, cwd=args.tree, env=env, stdout=stream, stderr=subprocess.STDOUT,
                                     start_new_session=True)
            timed_out=False
            try: process.wait(timeout=stage.get('timeout_seconds', 21600))
            except subprocess.TimeoutExpired:
                timed_out=True
                try: os.killpg(process.pid, signal.SIGTERM)
                except ProcessLookupError: pass
                try: process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    try: os.killpg(process.pid, signal.SIGKILL)
                    except ProcessLookupError: pass
                    process.wait()
        text=log.read_text(errors='replace'); results=parse_results(text)
        problems=[]
        if timed_out: problems.append('command timed out')
        command_stage=dict(stage)
        if str(index) in stage.get('minimum_passed_by_command', {}): command_stage['minimum_passed']=stage['minimum_passed_by_command'][str(index)]
        if str(index) in stage.get('required_markers_by_command', {}): command_stage['required_markers']=stage['required_markers_by_command'][str(index)]
        problems.extend(test_problems(command_stage, argv, results, text, expected if stage.get('all_targets') else None))
        for forbidden in stage.get('forbidden_log_markers', []):
            if forbidden in text: problems.append('forbidden skipped prerequisite: '+forbidden)
        if stage.get('capture_diagnostics_only'):
            problems.extend(diagnostic_problems(text, process.returncode))
        smoke=None
        if stage.get('public_perf_smoke'):
            try:
                if stage.get('public_perf_native_proof')!='nano-public-perf-native-v1': raise ValueError('mandatory native public proof contract absent')
                smoke=public_smoke_artifacts(args.tree,args.target,args.out,text,source)
            except (ValueError,OSError,KeyError) as error: problems.append('incomplete public smoke artifacts: '+str(error))
        if process.returncode and not stage.get('capture_diagnostics_only'): failed=True
        if problems: failed=True
        binaries=[]
        for match in re.finditer(r'Running [^\n]* \(([^)]+)\)', text):
            binary=Path(match.group(1)); binary=binary if binary.is_absolute() else args.tree/binary
            if binary.is_file(): binaries.append({'path':str(binary), 'sha256':hash_file(binary)})
        daemon=args.target/('release' if '--release' in argv else 'debug')/'heliosdb-nano'
        if daemon.is_file(): binaries.append({'path':str(daemon), 'sha256':hash_file(daemon), 'role':'Cargo daemon path'})
        receipts.append({'argv':argv, 'features':command_features(argv,stage), 'toolchain':versions, 'exit':process.returncode, 'timed_out':timed_out, 'started':before, 'elapsed_s':time.time()-before,
                         'log':str(log), 'results':results, 'completeness_problems':problems, 'binaries':binaries})
        if smoke is not None: receipts[-1]['public_perf_smoke']=smoke
        # --no-fail-fast is inside full-tier commands; keep independent commands
        # in a stage running so an early red does not conceal later results.
    verify_source(args.tree, source)
    source_unchanged=finish_manifest(source_identity)
    expected_unchanged=finish_manifest(expected_identity)
    if not source_unchanged or not expected_unchanged: failed=True
    (args.out/'receipt.json').write_text(json.dumps({'stage':args.stage, 'source_manifest':str(args.source_manifest),
        'source_manifest_sha256':source_identity['sha256_before'], 'expected_manifest_sha256':expected_identity['sha256_before'],
        'manifest_inputs':{'source':source_identity,'expected':expected_identity},
        'features':stage.get('features', []), 'tool_preconditions':tool_preconditions, 'environment':{key:env[key] for key in ['RUSTFLAGS','CARGO_BUILD_JOBS','CARGO_BUILD_TARGET','RUSTUP_TOOLCHAIN','CARGO_PROFILE_TEST_DEBUG','CARGO_PROFILE_DEV_DEBUG','ORT_LIB_PATH','ORT_PREFER_DYNAMIC_LINK','LD_LIBRARY_PATH','HF_HOME','FASTEMBED_CACHE_DIR','PERF_GATE_REPS','PERF_GATE_THRESHOLD'] if key in env},
        'commands':receipts, 'failed':failed, 'runtime_acceptance':'needs requirement-specific invariant review'}, indent=2)+'\n')
    raise SystemExit(1 if failed else 0)

if __name__=='__main__': main()
