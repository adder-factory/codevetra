#!/usr/bin/env python3
"""Required proof inventory and result checking; runs only inside formal/prove.sh."""

import importlib.util
from collections import deque
import json
from pathlib import Path
import re
import subprocess
import sys

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location('prepare_formal', Path(__file__).parents[1] / 'prepare-formal.py')
binding = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(binding)


def inventory(root, manifest):
    if manifest.get('schema') != 1 or not manifest.get('kani'):
        raise ValueError('Missing or unsupported proof inventory')
    sources = set()
    total = 0
    for suite in manifest['kani']:
        source = suite['source']
        if not re.fullmatch(r'verification/kani/[a-z_]+\.rs', source) or source in sources:
            raise ValueError('Invalid or duplicate Kani source')
        sources.add(source)
        required, experimental = suite['required'], suite['experimental']
        names = required + list(experimental)
        if len(set(names)) != len(names) or any(not reason.strip() for reason in experimental.values()):
            raise ValueError('Duplicate harness or missing experimental limitation')
        masked = binding.code_mask((root / source).read_text())
        if re.search(r'\bkani\s*::\s*solver\s*\(', masked):
            raise ValueError('Per-harness solver overrides are forbidden; use the reviewed SAT allowlist')
        declared = re.findall(r'#\[kani::proof\]\s*(?:#\[[^\]]+\]\s*)*fn\s+([a-z_]\w*)', masked)
        if sorted(declared) != sorted(names) or masked.count('#[kani::proof]') != len(declared):
            raise ValueError(f'Proof inventory drift: {source}')
        total += len(required)
    if sources != {str(path.relative_to(root)) for path in (root / 'verification/kani').glob('*.rs')} or not total:
        raise ValueError('Unaccounted Kani source or empty required suite')
    return total


def run_recorded(command):
    # Stream diagnostics so the outer deadline/OOM cannot discard the log.
    # Keep only its bounded tail for checking completion summaries.
    tail = deque(maxlen=2000)
    with subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                          text=True, bufsize=1) as process:
        for line in process.stdout:
            print(line, end='', flush=True)
            tail.append(line)
        return subprocess.CompletedProcess(command, process.wait(), ''.join(tail))


def checked(command, pattern):
    completed = run_recorded(command)
    if completed.returncode or not re.search(pattern, completed.stdout):
        raise SystemExit('Verifier failed or did not confirm the complete expected proof count')


def negative_controls(root, manifest, count):
    """Both verifiers must reject an always-rejecting production-body mutation."""
    source = root / 'crates/codevetra-mcp/src/byte_budget.rs'
    original = source.read_text()
    needle = 'Some(current + additional)'
    if original.count(needle) != 1:
        raise SystemExit('Review the admission mutation control after source changes')
    try:
        source.write_text(original.replace(needle, 'None'))
        generated = root / 'negative.rs'
        generated.write_text(binding.generate(root, manifest))
        commands = [
            (['verus', '--no-cheating', str(generated)], rf'verification results:: {count - 1} verified, 1 errors\b'),
            (['kani', 'verification/kani/byte_budget.rs', '--harness',
              'admission_is_exact_and_cannot_overflow', '--exact', '--jobs', '1',
              '--solver', 'cadical', '--output-format', 'terse'], r'Complete - 0 successfully verified harnesses, 1 failures, 1 total\.'),
        ]
        for command, pattern in commands:
            completed = run_recorded(command)
            if not completed.returncode or not re.search(pattern, completed.stdout):
                print(completed.stdout, end='', flush=True)
                raise SystemExit('Negative control was not rejected by a completed verification')
            if command[0] == 'kani' and 'assertion failed' not in completed.stdout:
                print(completed.stdout, end='', flush=True)
                raise SystemExit('Kani negative control failed for a reason other than its assertion')
            print(f'Negative control: {command[0]} rejected always-rejecting admission.', flush=True)
    finally:
        source.write_text(original)


def main():
    root = Path('/work')
    manifest = json.loads((root / 'verification/contracts.json').read_text())
    total = inventory(root, manifest)
    action = sys.argv[1] if len(sys.argv) > 1 else 'verify'
    if action == 'kani':
        source, harness, solver = sys.argv[2:5]
        if solver not in ('cadical', 'kissat'):
            raise SystemExit('Kani requires a reviewed SAT solver; CBMC 6.11 SMT rounding is incorrect')
        if not any(item['source'] == source and harness in item['required'] + list(item['experimental'])
                   for item in manifest['kani']):
            raise SystemExit('Harness is absent from the reviewed inventory')
        command = ['kani', source, '--harness', harness, '--exact', '--jobs', '1',
                   '--output-format', 'regular', '--solver', solver]
        if sys.argv[5:] == ['--counterexample']:
            command += ['-Z', 'concrete-playback', '--concrete-playback', 'print']
        elif sys.argv[5:]:
            raise SystemExit('Unknown diagnostic option')
        checked(command,
                r'Complete - 1 successfully verified harnesses, 0 failures, 1 total\.')
        return
    generated = root / 'contracts.rs'
    generated.write_text(binding.generate(root, manifest))
    count = sum(bool(fn.get('ensures')) for module in manifest['modules'] for fn in module['functions'])
    if not count:
        raise SystemExit('Empty Verus suite')
    checked(['verus', '--no-cheating', str(generated)], rf'verification results:: {count} verified, 0 errors\b')
    if action == 'verus':
        return
    if action != 'verify':
        raise SystemExit('Unknown proof action')
    for suite in manifest['kani']:
        required = suite['required']
        if required:
            command = ['kani', suite['source'], '--exact', '--jobs', '1', '--solver', 'cadical', '--output-format', 'terse']
            for name in required:
                command += ['--harness', name]
            checked(command, rf'Complete - {len(required)} successfully verified harnesses, 0 failures, {len(required)} total\.')
        for name, reason in suite['experimental'].items():
            print(f'UNPROVEN experiment {name}: {reason}', flush=True)
    negative_controls(root, manifest, count)
    print(f'Required suite passed: {total} Kani harnesses, {count} Verus contracts.', flush=True)


if __name__ == '__main__':
    main()
