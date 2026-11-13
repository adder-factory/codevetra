#!/usr/bin/env python3
"""Fail-closed stable-Rust Shuttle gate, called only inside the shared sandbox."""
import os
from pathlib import Path
import re
import subprocess
import sys

CASES = (
    'cancellation_wakes_all', 'parent_and_owner_cancellation',
    'stage_watch_cancellation', 'drain_waits_for_completion',
    'registration_races_close', 'failed_worker_is_retained',
    'cancelled_drain_can_be_retried', 'abandoned_wait_can_be_retried',
)
MANIFEST = Path('/work/verification/shuttle/Cargo.toml')
BINARY = MANIFEST.parent / 'target/debug/codevetra-shuttle-checks'


def build(mutation=''):
    env = dict(os.environ, CODEVETRA_SHUTTLE_MUTATION=mutation)
    subprocess.run(['cargo', '+1.99.1', 'build', '--locked', '--offline',
                    '--manifest-path', str(MANIFEST)], env=env, check=True)


def execute(args):
    result = subprocess.run([str(BINARY), *args], capture_output=True, text=True)
    print(result.stdout, end='', flush=True)
    print(result.stderr, end='', file=sys.stderr, flush=True)
    return result


def require_completed(result, cases):
    if result.returncode != 0:
        raise SystemExit(result.returncode)
    completed = re.findall(r'^SHUTTLE_PASSED (\w+) random=1000 pct=1000 seed=\d+$',
                           result.stdout, re.MULTILINE)
    if completed != list(cases):
        raise SystemExit('Shuttle omitted, duplicated or truncated a required scenario.')


def failure_schedule(result, expected):
    output = result.stdout + result.stderr
    match = re.search(r'failing schedule:\s*"\s*([0-9a-fA-F\s]+)\s*"', output)
    schedule = ''.join(match.group(1).split()) if match else ''
    if (result.returncode <= 0 or expected not in output
            or not re.fullmatch(r'[0-9a-fA-F]{1,4096}', schedule)):
        raise SystemExit('Shuttle negative control did not produce its expected replayable failure.')
    return schedule


def main(args):
    if len(args) > 2 or (args and args[0] not in CASES):
        raise SystemExit('Select a known Shuttle case.')
    if len(args) == 2 and not re.fullmatch(r'[0-9a-fA-F]{1,4096}', args[1]):
        raise SystemExit('Invalid replay schedule.')
    subprocess.run(['cargo', '+1.99.1', 'fmt', '--manifest-path', str(MANIFEST),
                    '--check'], check=True)
    subprocess.run(['cargo', '+1.99.1', 'clippy', '--locked', '--offline',
                    '--manifest-path', str(MANIFEST), '--', '-D', 'warnings'], check=True)
    build()
    positive = execute(args)
    if len(args) == 2:
        if positive.returncode or f'SHUTTLE_REPLAY_PASSED {args[0]}' not in positive.stdout:
            raise SystemExit(positive.returncode or 1)
        return
    require_completed(positive, args or CASES)
    if args:
        return
    for mutation, case, expected in (
        ('lost-notification', 'cancellation_wakes_all', 'deadlock'),
        ('premature-drain', 'drain_waits_for_completion', 'SHUTTLE_ASSERT_DRAIN_WAITED'),
    ):
        build(mutation)
        negative = execute([case])
        schedule = failure_schedule(negative, expected)
        failure_schedule(execute([case, schedule]), expected)
        print(f'Shuttle negative control detected: {mutation}', flush=True)
    print('Shuttle suite passed: 8 scenarios, 16000 schedules, 2 negative controls.', flush=True)


if __name__ == '__main__':
    main(sys.argv[1:])
