#!/usr/bin/env python3
"""Run unchanged v5 prerequisite stages in an isolated, source-bound CI service."""
import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import time

HERE = Path(__file__).resolve().parent
V5_SHA = '1c2884e87666bc46e4b4571c174467b46a374ac52f285a95398d433146fbd5a2'
STAGES = ('security', 'msrv-default', 'msrv-embed', 'minimal-ring', 'fips')
TOOLS = {'cargo-audit': '0.22.2', 'cargo-deny': '0.19.9'}
ORT_PINS = {'ort_prereq.py': 'ba6845dd2476846e7320233e7fbb1d35c3b53aa5abee0af91d638321ecf64a6a', 'ort-prereq.json': '00dde27899411b54d1a0878cd6a91231f6475b8b5ad76ce5947c30b08665e5e9'}


def sha(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def save(path, value):
    with path.open('x') as stream:
        json.dump(value, stream, indent=2, sort_keys=True)
        stream.write('\n')


def git(tree, *args):
    return subprocess.check_output(['git', '-C', str(tree), *args], timeout=30)


def guard_source(tree, model, head):
    if git(tree, 'rev-parse', 'HEAD').decode().strip() != head:
        raise RuntimeError('checkout/event HEAD mismatch')
    if git(tree, 'diff', '--name-only', 'HEAD') or git(tree, 'ls-files', '--others', '--exclude-standard'):
        raise RuntimeError('source checkout is not clean')
    entries = {}
    for row in git(tree, 'ls-tree', '-rz', '--full-tree', 'HEAD').split(b'\0'):
        if row:
            meta, name = row.split(b'\t', 1)
            mode, kind, oid = meta.decode().split()
            if kind != 'blob':
                raise RuntimeError('unsupported source entry')
            entries[name.decode()] = {'mode': mode, 'oid': oid}
    expected = model['entries']
    if set(entries) != set(expected) | set(model['auxiliary_paths']):
        raise RuntimeError('committed source path set differs from canonical product plus declared auxiliary files')
    for name, record in expected.items():
        path = tree / name
        if entries[name] != {key: record[key] for key in ('mode', 'oid')}:
            raise RuntimeError('canonical Git input changed: ' + name)
        if record['mode'] == '120000':
            if not path.is_symlink() or os.readlink(path) != record['target_text']:
                raise RuntimeError('canonical symlink text changed: ' + name)
        elif path.is_symlink() or not path.is_file() or sha(path) != record['sha256']:
            raise RuntimeError('canonical source bytes changed: ' + name)
    for name in model['auxiliary_paths']:
        path = tree / name
        if entries[name]['mode'] != '100644' or path.is_symlink() or not path.is_file() or path.read_bytes() != git(tree, 'show', 'HEAD:' + name):
            raise RuntimeError('auxiliary committed input changed: ' + name)
    return {'head': head, 'product_commit': model['product_commit'], 'canonical_model_sha256': model['canonical_model_sha256'], 'product_paths': len(expected), 'all_paths': len(entries)}


def verify_v5():
    manifest = HERE / 'v5/bundle-sha256.json'
    if sha(manifest) != V5_SHA:
        raise RuntimeError('frozen v5 manifest pin mismatch')
    records = json.loads(manifest.read_text())
    for name, digest in records.items():
        if sha(manifest.parent / name) != digest:
            raise RuntimeError('frozen v5 member changed: ' + name)
    for name, digest in ORT_PINS.items():
        if sha(HERE / name) != digest:
            raise RuntimeError('shared ORT component pin mismatch: ' + name)


def config_identity(tree, cargo_home):
    # Cargo discovers cwd ancestors independently of CARGO_HOME. Record absence
    # too, without exposing config bytes or credential-bearing environment values.
    roots = [tree, *tree.parents]
    paths = {root / '.cargo' / name for root in roots for name in ('config', 'config.toml')}
    paths.update(cargo_home / name for name in ('config', 'config.toml'))
    result = {}
    for path in sorted(paths):
        present = path.exists() or path.is_symlink()
        if present and not path.is_file():
            raise RuntimeError('Cargo config is not a readable file: ' + str(path))
        result[str(path)] = {'present': present, 'symlink_text': os.readlink(path) if path.is_symlink() else None, 'sha256': sha(path) if present else None}
    return result


def bounded_command(argv, tree, env, out, label, seconds):
    log = out / (label + '.log')
    started = time.time()
    with log.open('x') as stream:
        child = subprocess.Popen(argv, cwd=tree, env=env, stdout=stream, stderr=subprocess.STDOUT, start_new_session=True)
        timed_out = False
        try:
            child.wait(timeout=seconds)
        except subprocess.TimeoutExpired:
            timed_out = True
            for sig, timeout in ((signal.SIGTERM, 15), (signal.SIGKILL, 10)):
                try:
                    os.killpg(child.pid, sig)
                except ProcessLookupError:
                    pass
                try:
                    child.wait(timeout=timeout)
                    break
                except subprocess.TimeoutExpired:
                    continue
            if child.poll() is None:
                raise RuntimeError('owned child did not terminate after bounded cleanup')
    record = {'argv': argv, 'exit': child.returncode, 'timed_out': timed_out, 'started': started, 'elapsed_s': time.time() - started, 'log': str(log), 'log_sha256': sha(log)}
    save(out / (label + '.json'), record)
    return record


def compiler_identity(tree, env, toolchain):
    result = {}
    for tool, flag in (('rustc', '-vV'), ('cargo', '--version')):
        actual = Path(subprocess.check_output(['rustup', 'which', '--toolchain', toolchain, tool], cwd=tree, env=env, text=True, timeout=30).strip())
        version = subprocess.check_output([str(actual), flag], cwd=tree, env=env, text=True, timeout=30).strip()
        if tool == 'rustc' and ('release: ' + toolchain) not in version.splitlines():
            raise RuntimeError('actual rustc version differs from required toolchain')
        if tool == 'cargo' and not version.startswith('cargo ' + toolchain + ' '):
            raise RuntimeError('actual Cargo version differs from required toolchain')
        result[tool] = {'path': str(actual), 'sha256': sha(actual), 'version': version}
    return result


def advisory_identity(cargo_home):
    root = cargo_home / 'advisory-db'
    candidates = [root] + (sorted(root.iterdir()) if root.is_dir() else [])
    result = []
    for path in candidates:
        if path.is_dir() and (path / '.git').exists():
            result.append({'path': str(path), 'head': git(path, 'rev-parse', 'HEAD').decode().strip(), 'tracked_dirty': bool(git(path, 'diff', '--name-only', 'HEAD')), 'tree': git(path, 'rev-parse', 'HEAD^{tree}').decode().strip()})
    return result


def security_graph_problems(stage_out):
    receipt = json.loads((stage_out / 'receipt.json').read_text())
    problems = []
    commands = receipt['commands']
    web = Path(commands[3]['log']).read_text(errors='replace')
    versions = re.findall(r'^webpki-roots v(\d+)\.(\d+)\.(\d+)(?:\s|$)', web, re.M)
    if not versions or any(tuple(map(int, version)) < (1, 0, 10) for version in versions):
        problems.append('webpki-roots inverse tree lacks supported >=1.0.10 root')
    rkyv = Path(commands[4]['log']).read_text(errors='replace')
    if re.search(r'^rkyv v', rkyv, re.M) or 'nothing to print' not in rkyv:
        problems.append('ignored rkyv advisory reachability was not proved empty')
    return problems


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--tree', type=Path, required=True)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--stage', choices=STAGES, required=True)
    parser.add_argument('--head', required=True)
    parser.add_argument('--model-sha256', required=True)
    args = parser.parse_args()
    tree, out = args.tree.resolve(), args.out.resolve()
    model_path = HERE / 'model.json'
    if sha(model_path) != args.model_sha256:
        raise RuntimeError('canonical source model pin mismatch')
    model = json.loads(model_path.read_text())
    verify_v5()
    if out == tree or tree in out.parents:
        raise RuntimeError('output must be outside the source checkout')
    group = next(row[3:] for row in Path('/proc/self/cgroup').read_text().splitlines() if row.startswith('0::'))
    cgroup = Path('/sys/fs/cgroup') / group.lstrip('/')
    limits = {name: (cgroup / name).read_text().strip() for name in ('memory.max', 'memory.swap.max')}
    if limits != {'memory.max': str(24 * 1024**3), 'memory.swap.max': '0'}:
        raise RuntimeError('24 GiB/no-swap service required')
    fleet = Path('/home/gpc/HDB/sprint/coordination/build.lock').open('r+')
    fcntl.flock(fleet, fcntl.LOCK_EX | fcntl.LOCK_NB)
    out.mkdir(parents=True, exist_ok=False)
    env = os.environ.copy()
    # Avoid inherited libtest child dispatch and compiler wrappers. The workflow
    # passes only its explicit nonsecret build environment into the service.
    for key in list(env):
        if key.startswith('__RUST_TEST_') or key in ('RUST_TEST_INVOKE', 'RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER', 'CARGO_ENCODED_RUSTFLAGS'):
            env.pop(key)
    env.update(NANO_FINAL_GATE_BOUNDED='1', CARGO_BUILD_JOBS='2', CARGO_TERM_COLOR='never', CARGO_PROFILE_TEST_DEBUG='0', CARGO_PROFILE_DEV_DEBUG='0')
    target = out / 'fresh-target'
    if target.exists():
        raise RuntimeError('target root must be fresh')
    env['CARGO_TARGET_DIR'] = str(target)
    cargo_home = Path(env['CARGO_HOME']).resolve()
    before = guard_source(tree, model, args.head)
    configs_before = config_identity(tree, cargo_home)
    save(out / 'source-before.json', before)
    save(out / 'cargo-config-before.json', configs_before)
    save(out / 'host.json', {'cgroup': group, 'limits': limits, 'uname': list(os.uname()), 'cpu_count': os.cpu_count(), 'meminfo': Path('/proc/meminfo').read_text(), 'namespace': 'exclusive fleet namespace of this hosted CI VM'})
    compilers = {'1.95.0': compiler_identity(tree, env, '1.95.0')}
    if args.stage in ('msrv-default', 'msrv-embed'):
        version = '1.88.0' if args.stage == 'msrv-default' else '1.92.0'
        compilers[version] = compiler_identity(tree, env, version)
    save(out / 'compilers.json', compilers)
    native_tools = {}
    wanted = ['cc', 'c++', 'clang', 'pkg-config']
    if args.stage == 'fips':
        wanted += ['cmake', 'ninja', 'nasm', 'perl', 'go']
    for tool in wanted:
        found = shutil.which(tool, path=env['PATH'])
        if not found:
            raise RuntimeError('required native build tool missing: ' + tool)
        path = Path(found).resolve()
        argv = [str(path), 'version'] if tool == 'go' else [str(path), '--version']
        version = subprocess.check_output(argv, cwd=tree, env=env, text=True, timeout=30).strip()
        native_tools[tool] = {'path': str(path), 'sha256': sha(path), 'version': version}
    save(out / 'native-build-tools.json', native_tools)
    capture = out / 'capture'
    subprocess.run([sys.executable, str(HERE / 'v5/inventory.py'), str(tree), str(capture), '--label', 'cloud-prerequisite-' + args.stage + '-' + args.head], check=True, env=env)
    setup = []
    tool_identities = {}
    advisory_before = advisory_identity(cargo_home) if args.stage == 'security' else []
    exit_code = 1
    problems = []
    ort_runtime = None
    ort_spec = None
    try:
        if args.stage == 'security':
            tool_root = out / 'security-tools'
            for tool, version in TOOLS.items():
                row = bounded_command(['cargo', '+1.95.0', 'install', '--locked', '--version', version, '--root', str(tool_root), tool], tree, env, out, 'install-' + tool, 1800)
                setup.append(row)
                if row['exit'] or row['timed_out']:
                    raise RuntimeError('security tool bootstrap failed: ' + tool)
            env['PATH'] = str(tool_root / 'bin') + os.pathsep + env['PATH']
            for tool, version in TOOLS.items():
                path = tool_root / 'bin' / tool
                actual = subprocess.check_output([str(path), '--version'], cwd=tree, env=env, text=True, timeout=30).strip()
                if not re.match(re.escape(tool) + r'\s+' + re.escape(version) + r'(?:\s|$)', actual):
                    raise RuntimeError('security tool version mismatch: ' + tool)
                tool_identities[tool] = {'version': actual, 'path': str(path), 'sha256': sha(path)}
            save(out / 'security-tools.json', tool_identities)
        if args.stage == 'msrv-embed':
            from ort_prereq import prepare
            ort_spec = json.loads((HERE / 'ort-prereq.json').read_text())
            ort_runtime, ort_environment = prepare(out / 'ort-prerequisite', ort_spec)
            env.update(ort_environment)
        command = [sys.executable, str(HERE / 'v5/run_stage.py'), args.stage, '--tree', str(tree), '--target', str(target), '--out', str(out / 'stage'), '--source-manifest', str(capture / 'source-sha256.json'), '--expected-manifest', str(capture / 'expected-manifest.json'), '--execute']
        # The unchanged runner bounds each exact v5 command. The outer service
        # and workflow independently bound the complete job including setup.
        result = subprocess.run(command, env=env)
        exit_code = result.returncode
        if args.stage == 'security' and (out / 'stage/receipt.json').is_file():
            problems.extend(security_graph_problems(out / 'stage'))
    finally:
        verify_v5()
        if ort_runtime is not None:
            from ort_prereq import verify_runtime
            verify_runtime(ort_runtime, ort_spec)
        after = guard_source(tree, model, args.head)
        configs_after = config_identity(tree, cargo_home)
        save(out / 'source-after.json', after)
        save(out / 'cargo-config-after.json', configs_after)
        if before != after or configs_before != configs_after:
            problems.append('source or Cargo configuration identity moved during gate')
        advisory_after = advisory_identity(cargo_home) if args.stage == 'security' else []
        if args.stage == 'security' and (not advisory_after or any(row['tracked_dirty'] for row in advisory_after)):
            problems.append('current advisory repository identity absent or dirty')
        save(out / 'adapter-result.json', {'schema': 1, 'stage': args.stage, 'head': args.head, 'source_model_sha256': args.model_sha256, 'unchanged_v5_manifest_sha256': V5_SHA, 'exit': exit_code, 'problems': problems, 'fresh_target_before_setup': True, 'setup': setup, 'advisory_before': advisory_before, 'advisory_after': advisory_after, 'advisory_identity_boundary': 'before/after whole unchanged security stage; command logs retain each normal fetch, not per-command database snapshots', 'source_guards_pass': before == after and configs_before == configs_after, 'release_acceptance_pass': False, 'scope': 'Prerequisite command execution only. MSRV code-embed is compile-only. Full FIPS uses unchanged v5 tests; generic v5 minimum counts are not a complete native inventory verdict.'})
    return exit_code or (1 if problems else 0)


if __name__ == '__main__':
    sys.exit(main())
