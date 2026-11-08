#!/usr/bin/env python3
"""Read-only source inventory. Static counts are expectations, never run receipts."""
import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess

DEFAULT = {"encryption", "vector-search", "ring-crypto", "ha-tier1"}
OPTINS = {"internal-tests", "code-graph", "graph-rag", "mcp-endpoint", "vector-persist",
          "compression", "server", "ha-full", "ha-tier1", "ha-tier2", "ha-tier3", "ha-dedup",
          "ha-ab-testing", "ha-branch-replication", "simd", "lock-census", "test-utils", "sync-experimental"}
TIERS = {"default": DEFAULT, "internal": DEFAULT | {"internal-tests"},
         "optins": DEFAULT | OPTINS, "code-embed": DEFAULT | OPTINS | {"code-embed"},
         "legacy-network": DEFAULT | {"legacy-network"},
         "fips": {"fips", "encryption", "vector-search", "ha-tier1", "internal-tests"}}

def cfg(expr, features):
    tokens = re.findall(r'"[^"]*"|[A-Za-z_][\w-]*|[(),=]', expr)
    cursor = 0
    def take():
        nonlocal cursor
        name = tokens[cursor]; cursor += 1
        if cursor < len(tokens) and tokens[cursor] == '(':
            cursor += 1; args = []
            while tokens[cursor] != ')':
                args.append(take())
                if tokens[cursor] == ',': cursor += 1
            cursor += 1
            if name == 'all': return all(args)
            if name == 'any': return any(args)
            if name == 'not' and len(args) == 1: return not args[0]
            raise ValueError(name)
        if cursor < len(tokens) and tokens[cursor] == '=':
            cursor += 1; value = tokens[cursor].strip('"'); cursor += 1
            if name == 'feature': return value in features
            if name == 'target_os': return value == 'linux'
            if name == 'target_arch': return value == 'x86_64'
            if name == 'target_family': return value == 'unix'
            raise ValueError(name)
        if name in {'unix', 'test', 'debug_assertions'}: return True
        if name == 'windows': return False
        raise ValueError(name)
    answer = take()
    if cursor != len(tokens): raise ValueError(expr)
    return answer

def inventory(tree):
    rows = []
    for path in sorted((tree / 'tests').glob('*.rs')):
        source = path.read_text()
        crate_cfg = re.findall(r'#!\[cfg\((.*?)\)\]', source, re.S)
        tests = []
        for match in re.finditer(r'((?:\s*#\[[^\]]+\]\s*)+)\s*(?:async\s+)?fn\s+(\w+)', source):
            attrs = match.group(1)
            if not re.search(r'#\[(?:tokio::)?test\b', attrs): continue
            tests.append({'name': match.group(2), 'cfg': re.findall(r'#\[cfg\((.*?)\)\]', attrs, re.S),
                          'ignored': bool(re.search(r'#\[ignore\b', attrs))})
        tiers = {}
        for tier, features in TIERS.items():
            try:
                enabled = all(cfg(gate, features) for gate in crate_cfg)
                active = [test for test in tests if all(cfg(gate, features) for gate in test['cfg'])] if enabled else []
                running = [test for test in active if not test['ignored']]
                # integration_v3 expands tests/integration/{mod,...}.rs.
                minimum = 1 if enabled and (running or path.stem == 'integration_v3') else 0
                tiers[tier] = {'minimum_passed': minimum, 'declared_enabled_tests': len(active),
                               'declared_enabled_unignored_tests': len(running),
                               'zero_reason': None if minimum else ('crate cfg disabled' if not enabled else 'all tests ignored or gated')}
            except (ValueError, IndexError) as error:
                tiers[tier] = {'minimum_passed': None, 'manual_cfg_review': str(error)}
        rows.append({'target': path.stem, 'source': str(path.relative_to(tree)),
                     'sha256': hashlib.sha256(path.read_bytes()).hexdigest(), 'crate_cfg': crate_cfg,
                     'declared_test_attributes': len(re.findall(r'#\[(?:tokio::)?test(?:\([^\]]*\))?\]', source)),
                     'tests': tests, 'tiers': tiers})
    return rows

def snapshot(tree):
    paths = subprocess.check_output(['git', 'ls-files', '-z'], cwd=tree).decode().split('\0')
    paths += subprocess.check_output(['git', 'ls-files', '--others', '--exclude-standard', '-z'], cwd=tree).decode().split('\0')
    return {rel: hashlib.sha256((tree / rel).read_bytes()).hexdigest()
            for rel in sorted(set(paths)) if rel and (tree / rel).is_file()}

def main():
    parser = argparse.ArgumentParser(); parser.add_argument('tree', type=Path); parser.add_argument('out', type=Path)
    parser.add_argument('--label', required=True)
    args = parser.parse_args(); args.out.mkdir(parents=True, exist_ok=False)
    before = snapshot(args.tree)
    rows = inventory(args.tree)
    manifest = {'schema': 2, 'label': args.label, 'status': 'PREPARED ONLY; NO RUNTIME ACCEPTANCE', 'platform': 'x86_64-unknown-linux-gnu',
                'source': str(args.tree.resolve()), 'head': subprocess.check_output(['git','rev-parse','HEAD'], cwd=args.tree, text=True).strip(),
                'cargo_features': {match.group(1): re.findall(r'"([^"]+)"',match.group(2)) for match in
                    re.finditer(r'^([\w-]+)\s*=\s*(\[[\s\S]*?\])',
                        (args.tree/'Cargo.toml').read_text().split('[features]',1)[1].split('\n[',1)[0], re.M)},
                'tiers': {key: sorted(value) for key, value in TIERS.items()}, 'targets': rows,
                'counts': {'targets': len(rows), 'default_expected_nonzero': sum(row['tiers']['default']['minimum_passed'] == 1 for row in rows)}}
    (args.out/'expected-manifest.json').write_text(json.dumps(manifest, indent=2)+'\n')
    (args.out/'source-sha256.json').write_text(json.dumps(before, indent=2)+'\n')
    lines = ['# Cfg/ignored-empty targets and their execution tier', '', 'Static source expectations only. Runtime list/result logs remain required.', '',
             '| Target | Crate gate | Default | Required nonzero execution |', '|---|---|---|---|']
    for row in rows:
        if row['crate_cfg'] or row['tiers']['default']['minimum_passed'] == 0:
            destinations = [tier for tier in TIERS if row['tiers'][tier]['minimum_passed'] == 1]
            if row['target'] == 'code_graph_local_embedder': destinations = ['code-embed + explicit --include-ignored (model/runtime available)']
            if row['target'] == 'code_graph_parallel_index_bench': destinations = ['optins + explicit --include-ignored, load-gated measurement']
            if row['target'] == 'physical_ddl_window': destinations = ['explicit positive default window; internal positive windows + detector']
            if row['target'] == 'pg35_benchmark': destinations = ['separate perf-matrix PG35 ABBA (ignored)']
            if not destinations:
                destinations = ['explicit ignored diagnostic/measurement; see ignored-diagnostics.md (not default correctness credit)']
            lines.append('| '+row['target']+' | '+', '.join(row['crate_cfg']).replace('|','\\|')+' | '+str(row['tiers']['default']['minimum_passed'])+' | '+', '.join(destinations)+' |')
    (args.out/'empty-suite-tiers.md').write_text('\n'.join(lines)+'\n')
    if snapshot(args.tree) != before: raise SystemExit('source moved during inventory capture; discard this capture and retry at a safe freeze boundary')
    print(json.dumps(manifest['counts']))

if __name__ == '__main__': main()
