#!/usr/bin/env python3
"""Run a bounded Cargo command using the verified local cache placement."""

import argparse
import os
from pathlib import Path
import platform
import signal
import socket
import subprocess
import time

from build_tools import ROOT, admission, cache_path, cache_policy, cache_root, environment


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
            subprocess.run(['taskkill', '/PID', str(child.pid), '/T', '/F'], check=True, timeout=15)
    else:
        signal_group(child.pid, signal.SIGTERM)
        deadline = time.monotonic() + 10
        while exit_status(child) is None and time.monotonic() < deadline:
            time.sleep(0.05)
        signal_group(child.pid, signal.SIGKILL)
    child.wait(timeout=10)


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


def run(command, env, timeout, lock=None):
    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    server = None
    child = None
    endpoint = None
    started = time.monotonic()
    peak = 0
    # Own a foreground server so cancellation reaps its compiler children too.
    # Do not stop or reconfigure another project's existing sccache daemon.
    env = dict(env)
    for key in ('SCCACHE_SERVER_UDS', 'SCCACHE_SERVER_PORT', 'SCCACHE_START_SERVER', 'SCCACHE_NO_DAEMON'):
        env.pop(key, None)
    if os.name == 'nt':
        with socket.socket() as probe:
            probe.bind(('127.0.0.1', 0))
            env['SCCACHE_SERVER_PORT'] = str(probe.getsockname()[1])
    else:
        endpoint = cache_root() / f'run-{os.getpid()}.sock'
        if len(os.fsencode(endpoint)) >= 100:
            raise ValueError('Cache root is too long for a private compiler-server socket')
        if endpoint.exists():
            raise ValueError('Compiler-server endpoint already exists; refusing to reuse it')
        env['SCCACHE_SERVER_UDS'] = str(endpoint)
    previous = signal.signal(signal.SIGTERM, interrupted)
    inherited = {'pass_fds': (lock.fileno(),)} if lock is not None and os.name != 'nt' else {}
    try:
        server_env = dict(env, SCCACHE_START_SERVER='1', SCCACHE_NO_DAEMON='1')
        server = subprocess.Popen(['sccache'], env=server_env, start_new_session=os.name != 'nt', **inherited)
        for _ in range(100):
            if exit_status(server) is not None:
                raise RuntimeError('Owned compiler server exited during startup')
            if os.name == 'nt':
                try:
                    with socket.create_connection(('127.0.0.1', int(env['SCCACHE_SERVER_PORT'])), timeout=0.1):
                        break
                except OSError:
                    pass
            elif endpoint.exists():
                break
            time.sleep(0.1)
        else:
            raise RuntimeError('Owned compiler server did not start within 10 seconds')
        child = subprocess.Popen(command, cwd=ROOT, env=env, start_new_session=os.name != 'nt', **inherited)
        while exit_status(child) is None:
            if exit_status(server) is not None:
                raise RuntimeError('Owned compiler server exited during the build')
            if time.monotonic() - started > timeout:
                raise TimeoutError('Build deadline exceeded')
            if os.name != 'nt':
                peak = max(peak, rss_kib([child.pid, server.pid]))
                if peak > 8 * 1024 * 1024:
                    raise RuntimeError('Build and compiler-server descendants exceeded the 8 GiB RSS budget')
            time.sleep(0.5)
        subprocess.run(['sccache', '--show-stats'], env=env, check=True, timeout=10)
        return exit_status(child)
    finally:
        try:
            if child is not None:
                terminate(child)
        finally:
            if server is not None:
                terminate(server)
            if endpoint is not None:
                endpoint.unlink(missing_ok=True)
            signal.signal(signal.SIGTERM, previous)
        print(f'[rust-dev] elapsed {time.monotonic() - started:.2f}s; peak {peak / 1024:.1f} MiB', flush=True)


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
            resolved = cache_path(value, policy)
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
        return run(['cargo', *command], env, args.timeout, lock)


if __name__ == '__main__':
    raise SystemExit(main())
