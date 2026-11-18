#!/usr/bin/env python3
"""Run a bounded Cargo command using the verified local cache placement."""

import argparse
import os
from pathlib import Path
import platform
import signal
import socket
import subprocess
import sys
import time

from build_tools import ROOT, admission, cache_path, cache_policy, cache_root, environment

KIB_PER_MIB = 1024
MAXIMUM_BUILD_RSS_KIB = 8 * 1024 * KIB_PER_MIB
MAXIMUM_SOCKET_PATH_BYTES = 100
SERVER_IDLE_GRACE_SECONDS = 30
SERVER_START_ATTEMPTS = 100
SERVER_POLL_SECONDS = 0.1
BUILD_POLL_SECONDS = 0.5
STATS_TIMEOUT_SECONDS = 10
TERMINATION_GRACE_SECONDS = 10
TERMINATION_POLL_SECONDS = 0.05
TASKKILL_TIMEOUT_SECONDS = 15


def exit_status(child):
    if os.name == 'nt':
        return child.poll()
    # Keep the leader unreaped until its group is cleaned up. Its zombie PID
    # reserves the group identity and cannot be recycled for an unrelated task.
    info = os.waitid(os.P_PID, child.pid, os.WEXITED | os.WNOHANG | os.WNOWAIT)
    if info is None:
        return None
    return info.si_status if info.si_code == os.CLD_EXITED else -info.si_status


def signal_group(pid, value):
    try:
        os.killpg(pid, value)
    except ProcessLookupError:
        pass
    except PermissionError:
        # Darwin returns EPERM for a group containing only zombies. Never
        # suppress permission failure while a live member remains.
        rows = subprocess.check_output(['ps', '-axo', 'pgid=,stat='], text=True)
        if any(int(group) == pid and not state.startswith('Z')
               for group, state in (row.split() for row in rows.splitlines())):
            raise


def terminate(child):
    if os.name == 'nt':
        if child.poll() is None:
            subprocess.run(['taskkill', '/PID', str(child.pid), '/T', '/F'], stdout=sys.stderr,
                           check=True, timeout=TASKKILL_TIMEOUT_SECONDS)
    else:
        signal_group(child.pid, signal.SIGTERM)
        deadline = time.monotonic() + TERMINATION_GRACE_SECONDS
        while exit_status(child) is None and time.monotonic() < deadline:
            time.sleep(TERMINATION_POLL_SECONDS)
        signal_group(child.pid, signal.SIGKILL)
    child.wait(timeout=TERMINATION_GRACE_SECONDS)


def rss_kib(roots):
    """Include compiler-server descendants, which are outside Cargo's group."""
    rows = subprocess.check_output(['ps', '-axo', 'pid=,ppid=,rss='], text=True)
    processes = [tuple(map(int, row.split())) for row in rows.splitlines()]
    owned = set(roots)
    while True:
        children = {pid for pid, parent, _ in processes if parent in owned}
        expanded = owned | children
        if expanded == owned:
            break
        owned = expanded
    return sum(rss for pid, _, rss in processes if pid in owned)


class BuildRun:
    """Own one Cargo process and its private compiler server until both are reaped."""

    def __init__(self, command, env, timeout):
        self.command = command
        self.env = dict(env)
        self.timeout = timeout
        self.server = None
        self.child = None
        self.endpoint = None
        self.started = None
        self.peak = 0

    def configure_endpoint(self):
        # Do not stop or reconfigure another project's existing daemon.
        for key in ('SCCACHE_SERVER_UDS', 'SCCACHE_SERVER_PORT', 'SCCACHE_START_SERVER', 'SCCACHE_NO_DAEMON'):
            self.env.pop(key, None)
        if os.name == 'nt':
            with socket.socket() as probe:
                probe.bind(('127.0.0.1', 0))
                self.env['SCCACHE_SERVER_PORT'] = str(probe.getsockname()[1])
        else:
            self.endpoint = cache_root() / f'run-{os.getpid()}.sock'
            if len(os.fsencode(self.endpoint)) >= MAXIMUM_SOCKET_PATH_BYTES:
                raise ValueError('Cache root is too long for a private compiler-server socket')
            if self.endpoint.exists():
                raise ValueError('Compiler-server endpoint already exists; refusing to reuse it')
            self.env['SCCACHE_SERVER_UDS'] = str(self.endpoint)

    def launch_server(self, inherited):
        # Idle test/soak phases can make no compiler requests. The finite idle
        # exit still releases an orphan's admission after supervisor SIGKILL.
        server_env = dict(self.env, SCCACHE_START_SERVER='1', SCCACHE_NO_DAEMON='1',
                          SCCACHE_IDLE_TIMEOUT=str(self.timeout + SERVER_IDLE_GRACE_SECONDS))
        # sccache closes inherited descriptors; this waiting wrapper retains
        # the admission FD and lets the child keep default SIGTERM handling.
        command = [sys.executable, '-c',
                   'import signal,subprocess,sys; '
                   'signal.signal(signal.SIGTERM, lambda *_: None); '
                   'sys.exit(subprocess.call(["sccache"]))']
        self.server = subprocess.Popen(command, env=server_env, stdout=sys.stderr,
                                       start_new_session=os.name != 'nt', **inherited)
        self.wait_for_server()

    def server_ready(self):
        if exit_status(self.server) is not None:
            raise RuntimeError('Owned compiler server exited during startup')
        if os.name != 'nt':
            return self.endpoint.exists()
        try:
            with socket.create_connection(('127.0.0.1', int(self.env['SCCACHE_SERVER_PORT'])), timeout=SERVER_POLL_SECONDS):
                return True
        except OSError:
            return False

    def wait_for_server(self):
        for _ in range(SERVER_START_ATTEMPTS):
            if self.server_ready():
                return
            time.sleep(SERVER_POLL_SECONDS)
        raise RuntimeError('Owned compiler server did not start within 10 seconds')

    def supervise(self, inherited):
        self.launch_server(inherited)
        self.child = subprocess.Popen(self.command, cwd=ROOT, env=self.env, start_new_session=os.name != 'nt', **inherited)
        while exit_status(self.child) is None:
            if exit_status(self.server) is not None:
                raise RuntimeError('Owned compiler server exited during the build')
            if time.monotonic() - self.started > self.timeout:
                raise TimeoutError('Build deadline exceeded')
            if os.name != 'nt':
                self.peak = max(self.peak, rss_kib([self.child.pid, self.server.pid]))
                if self.peak > MAXIMUM_BUILD_RSS_KIB:
                    raise RuntimeError('Build and compiler-server descendants exceeded the 8 GiB RSS budget')
            time.sleep(BUILD_POLL_SECONDS)
        # Cargo owns stdout, including JSON diagnostics and llvm-cov shell exports.
        subprocess.run(['sccache', '--show-stats'], env=self.env, stdout=sys.stderr,
                       check=True, timeout=STATS_TIMEOUT_SECONDS)
        return exit_status(self.child)

    def cleanup(self):
        try:
            if self.child is not None:
                terminate(self.child)
        finally:
            if self.server is not None:
                terminate(self.server)
            if self.endpoint is not None:
                self.endpoint.unlink(missing_ok=True)

    def run(self, lock=None):
        if self.started is not None:
            raise RuntimeError('A build owner cannot be reused')
        self.started = time.monotonic()
        self.configure_endpoint()

        def interrupted(_signal, _frame):
            raise KeyboardInterrupt

        previous = signal.signal(signal.SIGTERM, interrupted)
        try:
            inherited = {'pass_fds': (lock.fileno(),)} if lock is not None and os.name != 'nt' else {}
            return self.supervise(inherited)
        finally:
            try:
                self.cleanup()
            finally:
                # Failed cleanup must not strand the caller's signal handler.
                signal.signal(signal.SIGTERM, previous)
                print(f'[rust-dev] elapsed {time.monotonic() - self.started:.2f}s; peak {self.peak / KIB_PER_MIB:.1f} MiB', file=sys.stderr, flush=True)


def validate_arguments(arguments, policy):
    if 'requiredVolume' not in policy:
        return
    paths = {'--target-dir', '--out-dir', '-o', '--archive-file', '--extract-to', '--root',
             '--target-dir-remap', '--build-dir-remap'}
    for index, argument in enumerate(arguments):
        name, separator, value = argument.partition('=')
        if name in ('--config', '--config-file', '--tool-config-file'):
            raise ValueError('Inline Cargo configuration can bypass the cache-volume policy')
        if argument.startswith('-o') and not argument.startswith('--') and len(argument) > 2:
            name, separator, value = '-o', '=', argument[2:]
        if name in paths:
            if not separator:
                if index + 1 == len(arguments):
                    raise ValueError('Missing cache/output path argument')
                value = arguments[index + 1]
            candidate = Path(value)
            if not candidate.is_absolute():
                candidate = ROOT / candidate
            resolved = cache_path(candidate, policy)
            if name == '--target-dir' and resolved != Path(policy['targetRoot']).resolve():
                raise ValueError('Cargo target directory differs from its recorded cache policy')


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--timeout', type=int, default=3600)
    parser.add_argument('--cache-root', action='store_true')
    parser.add_argument('cargo_args', nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if args.cache_root:
        print(cache_root())
        return 0
    if not 1 <= args.timeout <= 7200:
        parser.error('timeout must be between 1 and 7200 seconds')
    command = args.cargo_args
    if command[:1] == ['--']:
        command = command[1:]
    if not command:
        parser.error('provide Cargo arguments, for example: nextest run -p codevetra-domain')
    env = environment()
    validate_arguments(command, cache_policy())
    if platform.system() == 'Darwin':
        pressure = subprocess.check_output(['sysctl', '-n', 'kern.memorystatus_vm_pressure_level'], text=True).strip()
        if pressure != '1':
            raise RuntimeError('Build admission refused: macOS memory pressure is not normal')
    with admission() as lock:
        return BuildRun(['cargo', *command], env, args.timeout).run(lock)


if __name__ == '__main__':
    raise SystemExit(main())
