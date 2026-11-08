#!/usr/bin/env python3.11
"""Post-v8 native test inventory/count proof. No test body or Cargo execution."""
import argparse
from collections import Counter
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import time
import tomllib

HERE = Path(__file__).resolve().parent
V5_BUNDLE_SHA = 'f80f9a18a3c2d4d008049ddb8e1261a393c8dcf2aff819c4ea89dfe4a1d6572f'
FLEET = Path('/home/gpc/HDB/sprint/coordination/build.lock')
STAGES = {'lib': 'default', 'core-default': 'default', 'core-internal': 'internal',
          'core-optins': 'optins', 'core-embed-optins': 'code-embed'}
ANSI = re.compile(r'\x1b\[[0-?]*[ -/]*[@-~]')
RUNNING = re.compile(r'^\s*Running (unittests [^\r\n]+?|tests/[^\r\n]+?\.rs) \(([^\r\n]+)\)\s*$', re.M)
SUMMARY = re.compile(r'^test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored; (\d+) measured; (\d+) filtered out;', re.M)
MARKER = re.compile(r'\b(?:POINT_[A-Z0-9_]+_CELL|ROUTINE_[A-Z0-9_]+_CELL|sequence-(?:setval-outcome|refill-funnel)|layout-error-(?:overlap-premise|retry-cell|current-cell)|layout-boundary-(?:explicit|interrupt|native)-cell|physical-wal-tail-cell|codegraph-(?:cold-premise|transaction-cell))\b[^\r\n]*')


class Refusal(ValueError):
    pass


def need(condition, message):
    if not condition:
        raise Refusal(message)


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def read_json(path, expected=None):
    path = Path(path).resolve()
    raw = path.read_bytes()
    digest = hashlib.sha256(raw).hexdigest()
    if expected is not None:
        need(re.fullmatch(r'[0-9a-f]{64}', expected) and digest == expected, 'JSON hash mismatch: ' + str(path))
    return json.loads(raw), {'path': str(path), 'sha256': digest}


def write_json(path, value):
    with Path(path).open('x') as stream:
        json.dump(value, stream, indent=2)
        stream.write('\n')


def pin_check(pins):
    for path, digest in pins.items():
        need(Path(path).is_file() and sha(path) == digest, 'input bytes changed: ' + path)


def package_identity():
    manifest, identity = read_json(HERE / 'manifest.json')
    for rel, record in manifest['files'].items():
        need(sha(HERE / rel) == record['sha256'], 'adjudicator package changed: ' + rel)
    return identity


def text_identity(path):
    path = Path(path).resolve()
    raw = path.read_bytes()
    return raw.decode(), {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest()}


def source_guard(tree, source, source_file_set=None):
    tree = Path(tree)
    need(all(not Path(rel).is_absolute() and '..' not in Path(rel).parts for rel in source), 'relative capture source keys required')
    if source_file_set is None:
        raw = subprocess.check_output(['git', 'ls-files', '-z', '--cached', '--others', '--exclude-standard'], cwd=tree, timeout=30)
        source_file_set = {os.fsdecode(p) for p in raw.split(b'\0') if p and (tree / os.fsdecode(p)).is_file()}
    need(set(source) == set(source_file_set), 'whole-source file-set differs from supplied capture')
    for rel, digest in source.items():
        need((tree / rel).is_file() and sha(tree / rel) == digest, 'whole-source bytes differ: ' + rel)
    return {'contents_verified': len(source), 'file_set_verified': True}


def parse_segments(text):
    """Cargo identifies the outer executable; child libtest output stays inside it."""
    text = ANSI.sub('', text)
    headers = list(RUNNING.finditer(text))
    need(headers, 'no Cargo Running records; cannot identify native executables')
    result = []
    for index, header in enumerate(headers):
        label, binary = header.groups()
        body = text[header.end():headers[index + 1].start() if index + 1 < len(headers) else len(text)]
        summaries = list(SUMMARY.finditer(body))
        counts = [int(n) for n in re.findall(r'^running (\d+) tests?$', body, re.M)]
        need(summaries and counts, 'native target has no complete outer Rust result: ' + label)
        # Native self-exec crash tests print nested running/summary lines. The
        # first running count and final summary belong to the outer Cargo target.
        summary = summaries[-1].groups()
        parsed = dict(zip(['passed', 'failed', 'ignored', 'measured', 'filtered_out'], map(int, summary[1:])))
        parsed['status'] = summary[0]
        target = label if label.startswith('unittests ') else Path(label).stem
        result.append({'target': target, 'label': label, 'source': label.removeprefix('unittests '),
                       'binary_raw': binary, 'running': counts[0], 'summary': parsed,
                       'nested_running_records': len(counts) - 1, 'nested_summary_records': len(summaries) - 1,
                       'observed_names': sorted(set(re.findall(r'^test\s+(\S+)\s+\.\.\.', body, re.M))),
                       'body': body})
    need(len({x['target'] for x in result}) == len(result), 'duplicate Cargo target Running record')
    need(len({x['binary_raw'] for x in result}) == len(result), 'same native executable assigned to multiple Cargo targets')
    return result


def parse_listing(text):
    """Rust1.95 terse discovery emits only name: test|benchmark (no footer)."""
    rows = {}
    for line in text.splitlines():
        if not line:
            continue
        match = re.fullmatch(r'(.+): (test|benchmark)', line)
        need(match is not None, 'malformed native --list line: ' + repr(line))
        name, kind = match.groups()
        need(name not in rows and name.strip() == name, 'duplicate/invalid native test name')
        rows[name] = kind
    return rows


def exact_count(segment, all_names, ignored_names, skip):
    need(set(ignored_names) <= set(all_names), 'ignored list is not a subset of complete native list')
    need(all(all_names[name] == kind for name, kind in ignored_names.items()), 'ignored native kind mismatch')
    skipped = {name for name in all_names if skip and 'lock_management' in name}
    selected = set(all_names) - skipped
    ignored = set(ignored_names) - skipped
    actual = segment['summary']
    expected = {'selected': len(selected), 'runnable': len(selected - ignored), 'ignored': len(ignored), 'filtered_out': len(skipped)}
    problems = []
    if segment['running'] != expected['selected']:
        problems.append('outer running count differs from native selection')
    if actual['passed'] + actual['failed'] != expected['runnable']:
        problems.append('passed+failed differs from all selected nonignored native entries')
    if actual['ignored'] != expected['ignored'] or actual['filtered_out'] != expected['filtered_out'] or actual['measured'] != 0:
        problems.append('ignored/filtered/measured differs from exact allowed selection')
    if not selected <= set(segment['observed_names']):
        problems.append('native selected parent names absent from raw run log')
    return {'native_total': len(all_names), 'native_ignored_total': len(ignored_names), 'native_benchmark_entries': sum(k == 'benchmark' for k in all_names.values()),
            'skipped_names': sorted(skipped), 'expected': expected, 'actual': actual,
            'count_complete': not problems, 'problems': problems,
            'correctness_green': not problems and actual['failed'] == 0 and actual['status'] == 'ok'}


def cargo_units(tree, source, features):
    manifest = tomllib.loads((Path(tree) / 'Cargo.toml').read_text())
    units = []
    library = manifest.get('lib', {})
    path = library.get('path', 'src/lib.rs')
    if path in source and library.get('test', True):
        need(library.get('harness', True), 'custom non-libtest library harness cannot be listed safely')
        units.append('unittests ' + path)
    bins = manifest.get('bin', [])
    if not bins and 'src/main.rs' in source and manifest['package'].get('autobins', True):
        bins = [{'path': 'src/main.rs'}]
    for binary in bins:
        path = binary.get('path', 'src/main.rs')
        if binary.get('test', True) and set(binary.get('required-features', [])) <= set(features):
            need(binary.get('harness', True), 'custom non-libtest binary harness cannot be listed safely')
            need(path in source, 'declared binary unit source missing')
            units.append('unittests ' + path)
    for test in manifest.get('test', []):
        need(test.get('harness', True), 'custom test harness cannot safely promise --list-only')
    # This aid is scoped to Nano's actual ordinary libtest manifest, not a
    # general Cargo target discovery replacement.
    need(not manifest.get('test') and manifest['package'].get('autotests', True), 'explicit test targets/autotests override require independent adaptation')
    need(not any(p.startswith('src/bin/') and p.endswith('.rs') for p in source), 'additional auto binary targets require independent adaptation')
    return units


def context(request, source_files=None):
    tree, target = Path(request['tree']).resolve(), Path(request['target_root']).resolve()
    receipt, receipt_id = read_json(request['receipt'], request['receipt_sha'])
    source, source_id = read_json(request['source_manifest'], request['source_sha'])
    expected, expected_id = read_json(request['expected_manifest'], request['expected_sha'])
    bundle = Path(request['bundle']).resolve()
    bundle_files, bundle_id = read_json(bundle / 'bundle-sha256.json', V5_BUNDLE_SHA)
    bundle_pins = {str(bundle / rel): digest for rel, digest in bundle_files.items()}
    pin_check(bundle_pins)
    commands, commands_id = read_json(bundle / 'commands.json', bundle_files['commands.json'])
    stage_name = receipt['stage']
    need(stage_name in STAGES, 'only existing v5 full/library tiers are supported')
    stage = commands[stage_name]
    need(receipt['source_manifest_sha256'] == source_id['sha256'] and receipt['expected_manifest_sha256'] == expected_id['sha256'], 'v5 supplied manifest SHA mismatch')
    need(Path(expected['source']).resolve() == tree, 'expected capture belongs to a different source tree')
    for key, identity in [('source', source_id), ('expected', expected_id)]:
        recorded = receipt['manifest_inputs'][key]
        need(recorded['sha256_before'] == recorded['sha256_after'] == identity['sha256'] and recorded['unchanged'] is True, 'v5 manifest guard was not successful')
    need(len(receipt['commands']) == len(stage['commands']) == 1, 'full/library tier must have one unfiltered Cargo invocation')
    command = receipt['commands'][0]
    need(command['argv'] == stage['commands'][0], 'v5 executed argv differs from frozen stage recipe')
    need(receipt['features'] == stage.get('features', []) and command['features']['explicit'] == stage.get('features', []), 'v5 feature metadata differs from exact command')
    argv = command['argv']
    need('--tests' in argv or '--lib' in argv, 'only unfiltered full/library commands supported')
    suffix = argv[argv.index('--') + 1:] if '--' in argv else []
    need(suffix in [['--nocapture'], ['--skip', 'lock_management', '--nocapture']], 'only documented lock_management skip is allowed')
    skip = '--skip' in suffix
    tiers = {t['target']: t for t in expected['targets']}
    need(len(tiers) == len(expected['targets']), 'duplicate expected integration target')
    for item in tiers.values():
        need(item['source'] == 'tests/' + item['target'] + '.rs', 'expected auto target/source mapping mismatch')
        need(source.get(item['source']) == item['sha256'], 'source/expected target capture disagreement')
    source_targets = {Path(rel).stem for rel in source if Path(rel).parent == Path('tests') and rel.endswith('.rs')}
    need(set(tiers) == source_targets, 'expected capture omits/adds ordinary auto-discovered integration targets')
    features = expected['tiers'][STAGES[stage_name]]
    units = cargo_units(tree, source, features)
    if '--lib' in argv:
        units = [name for name in units if name == 'unittests ' + tomllib.loads((tree / 'Cargo.toml').read_text()).get('lib', {}).get('path', 'src/lib.rs')]
    wanted = set(units) | (set(tiers) if '--tests' in argv else set())
    need(wanted, 'empty expected full tier')
    log = Path(command['log'])
    need(log.is_absolute() and log.is_file(), 'absolute existing v5 raw log required')
    text, log_id = text_identity(log)
    segments = parse_segments(text)
    need({s['target'] for s in segments} == wanted, 'missing/extra full-tier Cargo target names')
    normalized = {s['target']: {k: s['summary'][k] for k in ['status', 'passed', 'failed', 'ignored']} for s in segments}
    need(normalized == command['results'], 'raw final target summaries disagree with v5 results')
    binaries = {}; artifact_pins = {}
    for item in command['binaries']:
        p = Path(item['path'])
        p = p if p.is_absolute() else tree / p
        p = p.resolve()
        need(target in p.parents and p.is_file() and sha(p) == item['sha256'], 'v5 binary artifact missing/changed/outside supplied target root')
        need(str(p) not in artifact_pins, 'duplicate v5 binary artifact')
        artifact_pins[str(p)] = item['sha256']
        if item.get('role'):
            need(item['role'] == 'Cargo daemon path', 'unrecognized v5 binary role')
            continue  # verify/pin daemon bytes, but never invoke it for discovery
        need(str(p) not in binaries, 'duplicate v5 native executable hash record')
        binaries[str(p)] = item['sha256']
    records = []
    for index, segment in enumerate(segments):
        binary = Path(segment.pop('binary_raw'))
        binary = (binary if binary.is_absolute() else tree / binary).resolve()
        need(target in binary.parents and 'deps' in binary.parts, 'native executable outside supplied target root/deps')
        need(binary.is_file() and os.access(binary, os.X_OK), 'native executable missing/not executable')
        with binary.open('rb') as stream:
            need(stream.read(4) == b'\x7fELF', 'only actual native ELF test executables may be listed')
        need(str(binary) in binaries and sha(binary) == binaries[str(binary)], 'native Running path/hash is not exactly bound by v5 receipt')
        record = dict(segment, id=f'{index:04d}', binary=str(binary), binary_sha256=binaries[str(binary)])
        records.append(record)
    need({r['binary'] for r in records} == set(binaries), 'unattributed v5 executable hash record')
    pins = {x['path']: x['sha256'] for x in [receipt_id, source_id, expected_id, commands_id, bundle_id, log_id]}
    pins.update(bundle_pins)  # log SHA is bound at this postrun intake, not by v5.
    pins.update(artifact_pins)
    proof = source_guard(tree, source, source_files)
    pin_check(pins)
    return {'request': request, 'stage': stage_name, 'tier': STAGES[stage_name], 'pins': pins,
            'source': source, 'expected': expected, 'records': records, 'skip_lock_management': skip,
            'source_guard': proof, 'receipt_failed': receipt['failed'], 'command_exit': command['exit'],
            'command_timed_out': command['timed_out'], 'v5_completeness_problems': command['completeness_problems'],
            'environment': receipt.get('environment', {}), 'log_hash_origin': 'postrun intake; original v5 did not attest a log SHA'}


def fleet_guard():
    import fcntl
    need(os.environ.get('NANO_FULL_LIST_BOUNDED') == '24G-noswap', 'use root bounded collect wrapper')
    relative = next(s.split(':', 2)[2] for s in Path('/proc/self/cgroup').read_text().splitlines() if s.startswith('0::'))
    group = Path('/sys/fs/cgroup') / relative.lstrip('/')
    scope = os.environ.get('NANO_FULL_LIST_SCOPE')
    need(scope and scope in [p.name for p in [group, *group.parents]], 'listing scope identity mismatch')
    need((group / 'memory.max').read_text().strip() == str(24 * 1024**3) and (group / 'memory.swap.max').read_text().strip() == '0', 'listing requires24GiB/zero swap')
    with FLEET.open() as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return {'scope': scope, 'memory_max': 24 * 1024**3, 'swap_max': 0, 'fleet_lock_held': True}
    raise Refusal('shared fleet lock is not held')


def list_native(binary, ignored, root, prefix, env):
    argv = [binary, '--list'] + (['--ignored'] if ignored else []) + ['--format', 'terse']
    stdout, stderr = root / (prefix + '.stdout'), root / (prefix + '.stderr')
    # libtest checks its secondary-test invoker environment BEFORE --list.
    # Remove private child-runner state so discovery cannot dispatch a body.
    env, removed = listing_environment(env)
    started = time.time()
    with stdout.open('xb') as out, stderr.open('xb') as err:
        process = subprocess.Popen(argv, cwd=env.pop('_listing_cwd'), env=env, stdin=subprocess.DEVNULL,
                                   stdout=out, stderr=err, start_new_session=True)
        timed_out = False
        try:
            process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            timed_out = True
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)
    return {'argv': argv, 'exit': process.returncode, 'timed_out': timed_out, 'started': started,
            'elapsed_s': time.time() - started, 'stdout': str(stdout), 'stdout_sha256': sha(stdout),
            'stderr': str(stderr), 'stderr_sha256': sha(stderr), 'private_libtest_runner_environment_cleared': True,
            'removed_private_libtest_environment_names': removed}


def listing_environment(env):
    clean = dict(env)
    removed = sorted(name for name in clean if name.startswith('__RUST_TEST_') or name == 'RUST_TEST_INVOKE')
    for name in removed:
        del clean[name]
    return clean, removed


def new_output(path, ctx):
    path = Path(path)
    need(path.is_absolute() and not path.exists(), 'new absolute output directory required')
    resolved = path.resolve()
    for protected in [HERE, Path(ctx['request']['tree']).resolve(), Path(ctx['request']['target_root']).resolve(), Path(ctx['request']['bundle']).resolve()]:
        need(resolved != protected and protected not in resolved.parents, 'output must be external to source/target/frozen packages')
    path.mkdir(mode=0o700)
    return path


def load_plan(path, digest):
    plan, identity = read_json(path, digest)
    need(plan['kind'] == 'v5_native_inventory_plan' and plan['release_acceptance_pass'] is False, 'wrong inventory plan kind')
    ctx = context(plan['request'])
    need(ctx['pins'] == plan['pins'] and [{k: v for k, v in r.items() if k != 'body'} for r in ctx['records']] == plan['records'], 'v5 intake evidence changed since plan')
    return plan, identity, ctx


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest='action', required=True)
    prepare = sub.add_parser('plan')
    for name in ['receipt', 'receipt-sha', 'tree', 'target-root', 'source-manifest', 'source-sha', 'expected-manifest', 'expected-sha', 'bundle']:
        prepare.add_argument('--' + name, required=True)
    prepare.add_argument('--out', required=True)
    collect = sub.add_parser('collect')
    judge = sub.add_parser('adjudicate')
    for part in [collect, judge]:
        part.add_argument('--plan', required=True)
        part.add_argument('--plan-sha', required=True)
        part.add_argument('--out', required=True)
    collect.add_argument('--execute-list-only', action='store_true')
    judge.add_argument('--inventory', required=True)
    judge.add_argument('--inventory-sha', required=True)
    args = parser.parse_args()
    package = package_identity()
    if args.action == 'plan':
        request = {key: value for key, value in vars(args).items() if key not in ['action', 'out']}
        ctx = context(request)
        source_guard(request['tree'], ctx['source']); pin_check(ctx['pins'])
        need(package_identity() == package, 'package changed during intake')
        root = new_output(args.out, ctx)
        write_json(root / 'plan.json', {'kind': 'v5_native_inventory_plan', 'release_acceptance_pass': False,
                   'package_manifest': package, 'request': request, 'pins': ctx['pins'], 'stage': ctx['stage'],
                   'source_guard': ctx['source_guard'], 'log_hash_origin': ctx['log_hash_origin'],
                   'records': [{k: v for k, v in r.items() if k != 'body'} for r in ctx['records']]})
        return 0
    plan, plan_id, ctx = load_plan(args.plan, args.plan_sha)
    need(plan['package_manifest'] == package, 'inventory package differs from plan')
    root = new_output(args.out, ctx)
    if args.action == 'collect':
        need(args.execute_list_only, 'explicit --execute-list-only required; no native invocation made')
        fleet = fleet_guard(); rows = []; error = None
        try:
            for record in ctx['records']:
                pair = {'id': record['id'], 'binary': record['binary'], 'binary_sha256': record['binary_sha256']}
                for kind in ['all', 'ignored']:
                    pin_check(ctx['pins']); need(sha(record['binary']) == record['binary_sha256'], 'binary changed before listing')
                    env = os.environ.copy(); env.update(ctx['environment']); env['_listing_cwd'] = ctx['request']['tree']
                    one = list_native(record['binary'], kind == 'ignored', root, record['id'] + '.' + kind, env)
                    one['binary_sha256_before'] = record['binary_sha256']; one['binary_sha256_after'] = sha(record['binary'])
                    need(one['binary_sha256_after'] == record['binary_sha256'], 'binary changed during listing')
                    pair[kind] = one
                rows.append(pair)
        except Exception as caught:
            error = str(caught)
        try:
            source_guard(ctx['request']['tree'], ctx['source']); pin_check(ctx['pins'])
            need(package_identity() == package, 'package changed during listing')
        except Exception as caught:
            error = error or str(caught)
        write_json(root / 'inventory.json', {'kind': 'v5_native_list_only_receipt', 'release_acceptance_pass': False,
                   'test_bodies_executed': False, 'plan': plan_id, 'package_manifest': package, 'fleet': fleet,
                   'fatal_error': error, 'source_manifest_sha256': ctx['request']['source_sha'],
                   'listings': rows, 'all_binaries_attempted': len(rows) == len(ctx['records'])})
        return 3 if error else 0
    inventory, inventory_id = read_json(args.inventory, args.inventory_sha)
    need(inventory['kind'] == 'v5_native_list_only_receipt' and inventory['plan'] == plan_id and inventory['package_manifest'] == package, 'inventory linkage mismatch')
    need(inventory['release_acceptance_pass'] is False and inventory['test_bodies_executed'] is False and inventory['fatal_error'] is None and inventory['all_binaries_attempted'] is True, 'native listing collection incomplete')
    need(inventory['source_manifest_sha256'] == ctx['request']['source_sha'] and inventory['fleet']['memory_max'] == 24 * 1024**3 and inventory['fleet']['swap_max'] == 0 and inventory['fleet']['fleet_lock_held'] is True, 'inventory source/fleet proof mismatch')
    listings = {x['id']: x for x in inventory['listings']}
    need(len(listings) == len(inventory['listings']) == len(ctx['records']), 'missing/duplicate native inventories')
    results = []
    for record in ctx['records']:
        pair = listings[record['id']]
        need(pair['binary'] == record['binary'] and pair['binary_sha256'] == record['binary_sha256'], 'inventory binary mismatch')
        parsed = {}
        for kind in ['all', 'ignored']:
            one = pair[kind]
            wanted = [record['binary'], '--list'] + (['--ignored'] if kind == 'ignored' else []) + ['--format', 'terse']
            need(one['argv'] == wanted and one['exit'] == 0 and one['timed_out'] is False, 'native list command was not successful exact --list-only')
            need(one['private_libtest_runner_environment_cleared'] is True, 'private libtest invoker state was not cleared')
            need(one['binary_sha256_before'] == one['binary_sha256_after'] == record['binary_sha256'], 'native inventory executable changed')
            listing, listing_id = text_identity(one['stdout'])
            need(listing_id['sha256'] == one['stdout_sha256'] and sha(one['stderr']) == one['stderr_sha256'], 'native listing bytes changed')
            ctx['pins'][listing_id['path']] = listing_id['sha256']
            ctx['pins'][str(Path(one['stderr']).resolve())] = one['stderr_sha256']
            parsed[kind] = parse_listing(listing)
        record['native_names'] = sorted(parsed['all'])
        record['native_ignored_names'] = sorted(parsed['ignored'])
        result = exact_count(record, parsed['all'], parsed['ignored'], ctx['skip_lock_management'])
        if not record['target'].startswith('unittests '):
            demand = next(t for t in ctx['expected']['targets'] if t['target'] == record['target'])['tiers'][ctx['tier']]['minimum_passed']
            if demand and result['expected']['runnable'] == 0:
                result['problems'].append('source capture expected enabled nonzero target; native runnable inventory empty')
                result['count_complete'] = result['correctness_green'] = False
        results.append(dict(result, target=record['target'], binary=record['binary'], binary_sha256=record['binary_sha256']))
    critical = critical_checks(ctx)
    source_guard(ctx['request']['tree'], ctx['source']); pin_check(ctx['pins'])
    need(package_identity() == package, 'package changed during adjudication')
    need(sha(args.plan) == args.plan_sha and sha(args.inventory) == args.inventory_sha, 'plan/inventory changed during adjudication')
    complete = all(r['count_complete'] for r in results) and critical['pass']
    green = complete and all(r['correctness_green'] for r in results) and not ctx['receipt_failed'] and ctx['command_exit'] == 0 and not ctx['command_timed_out'] and not ctx['v5_completeness_problems']
    write_json(root / 'adjudication.json', {'kind': 'v5_full_tier_native_count_adjudication', 'plan': plan_id,
               'inventory': inventory_id, 'native_count_adjudication_pass': complete, 'full_stage_correctness_green': green,
               'release_acceptance_pass': False, 'source_manifest_sha256': ctx['request']['source_sha'],
               'targets': results, 'critical': critical, 'log_hash_origin': ctx['log_hash_origin'],
               'note': 'Exact native counts supplement requirement-specific runtime invariants; this report alone cannot grant release acceptance.'})
    return 0 if green else 1


def critical_checks(ctx):
    """Native names and source-pinned loop/cell proofs, never regex test totals."""
    required, _ = read_json(HERE / 'critical-expectations.json')
    records = {r['target']: r for r in ctx['records']}
    findings = []; problems = []
    for probe in required['probes']:
        if ctx['tier'] not in probe.get('tiers', ['default','internal','optins','code-embed']):
            continue
        if probe['internal_only'] and ctx['tier'] == 'default':
            continue
        target = probe['target']
        if target not in records:  # standalone --lib has no integration targets
            if ctx['stage'] == 'lib' and not target.startswith('unittests '):
                continue
            problems.append('critical target missing: ' + target); continue
        record = records[target]
        need(ctx['source'].get(probe['source']) == probe['source_sha256'], 'critical fixture source changed; refresh independent expectations: ' + probe['source'])
        missing_names = set(probe['names']) - (set(record['observed_names']) & (set(record['native_names']) - set(record['native_ignored_names'])))
        markers = Counter(m.group().strip() for m in MARKER.finditer(record['body']))
        # Full lib also prints other ROUTINE marker families. Restrict to each
        # independently declared family, but reject all unknown identities in it.
        families = set(probe['families'])
        selected = {k: v for k, v in markers.items() if k.split()[0] in families}
        good = not missing_names and Counter(selected) == Counter({k: 1 for k in probe['markers']})
        findings.append({'target': target, 'source': probe['source'], 'parents': len(probe['names']),
                         'expected_cells': probe.get('case_count', sum(probe['markers'].values())), 'pass': good,
                         'missing_parent_names': sorted(missing_names), 'observed_marker_identities': selected})
        if not good: problems.append('critical exact names/cell families incomplete: ' + probe['source'])
    if 'row_shape' in records:
        r57 = required['r57']; record = records['row_shape']
        need(ctx['source'].get('tests/row_shape.rs') == r57['source_sha256'], 'R57 fixture changed; refresh independent expectations')
        good = set(r57['names']) <= (set(record['observed_names']) & (set(record['native_names']) - set(record['native_ignored_names']))) and record['summary']['failed'] == 0
        findings.append({'target': 'row_shape', 'parents': 5, 'source_backed_native_cases': 10,
                         'dimension': 'logical_wal_per_statement=false/true (not TT)', 'pass': good})
        if not good: problems.append('five unignored R57 parents did not all pass')
    return {'pass': not problems, 'findings': findings, 'problems': problems}


if __name__ == '__main__':
    try:
        raise SystemExit(main())
    except (Refusal, OSError, KeyError, TypeError, ValueError, IndexError, subprocess.SubprocessError) as error:
        print('FULL-TIER COUNT REFUSED: ' + str(error), file=__import__('sys').stderr)
        raise SystemExit(3)
