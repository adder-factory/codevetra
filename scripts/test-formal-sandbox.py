#!/usr/bin/env python3
"""Formal-run admission tests; --live also proves actual kernel containment."""

import importlib.util
import contextlib
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
import copy
from unittest.mock import patch

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location('formal_sandbox', Path(__file__).with_name('formal-sandbox.py'))
sandbox = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(sandbox)
SUITE_SPEC = importlib.util.spec_from_file_location('formal_suite', Path(__file__).parent / 'formal/suite.py')
suite = importlib.util.module_from_spec(SUITE_SPEC)
SUITE_SPEC.loader.exec_module(suite)
SHUTTLE_SPEC = importlib.util.spec_from_file_location('shuttle_suite', Path(__file__).parent / 'formal/shuttle.py')
shuttle = importlib.util.module_from_spec(SHUTTLE_SPEC)
SHUTTLE_SPEC.loader.exec_module(shuttle)


class AdmissionTests(unittest.TestCase):
    def test_shuttle_incomplete_success_is_rejected(self):
        complete = [f'SHUTTLE_PASSED {case} random=1000 pct=1000 seed=1\n' for case in shuttle.CASES]
        shuttle.require_completed(subprocess.CompletedProcess([], 0, ''.join(complete), ''), shuttle.CASES)
        for lines in [[], complete[:-1], complete + complete[:1],
                      [line.replace('pct=1000', 'pct=999') for line in complete]]:
            with self.subTest(lines=lines), self.assertRaises(SystemExit):
                shuttle.require_completed(subprocess.CompletedProcess([], 0, ''.join(lines), ''), shuttle.CASES)

    def test_shuttle_negative_controls_require_a_completed_replayable_failure(self):
        failure = 'deadlock\nfailing schedule:\n"\nabcd\n1234\n"'
        result = subprocess.CompletedProcess([], 101, '', failure)
        self.assertEqual(shuttle.failure_schedule(result, 'deadlock'), 'abcd1234')
        for code, output in [(0, failure), (-9, failure), (101, 'deadlock'),
                             (101, failure.replace('deadlock', 'unrelated failure'))]:
            with self.subTest(code=code, output=output), self.assertRaises(SystemExit):
                shuttle.failure_schedule(subprocess.CompletedProcess([], code, '', output), 'deadlock')

    def test_shuttle_replay_is_bounded_and_requires_a_known_case(self):
        parsed = sandbox.parse_args(['shuttle', '--case', 'cancellation_wakes_all', '--replay', 'abcd'])
        self.assertEqual(parsed.replay, 'abcd')
        for argv in [['shuttle', '--replay', 'abcd'],
                     ['shuttle', '--case', 'unknown'],
                     ['shuttle', '--iterations', '1000000000'],
                     ['shuttle', '--case', 'cancellation_wakes_all', '--replay', '../file'],
                     ['shuttle', '--case', 'cancellation_wakes_all', '--replay', 'a' * 4097]]:
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                sandbox.parse_args(argv)

    def test_shuttle_lockfile_changes_the_setup_image_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'scripts/formal').mkdir(parents=True)
            (root / 'verification/shuttle').mkdir(parents=True)
            for name in ['scripts/formal/setup.sh', 'verification/shuttle/Cargo.toml',
                         'verification/shuttle/Cargo.lock']:
                (root / name).write_text(name)
            first = sandbox.image_name(root)
            (root / 'verification/shuttle/Cargo.lock').write_text('changed dependency lock')
            self.assertNotEqual(first, sandbox.image_name(root))

    def test_inventory_cannot_silently_omit_or_duplicate_a_harness(self):
        manifest = json.loads((sandbox.ROOT / 'verification/contracts.json').read_text())
        self.assertGreater(suite.inventory(sandbox.ROOT, manifest), 1)
        for mode in ('omit', 'duplicate', 'unknown'):
            changed = copy.deepcopy(manifest)
            required = changed['kani'][0]['required']
            if mode == 'omit':
                required.pop()
            elif mode == 'duplicate':
                required.append(required[0])
            else:
                required.append('nonexistent_proof')
            with self.subTest(mode=mode), self.assertRaises(ValueError):
                suite.inventory(sandbox.ROOT, changed)

    def test_binding_preserves_body_and_rejects_signature_drift(self):
        source = 'fn checked(x: u8) -> u8 { /* } */ let s = r#"}"#; x + 1 }'
        self.assertEqual(suite.binding.extract(source, 'checked', 'fn checked(x: u8) -> u8'),
                         '{ /* } */ let s = r#"}"#; x + 1 }')
        with self.assertRaises(ValueError):
            suite.binding.extract(source, 'checked', 'fn checked(x: u16) -> u8')
        with self.assertRaises(ValueError):
            suite.binding.extract(source + source, 'checked', 'fn checked(x: u8) -> u8')

    def test_success_without_proof_count_is_rejected(self):
        for output in ('', 'Complete - 0 successfully verified harnesses, 0 failures, 0 total.'):
            response = subprocess.CompletedProcess([], 0, output, '')
            with patch.object(suite, 'run_recorded', return_value=response), contextlib.redirect_stdout(io.StringIO()), self.assertRaises(SystemExit):
                suite.checked(['unused'], r'Complete - 1 successfully verified harnesses, 0 failures, 1 total\.')

    def test_solver_annotation_cannot_restore_the_incorrect_smt_backend(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / 'verification/kani/example.rs'
            source.parent.mkdir(parents=True)
            manifest = {'schema': 1, 'kani': [{'source': 'verification/kani/example.rs',
                         'required': ['example'], 'experimental': {}}]}
            for attribute in ('#[kani::solver(z3)]', '#[cfg_attr(kani, kani::solver(z3))]'):
                source.write_text(f'#[kani::proof]\n{attribute}\nfn example() {{}}\n')
                with self.subTest(attribute=attribute), self.assertRaises(ValueError):
                    suite.inventory(root, manifest)

    def test_lock_rejects_overlap_and_releases_without_unlink(self):
        with tempfile.TemporaryDirectory() as directory:
            cache = Path(directory)
            with sandbox.admission(cache):
                inode = (cache / 'admission.lock').stat().st_ino
                with self.assertRaises(sandbox.Refusal):
                    with sandbox.admission(cache):
                        self.fail('overlapping owner admitted')
            with sandbox.admission(cache):
                self.assertEqual(inode, (cache / 'admission.lock').stat().st_ino)

    def test_foreign_or_orphan_container_is_preserved(self):
        class Existing:
            def inspect(self, _identity):
                return {'Config': {'Labels': {sandbox.LABEL: 'old-owner'}}}
        with self.assertRaises(sandbox.Refusal):
            sandbox.run_container(Existing(), 'unused', sandbox.ROOT, [], sandbox.ROOT)

    def test_unsafe_limits_cannot_be_requested(self):
        for memory, deadline in [(sandbox.MEMORY + 1, 300), (0, 300), (sandbox.MEMORY, 301)]:
            with self.subTest(memory=memory, deadline=deadline), self.assertRaises(sandbox.Refusal):
                sandbox.container_args('image', Path('/source'), [], deadline, memory, False, 'name', 'token')

    def test_host_verifier_override_is_rejected_before_docker(self):
        with patch.dict(os.environ, KANI_BIN='/native/kani'), patch.object(sandbox, 'Docker') as docker:
            with self.assertRaises(sandbox.Refusal):
                sandbox.main([])
            docker.assert_not_called()

    def test_remote_daemon_is_rejected(self):
        with patch.dict(os.environ, DOCKER_HOST='tcp://example.invalid:2375', DOCKER_CONTEXT=''):
            with self.assertRaises(sandbox.Refusal):
                sandbox.Docker()

    def test_experiments_require_one_harness_and_reject_raw_arguments(self):
        for argv in [['kani', 'verification/kani/structures.rs'],
                     ['kani', '../../outside.rs', '--harness', 'proof'],
                     ['kani', 'verification/kani/structures.rs', '--harness', 'proof', '--jobs', '8'],
                     ['kani', 'verification/kani/structures.rs', '--harness', 'proof', '--solver', 'z3'],
                     ['--timeout-seconds', '301'], ['--memory-mib', '4097'], ['--memory-mib', '0']]:
            with self.subTest(argv=argv), contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
                sandbox.parse_args(argv)

    def test_snapshot_excludes_generated_artifacts_and_private_state(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory)
            digest = sandbox.snapshot(source)
            self.assertEqual(len(digest), 64)
            self.assertTrue((source / 'verification/kani/structures.rs').is_file())
            self.assertFalse(list(source.rglob('*.out')))
            self.assertFalse((source / '.codevetra').exists())

    def test_snapshot_is_readable_without_container_dac_override(self):
        original = sandbox.ROOT / 'scripts/formal/entry.sh'
        original_mode = original.stat().st_mode
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory)
            previous = os.umask(0o077)
            try:
                sandbox.snapshot(source)
            finally:
                os.umask(previous)
            self.assertEqual(source.stat().st_mode & 0o777, 0o755)
            for path in source.rglob('*'):
                expected = 0o755 if path.is_dir() else 0o644
                self.assertEqual(path.stat().st_mode & 0o777, expected, str(path))
        self.assertEqual(original.stat().st_mode, original_mode)

    def test_failure_output_is_bounded_and_escapes_terminal_controls(self):
        with tempfile.TemporaryDirectory() as directory:
            report = Path(directory)
            (report / 'output.log').write_bytes(b'omitted' * 2000 + b'\x1b[31mPermission denied\n')
            stderr = io.StringIO()
            with contextlib.redirect_stderr(stderr):
                sandbox.print_failure_output(report)
            rendered = stderr.getvalue()
            self.assertNotIn('\x1b', rendered)
            tail = json.loads(rendered)['verifier_output_tail']
            self.assertEqual(len(tail.encode()), 8192)
            self.assertTrue(tail.endswith('Permission denied\n'))

    def test_missing_kernel_enforcement_is_rejected(self):
        info = {'OSType': 'linux', 'CgroupVersion': '2', 'MemoryLimit': True,
                'SwapLimit': True, 'PidsLimit': True, 'MemTotal': 16 * 1024**3}
        for missing in ('MemoryLimit', 'SwapLimit', 'PidsLimit'):
            with self.subTest(missing=missing), patch.dict(os.environ, DOCKER_HOST='unix:///local.sock', DOCKER_CONTEXT=''):
                response = subprocess.CompletedProcess([], 0, json.dumps({**info, missing: False}), '')
                with patch.object(sandbox.Docker, 'call', return_value=response), self.assertRaises(sandbox.Refusal):
                    sandbox.Docker()

    def test_cleanup_failure_cannot_leave_a_passing_result(self):
        class LostDaemon:
            prefix = ['ignored']
            env = {}
            calls = 0
            token = None
            def inspect(self, _identity):
                self.calls += 1
                if self.calls == 1:
                    return None
                if self.calls >= 3:
                    raise sandbox.Refusal('daemon unavailable during cleanup')
                return {'State': {'Running': False, 'ExitCode': 0, 'OOMKilled': False}}
            def call(self, args, **_kwargs):
                return subprocess.CompletedProcess(args, 0, 'owned-id', '')
        with tempfile.TemporaryDirectory() as directory, patch.object(sandbox.subprocess, 'Popen') as child:
            child.return_value.wait.return_value = 0
            child.return_value.poll.return_value = 0
            child.return_value.returncode = 0
            report_dir = Path(directory)
            with self.assertRaises(sandbox.Refusal):
                sandbox.run_container(LostDaemon(), 'image', sandbox.ROOT, ['true'], report_dir)
            report = json.loads((report_dir / 'result.json').read_text())
            self.assertEqual(report['outcome'], 'incomplete')
            self.assertEqual(report['cleanup'], 'unconfirmed')


def live_tests():
    docker = sandbox.Docker()
    # Use the small OS image: no actual proof is needed to test enforcement.
    docker.call(['pull', sandbox.BASE], timeout=180)
    with sandbox.admission(), tempfile.TemporaryDirectory(prefix='formal-safety-') as directory:
        reports = Path(directory)
        name = sandbox.NAME
        def run(label, command, seconds=10):
            report = sandbox.run_container(docker, sandbox.BASE, sandbox.ROOT, command,
                                           reports / label, deadline=seconds, memory=128 * 1024**2, name=name)
            assert report['cleanup'] == 'confirmed', report
            assert docker.inspect(name) is None
            return report
        good = run('success', ['bash', '-c', 'echo contained'])
        assert good['outcome'] == 'passed', good
        failed = run('proof-failure', ['bash', '-c', 'exit 23'])
        assert failed['outcome'] == 'failed' and failed['exit_code'] == 23, failed
        # Bounded to 128 MiB in-kernel, even though this asks for 256 MiB.
        oom = run('memory', ['bash', '-c', 'x=x; for ((i=0;i<28;i++)); do x=$x$x; done'])
        assert oom['outcome'] == 'memory_limit', oom
        timeout = run('timeout', ['bash', '-c', "trap '' TERM; setsid bash -c 'trap \"\" TERM; sleep 60' & wait"], 1)
        assert timeout['outcome'] == 'timeout_or_killed', timeout
        for death_signal in (signal.SIGTERM, signal.SIGKILL):
            label = 'cancel' if death_signal == signal.SIGTERM else 'supervisor-killed'
            # The child is a real host supervisor. Its Docker client and
            # in-container grandchild must not survive as unbounded work.
            pid = os.fork()
            if pid == 0:
                def cancel(_sig, _frame):
                    raise KeyboardInterrupt()
                signal.signal(signal.SIGTERM, cancel)
                try:
                    run(label, ['bash', '-c', 'sleep 60 & wait'], 5)
                except KeyboardInterrupt:
                    os._exit(1)
                except BaseException:
                    os._exit(2)
                os._exit(0)
            cid = None
            try:
                deadline = time.monotonic() + 10
                while time.monotonic() < deadline:
                    owned = docker.inspect(name)
                    if owned and owned['State']['Running']:
                        cid = owned['Id']
                        break
                    time.sleep(0.1)
                assert cid, 'supervisor did not start a container'
                os.kill(pid, death_signal)
                os.waitpid(pid, 0)
                if death_signal == signal.SIGTERM:
                    assert docker.inspect(cid) is None, 'cancelled container survived'
                    report = json.loads((reports / label / 'result.json').read_text())
                    assert report['cleanup'] == 'confirmed' and report['outcome'] == 'incomplete', report
                else:
                    with unittest.TestCase().assertRaises(sandbox.Refusal):
                        sandbox.run_container(docker, sandbox.BASE, sandbox.ROOT, ['true'], reports / 'overlap')
                    deadline = time.monotonic() + 12
                    while docker.inspect(cid)['State']['Running'] and time.monotonic() < deadline:
                        time.sleep(0.2)
                    state = docker.inspect(cid)['State']
                    assert not state['Running'] and state['ExitCode'] == 124, state
            finally:
                try:
                    os.kill(pid, signal.SIGKILL)
                    os.waitpid(pid, 0)
                except (ProcessLookupError, ChildProcessError):
                    pass
                if cid and docker.inspect(cid):
                    docker.call(['rm', '--force', cid])
        print('Live containment passed: success, failure, OOM, descendants, SIGTERM, SIGKILL, orphan fence.')
        for file in reports.glob('*/result.json'):
            print(file.parent.name, file.read_text().strip())


if __name__ == '__main__':
    if sys.argv[1:] == ['--live']:
        live_tests()
    else:
        unittest.main()
