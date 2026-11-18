#!/usr/bin/env python3
"""Run formal verifiers only inside a bounded, single-admission Linux container."""

import argparse
import contextlib
from dataclasses import dataclass
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
BASE = 'ubuntu@sha256:73b999393d07f2bf65a40373778a44dc9a6d54ad7718d29247d8ff9413c4d6a8'
LABEL = 'org.codevetra.formal.run'
MEMORY = 4 * 1024**3
NAME = f'codevetra-formal-{os.getuid()}'
CACHE = Path.home() / '.cache' / 'codevetra-formal'
MEBIBYTE = 1024**2
MINIMUM_MEMORY_BYTES = 64 * MEBIBYTE
MAXIMUM_PROOF_SECONDS = 300
MAXIMUM_SETUP_SECONDS = 900
DOCKER_CREATE_TIMEOUT_SECONDS = 180
ATTACH_GRACE_SECONDS = 15
CLIENT_STOP_SECONDS = 5
MINIMUM_CLI_MEMORY_MIB = 128
PRIVATE_DIRECTORY_MODE = 0o700
PUBLIC_DIRECTORY_MODE = 0o755
PUBLIC_SOURCE_MODE = 0o644
MAXIMUM_FAILURE_OUTPUT_BYTES = 8192
MAXIMUM_CLIENT_LOG_BYTES = 16 * MEBIBYTE


class Refusal(RuntimeError):
    """No verifier may be started when containment cannot be established."""


@contextlib.contextmanager
def admission(cache=CACHE):
    cache.mkdir(parents=True, exist_ok=True, mode=PRIVATE_DIRECTORY_MODE)
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
        self.prefix = ['docker']
        context = [self.env['DOCKER_CONTEXT']] if self.env.get('DOCKER_CONTEXT') else []
        host = self.env.get('DOCKER_HOST') if not context else None
        if not host:
            host = self.call(['context', 'inspect', *context, '--format',
                              '{{.Endpoints.docker.Host}}']).stdout.strip()
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
        kwargs.setdefault('timeout', 30)
        kwargs.setdefault('check', True)
        try:
            return subprocess.run([*self.prefix, *args], env=self.env,
                                  capture_output=True, text=True, **kwargs)
        except subprocess.CalledProcessError as error:
            detail = (error.stderr or '').strip()[-2000:]
            raise Refusal(f'Command failed ({error.returncode}): {detail}') from None

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
    resource.setrlimit(resource.RLIMIT_FSIZE, (MAXIMUM_CLIENT_LOG_BYTES, MAXIMUM_CLIENT_LOG_BYTES))


@dataclass(frozen=True, kw_only=True)
class ContainerSpec:
    """Immutable named settings, separate from the execution's mutable ownership state."""
    image: str
    source: Path
    command: tuple[str, ...]
    deadline: int = MAXIMUM_PROOF_SECONDS
    memory: int = MEMORY
    setup: bool = False
    name: str = NAME
    publish_image: str | None = None

    def arguments(self, token):
        maximum_seconds = MAXIMUM_SETUP_SECONDS if self.setup else MAXIMUM_PROOF_SECONDS
        if not MINIMUM_MEMORY_BYTES <= self.memory <= MEMORY or not 1 <= self.deadline <= maximum_seconds:
            raise Refusal('Requested resources exceed the reviewed safety bounds.')
        args = ['create', '--name', self.name, '--label', f'{LABEL}={token}', '--init',
                '--memory', str(self.memory), '--memory-swap', str(self.memory), '--cpus', '2',
                '--pids-limit', '128', '--ulimit', 'core=0',
                '--security-opt', 'no-new-privileges', '--restart', 'no',
                '--log-driver', 'local', '--log-opt', 'max-size=8m', '--log-opt', 'max-file=2',
                '--mount', f'type=bind,src={self.source},dst=/source,readonly',
                '--network', 'bridge' if self.setup else 'none']
        if not self.setup:
            args += ['--read-only', '--cap-drop', 'ALL',
                     '--tmpfs', '/work:rw,exec,size=1073741824',
                     '--tmpfs', '/tmp:rw,exec,size=268435456']
        return [*args, '--entrypoint', '/bin/bash', self.image, '/source/scripts/formal/entry.sh',
                str(self.deadline), *self.command]


def container_outcome(state):
    if state['OOMKilled']:
        return 'memory_limit'
    if state['ExitCode'] in (124, 137):
        return 'timeout_or_killed'
    return 'failed' if state['ExitCode'] else 'passed'


class ContainerRun:
    """Retain the ownership token, attach client, and durable outcome until cleanup completes."""
    def __init__(self, docker, spec, report_dir):
        self.docker = docker
        self.spec = spec
        self.report_dir = report_dir
        self.result_path = report_dir / 'result.json'
        self.token = uuid.uuid4().hex
        self.cid = None
        self.client = None
        self.report = {'command': list(spec.command), 'memory_bytes': spec.memory, 'swap_bytes': 0,
                       'deadline_seconds': spec.deadline, 'outcome': 'incomplete', 'cleanup': 'not_started'}

    def write_report(self):
        self.result_path.write_text(json.dumps(self.report, indent=2) + '\n')

    def run(self):
        # The fixed daemon-side name also fences orphaned runs after host SIGKILL.
        if self.docker.inspect(self.spec.name) is not None:
            raise Refusal(f'{self.spec.name} still exists. Inspect it before retrying; it will not be deleted automatically.')
        self.report_dir.mkdir(parents=True, exist_ok=True, mode=PRIVATE_DIRECTORY_MODE)
        self.write_report()
        try:
            self.create()
            self.wait_for_exit()
            self.confirm_completion()
            if self.spec.setup and self.report['outcome'] == 'passed':
                if self.spec.publish_image is None:
                    raise Refusal('Missing frozen setup image identity.')
                self.docker.call(['commit', self.cid, self.spec.publish_image], timeout=DOCKER_CREATE_TIMEOUT_SECONDS)
            return self.report
        except BaseException:
            if self.report['outcome'] == 'passed':
                self.report['outcome'] = 'incomplete'
            raise
        finally:
            self.cleanup()

    def create(self):
        args = self.spec.arguments(self.token)
        self.cid = self.docker.call(args, timeout=DOCKER_CREATE_TIMEOUT_SECONDS).stdout.strip()
        self.report['container_id'] = self.cid
        self.write_report()
        print(f'Formal run: {self.report_dir} (limit {self.spec.memory // MEBIBYTE} MiB, {self.spec.deadline}s)', flush=True)

    def wait_for_exit(self):
        with (self.report_dir / 'output.log').open('w') as log:
            self.client = subprocess.Popen([*self.docker.prefix, 'start', '--attach', self.cid],
                                           env=self.docker.env, stdout=log, stderr=subprocess.STDOUT,
                                           preexec_fn=bound_log)
            try:
                self.client.wait(timeout=self.spec.deadline + ATTACH_GRACE_SECONDS)
            except subprocess.TimeoutExpired:
                self.report['outcome'] = 'timeout'
                raise Refusal('Host deadline expired; terminating the entire proof container.')

    def confirm_completion(self):
        inspected = self.docker.inspect(self.cid)
        if inspected is None or inspected['State']['Running']:
            raise Refusal('Cannot confirm verifier completion.')
        state = inspected['State']
        if self.client.returncode != state['ExitCode']:
            raise Refusal('Docker attach status disagrees with the container; evidence is incomplete.')
        self.report.update(exit_code=state['ExitCode'], oom_killed=state['OOMKilled'],
                           outcome=container_outcome(state))

    def stop_client(self):
        if self.client is not None and self.client.poll() is None:
            self.client.terminate()
            try:
                self.client.wait(timeout=CLIENT_STOP_SECONDS)
            except subprocess.TimeoutExpired:
                self.client.kill()
                self.client.wait(timeout=CLIENT_STOP_SECONDS)

    def cleanup(self):
        previous_handlers = {sig: signal.signal(sig, signal.SIG_IGN)
                             for sig in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP)}
        # Recover a lost create response only by this run's unique ownership token.
        try:
            owned = self.docker.inspect(self.cid or self.spec.name)
            if owned is not None and owned['Config'].get('Labels', {}).get(LABEL) == self.token:
                self.docker.call(['rm', '--force', owned['Id']])
                if self.docker.inspect(owned['Id']) is not None:
                    raise Refusal('Container cleanup unconfirmed; admission remains fenced by its name.')
                self.report['cleanup'] = 'confirmed'
            elif self.cid is not None:
                raise Refusal('Owned container disappeared or changed ownership; completion is unconfirmed.')
        except BaseException:
            self.report.update(outcome='incomplete', cleanup='unconfirmed')
            raise
        finally:
            self.stop_client()
            self.write_report()
            for sig, handler in previous_handlers.items():
                signal.signal(sig, handler)


def run_container(docker, spec, report_dir):
    return ContainerRun(docker, spec, report_dir).run()


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
        target.chmod(PUBLIC_SOURCE_MODE)
    # The container drops DAC_OVERRIDE and may have a different UID from the
    # host. Publish this allowlisted public-source copy with explicit read and
    # traversal permissions, independent of the host's umask. Original source
    # and private evidence permissions remain untouched.
    for path in destination.rglob('*'):
        if path.is_dir():
            path.chmod(PUBLIC_DIRECTORY_MODE)
    destination.chmod(PUBLIC_DIRECTORY_MODE)
    return digest.hexdigest()


def print_failure_output(report_dir):
    with (report_dir / 'output.log').open('rb') as output:
        output.seek(0, os.SEEK_END)
        output.seek(max(0, output.tell() - MAXIMUM_FAILURE_OUTPUT_BYTES))
        tail = output.read(MAXIMUM_FAILURE_OUTPUT_BYTES).decode('utf-8', errors='replace')
    # Escape terminal controls and keep CI diagnostics bounded.
    print(json.dumps({'verifier_output_tail': tail}), file=sys.stderr)


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--timeout-seconds', type=int, default=MAXIMUM_PROOF_SECONDS, choices=range(1, MAXIMUM_PROOF_SECONDS + 1), metavar='1..300')
    parser.add_argument('--memory-mib', type=int, default=MEMORY // MEBIBYTE, choices=range(MINIMUM_CLI_MEMORY_MIB, MEMORY // MEBIBYTE + 1),
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


def sandbox_command(args):
    command = ['bash', '/source/scripts/formal/setup.sh'] if args.action == 'setup' else [
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
    return tuple(command)


def main(argv=None):
    args = parse_args(argv)
    if any(os.environ.get(name) for name in ('KANI_BIN', 'VERUS_BIN', 'SHUTTLE_BIN')):
        raise Refusal('Host verifier overrides are disabled. Use the pinned sandbox tools.')
    with admission():
        docker = Docker()
        setup = args.action == 'setup'
        image = BASE if setup else image_name()
        if setup:
            docker.call(['pull', BASE], timeout=DOCKER_CREATE_TIMEOUT_SECONDS)
        report_dir = CACHE / 'runs' / (time.strftime('%Y%m%d-%H%M%S-') + uuid.uuid4().hex[:8])
        report_dir.mkdir(parents=True, mode=PRIVATE_DIRECTORY_MODE)
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
            spec = ContainerSpec(image=image, source=source, command=sandbox_command(args),
                                 deadline=MAXIMUM_SETUP_SECONDS if setup else args.timeout_seconds,
                                 setup=setup, memory=args.memory_mib * MEBIBYTE,
                                 publish_image=frozen_image if setup else None)
            report = run_container(docker, spec, report_dir)
        print(json.dumps(report, indent=2))
        if report['outcome'] != 'passed':
            print_failure_output(report_dir)
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
