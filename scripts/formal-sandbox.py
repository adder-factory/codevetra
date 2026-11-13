#!/usr/bin/env python3
"""Run formal verifiers only inside a bounded, single-admission Linux container."""

import argparse
import contextlib
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import resource
import signal
import subprocess
import sys
import tempfile
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
BASE = 'ubuntu@sha256:2198e4c65076e9cb0b3d695219df667c0e3b0072da64ca3c2648aca17dcecaf5'
LABEL = 'org.codevetra.formal.run'
MEMORY = 4 * 1024**3
NAME = f'codevetra-formal-{os.getuid()}'
CACHE = Path.home() / '.cache' / 'codevetra-formal'


class Refusal(RuntimeError):
    """No verifier may be started when containment cannot be established."""


def execute(argv, timeout=30, check=True, **kwargs):
    result = subprocess.run(argv, timeout=timeout, check=False, **kwargs)
    if check and result.returncode:
        detail = (result.stderr or '').strip()[-2000:]
        raise Refusal(f'Command failed ({result.returncode}): {detail}')
    return result


@contextlib.contextmanager
def admission(cache=CACHE):
    cache.mkdir(parents=True, exist_ok=True, mode=0o700)
    # Never unlink a flock file: replacing its inode would admit two runners.
    with (cache / 'admission.lock').open('a+') as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise Refusal('Another formal run owns this user-wide slot; do not start a retry.') from error
        yield


class Docker:
    def __init__(self):
        self.env = dict(os.environ)
        context = [os.environ['DOCKER_CONTEXT']] if os.environ.get('DOCKER_CONTEXT') else []
        host = os.environ.get('DOCKER_HOST') if not context else None
        if not host:
            host = execute(['docker', 'context', 'inspect', *context, '--format',
                            '{{.Endpoints.docker.Host}}'], capture_output=True, text=True).stdout.strip()
        if not host.startswith('unix:///'):
            raise Refusal('Formal verification requires an existing local Docker Unix socket.')
        for key in ('DOCKER_CONTEXT', 'DOCKER_HOST', 'DOCKER_TLS_VERIFY', 'DOCKER_CERT_PATH'):
            self.env.pop(key, None)
        self.prefix = ['docker', '--host', host]
        info = json.loads(self.call(['info', '--format', '{{json .}}']).stdout)
        if (info.get('OSType') != 'linux' or info.get('CgroupVersion') != '2'
                or not all(info.get(key) for key in ('MemoryLimit', 'SwapLimit', 'PidsLimit'))
                or info.get('MemTotal', 0) < MEMORY * 2):
            raise Refusal('Need Linux cgroup v2, memory/swap/pid enforcement and at least 8 GiB VM RAM.')

    def call(self, args, **kwargs):
        return execute([*self.prefix, *args], env=self.env, capture_output=True, text=True, **kwargs)

    def inspect(self, identity):
        result = self.call(['container', 'inspect', identity], check=False)
        if result.returncode:
            if 'No such object' in result.stderr or 'No such container' in result.stderr:
                return None
            raise Refusal('Cannot establish container ownership: ' + result.stderr.strip())
        return json.loads(result.stdout)[0]


def image_name(root=ROOT):
    recipe = BASE.encode() + (root / 'scripts/formal/setup.sh').read_bytes()
    for name in ('Cargo.toml', 'Cargo.lock'):
        recipe += name.encode() + b'\0' + (root / 'verification/shuttle' / name).read_bytes()
    return 'codevetra-formal:' + hashlib.sha256(recipe).hexdigest()[:20]


def bound_log():
    # This bounds only the Docker client's output file, not solver memory.
    resource.setrlimit(resource.RLIMIT_FSIZE, (16 * 1024**2, 16 * 1024**2))


def container_args(image, source, command, deadline, memory, setup, name, token):
    if not 64 * 1024**2 <= memory <= MEMORY or not 1 <= deadline <= (900 if setup else 300):
        raise Refusal('Requested resources exceed the reviewed safety bounds.')
    args = ['create', '--name', name, '--label', f'{LABEL}={token}', '--init',
            '--memory', str(memory), '--memory-swap', str(memory), '--cpus', '2',
            '--pids-limit', '128', '--ulimit', 'core=0',
            '--security-opt', 'no-new-privileges', '--restart', 'no',
            '--log-driver', 'local', '--log-opt', 'max-size=8m', '--log-opt', 'max-file=2',
            '--mount', f'type=bind,src={source},dst=/source,readonly',
            '--network', 'bridge' if setup else 'none']
    if not setup:
        args += ['--read-only', '--cap-drop', 'ALL',
                 '--tmpfs', '/work:rw,exec,size=1073741824',
                 '--tmpfs', '/tmp:rw,exec,size=268435456']
    return [*args, '--entrypoint', '/bin/bash', image, '/source/scripts/formal/entry.sh',
            str(deadline), *command]


def run_container(docker, image, source, command, report_dir, *, deadline=300,
                  memory=MEMORY, setup=False, name=NAME, publish_image=None):
    """The fixed daemon-side name also fences orphaned runs after host SIGKILL."""
    if docker.inspect(name) is not None:
        raise Refusal(f'{name} still exists. Inspect it before retrying; it will not be deleted automatically.')
    token = uuid.uuid4().hex
    cid = None
    client = None
    report = {'command': command, 'memory_bytes': memory, 'swap_bytes': 0,
              'deadline_seconds': deadline, 'outcome': 'incomplete', 'cleanup': 'not_started'}
    report_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    result_path = report_dir / 'result.json'
    result_path.write_text(json.dumps(report, indent=2) + '\n')
    try:
        args = container_args(image, source, command, deadline, memory, setup, name, token)
        cid = docker.call(args, timeout=180).stdout.strip()
        report['container_id'] = cid
        result_path.write_text(json.dumps(report, indent=2) + '\n')
        print(f'Formal run: {report_dir} (limit {memory // 1024**2} MiB, {deadline}s)', flush=True)
        with (report_dir / 'output.log').open('w') as log:
            client = subprocess.Popen([*docker.prefix, 'start', '--attach', cid],
                                      env=docker.env, stdout=log, stderr=subprocess.STDOUT,
                                      preexec_fn=bound_log)
            try:
                client.wait(timeout=deadline + 15)
            except subprocess.TimeoutExpired:
                report['outcome'] = 'timeout'
                raise Refusal('Host deadline expired; terminating the entire proof container.')
        inspected = docker.inspect(cid)
        if inspected is None or inspected['State']['Running']:
            raise Refusal('Cannot confirm verifier completion.')
        state = inspected['State']
        if client.returncode != state['ExitCode']:
            raise Refusal('Docker attach status disagrees with the container; evidence is incomplete.')
        report.update(exit_code=state['ExitCode'], oom_killed=state['OOMKilled'])
        if state['OOMKilled']:
            report['outcome'] = 'memory_limit'
        elif state['ExitCode'] in (124, 137):
            report['outcome'] = 'timeout_or_killed'
        elif state['ExitCode']:
            report['outcome'] = 'failed'
        else:
            report['outcome'] = 'passed'
        if setup and report['outcome'] == 'passed':
            if publish_image is None:
                raise Refusal('Missing frozen setup image identity.')
            docker.call(['commit', cid, publish_image], timeout=180)
        return report
    except BaseException:
        if report['outcome'] == 'passed':
            report['outcome'] = 'incomplete'
        raise
    finally:
        previous_handlers = {sig: signal.signal(sig, signal.SIG_IGN)
                             for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
        # A create RPC may succeed even if its response is lost. Recover only
        # our unique ownership token, never a foreign or earlier container.
        try:
            owned = docker.inspect(cid or name)
            if owned is not None and owned['Config'].get('Labels', {}).get(LABEL) == token:
                docker.call(['rm', '--force', owned['Id']])
                if docker.inspect(owned['Id']) is not None:
                    raise Refusal('Container cleanup unconfirmed; admission remains fenced by its name.')
                report['cleanup'] = 'confirmed'
            elif cid is not None:
                raise Refusal('Owned container disappeared or changed ownership; completion is unconfirmed.')
        except BaseException:
            report.update(outcome='incomplete', cleanup='unconfirmed')
            raise
        finally:
            if client is not None and client.poll() is None:
                client.terminate()
                try:
                    client.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    client.kill()
                    client.wait(timeout=5)
            result_path.write_text(json.dumps(report, indent=2) + '\n')
            for sig, handler in previous_handlers.items():
                signal.signal(sig, handler)


def snapshot(destination):
    paths = [*ROOT.glob('crates/*/src/**/*.rs'), *ROOT.glob('verification/kani/*.rs'),
             *ROOT.glob('verification/shuttle/src/**/*.rs'),
             ROOT / 'verification/shuttle/build.rs',
             *ROOT.glob('scripts/formal/*.sh'), *ROOT.glob('scripts/formal/*.py'),
             ROOT / 'scripts/prepare-formal.py',
             ROOT / 'verification/contracts.json',
             ROOT / 'verification/shuttle/Cargo.toml',
             ROOT / 'verification/shuttle/Cargo.lock']
    digest = hashlib.sha256()
    for path in sorted(paths):
        relative = path.relative_to(ROOT)
        if path.is_symlink() or not path.resolve().is_relative_to(ROOT):
            raise Refusal('Proof input escaped the repository.')
        content = path.read_bytes()
        digest.update(str(relative).encode() + b'\0' + content)
        target = destination / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_bytes(content)
    return digest.hexdigest()


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--timeout-seconds', type=int, default=300, choices=range(1, 301), metavar='1..300')
    parser.add_argument('--memory-mib', type=int, default=4096, choices=range(128, 4097),
                        metavar='128..4096', help='Reduce the enforced aggregate budget for small proofs; never exceeds 4 GiB.')
    commands = parser.add_subparsers(dest='action')
    commands.add_parser('setup', help='Install pinned tools inside the same bounded container (15-minute limit).')
    commands.add_parser('verify', help='Run every required manifest proof sequentially (default).')
    commands.add_parser('verus', help='Verify every production-body contract in the manifest.')
    shuttle = commands.add_parser('shuttle', help='Check production async interleavings with bounded Shuttle schedules.')
    shuttle.add_argument('--case', choices=(
        'cancellation_wakes_all', 'parent_and_owner_cancellation',
        'stage_watch_cancellation', 'drain_waits_for_completion',
        'registration_races_close', 'failed_worker_is_retained',
        'cancelled_drain_can_be_retried', 'abandoned_wait_can_be_retried'))
    shuttle.add_argument('--replay', help='Replay one hexadecimal failing schedule for the selected case.')
    kani = commands.add_parser('kani', help='Run exactly one experimental harness under the same limits.')
    kani.add_argument('source')
    kani.add_argument('--harness', required=True)
    # CBMC 6.11.0's non-FPA SMT lowering has a reproduced binary64 rounding
    # defect. Keep Kani on its SAT backends until a reviewed upstream upgrade.
    kani.add_argument('--solver', choices=('cadical', 'kissat'), default='cadical')
    kani.add_argument('--counterexample', action='store_true', help='Print concrete playback for a failed assertion inside the sandbox.')
    args = parser.parse_args(argv)
    if args.action == 'shuttle' and args.replay is not None:
        if not args.case or not re.fullmatch(r'[0-9a-fA-F]{1,4096}', args.replay):
            parser.error('Shuttle replay requires one known case and a bounded hexadecimal schedule.')
    if args.action == 'kani':
        if (not re.fullmatch(r'verification/kani/[a-z_]+\.rs', args.source)
                or not (ROOT / args.source).is_file()
                or not re.fullmatch(r'[a-z_][a-z0-9_]*', args.harness)):
            parser.error('Select a repository Kani source and one exact harness name.')
    return args


def main(argv=None):
    args = parse_args(argv)
    if any(os.environ.get(name) for name in ('KANI_BIN', 'VERUS_BIN', 'SHUTTLE_BIN')):
        raise Refusal('Host verifier overrides are disabled. Use the pinned sandbox tools.')
    with admission():
        docker = Docker()
        setup = args.action == 'setup'
        image = BASE if setup else image_name()
        if setup:
            docker.call(['pull', BASE], timeout=180)
        report_dir = CACHE / 'runs' / (time.strftime('%Y%m%d-%H%M%S-') + uuid.uuid4().hex[:8])
        report_dir.mkdir(parents=True, mode=0o700)
        # Stage below the checkout, which existing macOS VM configurations
        # already share. The host evidence cache need not be exposed to Docker.
        staging = ROOT / 'target' / 'formal-sandbox'
        staging.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(prefix='source-', dir=staging) as temporary:
            source = Path(temporary)
            digest = snapshot(source)
            frozen_image = image_name(source)
            if not setup:
                image = frozen_image
                if docker.call(['image', 'inspect', image], check=False).returncode:
                    raise Refusal('Sandbox tools are missing. Run scripts/verify-formal.sh setup first.')
            (report_dir / 'source.sha256').write_text(digest + '\n')
            command = ['bash', '/source/scripts/formal/setup.sh'] if setup else [
                'bash', '/source/scripts/formal/prove.sh', args.action or 'verify']
            if args.action == 'shuttle':
                command = ['bash', '/source/scripts/formal/shuttle.sh']
                if args.case:
                    command += [args.case]
                if args.replay:
                    command += [args.replay]
            if args.action == 'kani':
                command += [args.source, args.harness, args.solver]
                if args.counterexample:
                    command += ['--counterexample']
            report = run_container(docker, image, source, command, report_dir,
                                   deadline=900 if setup else args.timeout_seconds, setup=setup,
                                   memory=args.memory_mib * 1024**2,
                                   publish_image=frozen_image if setup else None)
        print(json.dumps(report, indent=2))
        return 0 if report['outcome'] == 'passed' else 1


if __name__ == '__main__':
    def interrupted(signum, _frame):
        raise KeyboardInterrupt(f'signal {signum}')
    for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(sig, interrupted)
    try:
        sys.exit(main())
    except (Refusal, OSError, subprocess.SubprocessError, KeyboardInterrupt) as error:
        print(f'Formal verification incomplete: {error}', file=sys.stderr)
        sys.exit(1)
