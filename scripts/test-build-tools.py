#!/usr/bin/env python3
"""Regression checks for tool integrity, cache placement and run admission."""

import hashlib
import io
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

import build_tools as tools


class BuildToolsTests(unittest.TestCase):
    def setUp(self):
        # Even fixture artifacts stay on the selected Rust-cache filesystem.
        root = tools.cache_root() / 'test-fixtures'
        root.mkdir(parents=True, exist_ok=True)
        self.temp = tempfile.TemporaryDirectory(dir=root)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)

    def archive(self, members):
        archive = self.root / 'tool.tar.gz'
        with tarfile.open(archive, 'w:gz') as output:
            for name, content, kind in members:
                entry = tarfile.TarInfo(name)
                entry.type = kind
                entry.size = len(content)
                entry.linkname = '../foreign'
                output.addfile(entry, io.BytesIO(content))
        return archive

    def test_archive_paths_are_never_extracted(self):
        archive = self.archive([('../../outside/sccache', b'payload', tarfile.REGTYPE)])
        destination = self.root / 'selected'
        tools.extract_binary(archive, 'sccache', destination)
        self.assertEqual(destination.read_bytes(), b'payload')
        self.assertFalse((self.root.parent / 'outside').exists())

    def test_symlink_and_duplicate_executables_are_rejected(self):
        for entries in [[('sccache', b'', tarfile.SYMTYPE)],
                        [('a/sccache', b'a', tarfile.REGTYPE), ('b/sccache', b'b', tarfile.REGTYPE)]]:
            with self.subTest(entries=entries):
                archive = self.archive(entries)
                with self.assertRaises(ValueError):
                    tools.extract_binary(archive, 'sccache', self.root / 'selected')

    def test_executable_named_directory_does_not_hide_the_regular_binary(self):
        archive = self.archive([('lib/mold', b'', tarfile.DIRTYPE), ('bin/mold', b'binary', tarfile.REGTYPE)])
        destination = self.root / 'selected'
        tools.extract_binary(archive, 'mold', destination)
        self.assertEqual(destination.read_bytes(), b'binary')

    def test_download_checksum_is_required(self):
        url = 'https://github.com/mozilla/sccache/releases/download/v0.19.0/tool.tar.gz'
        with patch('urllib.request.urlopen', return_value=io.BytesIO(b'wrong')):
            with self.assertRaisesRegex(ValueError, 'checksum'):
                tools.download(url, self.root / 'download', hashlib.sha256(b'expected').hexdigest())

    def test_download_byte_bound_is_enforced(self):
        with patch.object(tools, 'MAX_DOWNLOAD', 3), patch('urllib.request.urlopen', return_value=io.BytesIO(b'oversized')):
            with self.assertRaisesRegex(ValueError, 'byte limit'):
                tools.download('https://github.com/a/b/releases/download/v1/tool.tar.gz', self.root / 'download', '0' * 64)

    def test_missing_volume_refuses_before_creating_a_cache(self):
        policy = self.root / 'policy.json'
        missing = self.root / 'missing-volume'
        policy.write_text(json.dumps({'requiredVolume': str(missing), 'volumeUUID': 'expected',
                                      'cacheRoot': str(missing / 'cache'), 'targetRoot': str(missing / 'target')}))
        with patch.object(tools, 'POLICY', policy):
            with self.assertRaisesRegex(ValueError, 'not mounted'):
                tools.environment()
        self.assertFalse(missing.exists())

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS volume identity policy')
    def test_replaced_volume_is_rejected(self):
        policy = tools.cache_policy()
        if 'requiredVolume' not in policy:
            self.skipTest('No local external-volume policy configured')
        policy['volumeUUID'] = 'wrong-volume'
        with self.assertRaisesRegex(ValueError, 'identity does not match'):
            tools.validate_volume(policy)

    @unittest.skipUnless(sys.platform == 'darwin', 'macOS volume identity policy')
    def test_target_override_cannot_escape_required_volume(self):
        policy = tools.cache_policy()
        if 'requiredVolume' not in policy:
            self.skipTest('No local external-volume policy configured')
        with patch.dict(os.environ, {'CARGO_TARGET_DIR': str(Path.home() / 'forbidden-target')}):
            with self.assertRaisesRegex(ValueError, 'target directory differs'):
                tools.environment()

    def test_mac_without_policy_refuses_before_creating_cache(self):
        missing = self.root / 'absent-policy.json'
        output = self.root / 'must-not-exist'
        with patch.object(tools, 'POLICY', missing), patch('platform.system', return_value='Darwin'), \
                patch.dict(os.environ, {'GITHUB_ACTIONS': 'false', 'CODEVETRA_BUILD_CACHE_ROOT': str(output)}):
            with self.assertRaisesRegex(ValueError, 'explicit cache-volume policy'):
                tools.environment()
        self.assertFalse(output.exists())

    def test_ci_temporary_fixtures_do_not_discover_the_checkout_git_repository(self):
        checkout = self.root / 'checkout'
        checkout.mkdir()
        subprocess.run(['git', '-c', 'init.templateDir=', 'init', '--quiet', str(checkout)], check=True)
        policy = {'cacheRoot': str(checkout / 'target/build-cache'), 'targetRoot': str(checkout / 'target')}
        with patch.object(tools, 'cache_policy', return_value=policy), \
                patch.dict(os.environ, {'GITHUB_ACTIONS': 'true', 'RUNNER_TEMP': str(self.root / 'runner-temp')}):
            env = tools.environment()
        fixture = Path(env['TMPDIR']) / 'not-a-repository'
        fixture.mkdir()
        result = subprocess.run(['git', '-C', str(fixture), 'rev-parse', '--show-toplevel'],
                                env=dict(os.environ, GIT_CEILING_DIRECTORIES=str(self.root)),
                                capture_output=True, text=True, timeout=5)
        self.assertNotEqual(result.returncode, 0)
        self.assertTrue(Path(env['SCCACHE_DIR']).is_relative_to(Path(policy['cacheRoot'])))

    @unittest.skipIf(os.name == 'nt', 'Unix symlink escape')
    def test_cache_children_cannot_redirect_to_main_disk(self):
        policy = tools.cache_policy()
        if 'requiredVolume' not in policy:
            self.skipTest('No local external-volume policy configured')
        policy['cacheRoot'] = str(self.root)
        for name in ('bootstrap', 'sccache', 'linux', 'tmp'):
            link = self.root / name
            link.symlink_to(Path.home(), target_is_directory=True)
            with self.subTest(name=name), self.assertRaisesRegex(ValueError, 'escapes'):
                tools.cache_path(link / 'must-not-create', policy)
        with patch.object(tools, 'cache_policy', return_value=policy):
            with self.assertRaisesRegex(ValueError, 'escapes'):
                tools.install(['sccache'], self.root / 'bin')
            with self.assertRaisesRegex(ValueError, 'escapes'):
                tools.link_local_cache()

    def runner_module(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location('rust_dev_test', tools.ROOT / 'scripts/rust-dev.py')
        runner = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(runner)
        return runner

    def test_output_location_overrides_cannot_escape_the_policy(self):
        runner = self.runner_module()
        policy = {'requiredVolume': str(self.root), 'targetRoot': str(self.root / 'target')}
        outside = str(self.root.parent / 'must-not-exist')
        for args in [['build', '--target-dir', outside], ['nextest', 'run', '--target-dir=' + outside],
                     ['nextest', 'archive', '--archive-file', outside], ['nextest', 'run', '--extract-to', outside],
                     ['build', '--config', 'build.target-dir="' + outside + '"'], ['rustc', '--', '-o' + outside]]:
            with self.subTest(args=args), self.assertRaises(ValueError):
                runner.validate_arguments(args, policy)
        self.assertFalse(Path(outside).exists())

    def test_relative_output_paths_are_resolved_from_the_cargo_checkout(self):
        runner = self.runner_module()
        volume = self.root / 'volume'
        (volume / 'target').mkdir(parents=True)
        checkout = self.root / 'checkout'
        checkout.mkdir()
        policy = {'requiredVolume': str(volume), 'targetRoot': str(volume / 'target')}
        previous = Path.cwd()
        try:
            os.chdir(volume)
            with patch.object(runner, 'ROOT', checkout), self.assertRaisesRegex(ValueError, 'escapes'):
                runner.validate_arguments(['build', '--target-dir', 'target'], policy)
        finally:
            os.chdir(previous)

    def test_bad_download_preserves_installed_tool(self):
        destination = self.root / 'bin'
        destination.mkdir()
        executable = destination / ('sccache.exe' if os.name == 'nt' else 'sccache')
        executable.write_bytes(b'previous verified tool')
        with patch.object(tools, 'download', side_effect=ValueError('checksum mismatch')):
            with self.assertRaisesRegex(ValueError, 'checksum'):
                tools.install(['sccache'], destination)
        self.assertEqual(executable.read_bytes(), b'previous verified tool')

    def test_snapshot_excludes_ignored_private_data_and_detects_source_changes(self):
        checkout = self.root / 'checkout'
        checkout.mkdir()
        subprocess.run(['git', '-c', 'init.templateDir=', 'init', '--quiet', str(checkout)], check=True)
        (checkout / '.gitignore').write_text('.codevetra/\ntarget/\n')
        (checkout / '.codevetra').mkdir()
        (checkout / '.codevetra/secret').write_text('private fixture')
        (checkout / 'public.rs').write_text('pub fn answer() -> u8 { 42 }')
        staged = self.root / 'staged'
        with patch.object(tools, 'ROOT', checkout):
            first = tools.snapshot(staged)
            self.assertEqual(first, tools.snapshot())
            (checkout / 'public.rs').write_text('pub fn answer() -> u8 { 43 }')
            self.assertNotEqual(first, tools.snapshot())
        self.assertFalse((staged / '.codevetra').exists())
        self.assertFalse((staged / '.git').exists())
        self.assertIn('42', (staged / 'public.rs').read_text())

    @unittest.skipIf(os.name == 'nt', 'Unix symlink escape')
    def test_snapshot_rejects_symlink_to_private_input(self):
        checkout = self.root / 'checkout'
        checkout.mkdir()
        subprocess.run(['git', '-c', 'init.templateDir=', 'init', '--quiet', str(checkout)], check=True)
        (self.root / 'private').write_text('private fixture')
        (checkout / 'source.rs').symlink_to(self.root / 'private')
        with patch.object(tools, 'ROOT', checkout):
            with self.assertRaisesRegex(ValueError, 'escaped'):
                tools.snapshot(self.root / 'staged')

    def test_snapshot_freshness_rebuilds_rejected_content_then_reuses_identical_binary(self):
        import shutil
        import tomllib
        checkout = self.root / 'checkout'
        (checkout / 'src').mkdir(parents=True)
        subprocess.run(['git', '-c', 'init.templateDir=', 'init', '--quiet', str(checkout)], check=True)
        (checkout / 'Cargo.toml').write_text('[package]\nname = "freshness_probe"\nversion = "0.0.0"\nedition = "2024"\n[workspace]\n')
        (checkout / 'Cargo.lock').write_text('version = 4\n[[package]]\nname = "freshness_probe"\nversion = "0.0.0"\n')
        source = checkout / 'src/main.rs'
        source.write_text('fn main() { println!("A"); }')
        cache = self.root / 'cache'
        target = cache / 'target'
        staged = self.root / 'staged'
        env = tools.environment()
        policy = tools.cache_policy()
        env.update(CARGO_TARGET_DIR=str(target), CARGO_BUILD_BUILD_DIR=str(target),
                   CARGO_BUILD_JOBS='1', CARGO_INCREMENTAL='0', RUSTC_WRAPPER='',
                   RUSTUP_TOOLCHAIN=tomllib.loads((tools.ROOT / 'rust-toolchain.toml').read_text())['toolchain']['channel'])
        for name in ('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_BUILD_TARGET', 'RUSTC_WORKSPACE_WRAPPER'):
            env.pop(name, None)

        def build():
            result = subprocess.run(['cargo', 'build', '--offline', '--locked', '--message-format=json'],
                                    cwd=staged, env=env, text=True, capture_output=True, timeout=30, check=True)
            artifacts = [row for row in map(json.loads, result.stdout.splitlines())
                         if row.get('reason') == 'compiler-artifact' and row.get('executable')]
            self.assertEqual(len(artifacts), 1)
            artifact = artifacts[0]
            executable = Path(artifact['executable'])
            output = subprocess.check_output([str(executable)], text=True, timeout=5).strip()
            return artifact['fresh'], executable, output

        # This tiny, offline compiler fixture owns an isolated target/cache;
        # it never acquires or mutates the workspace's build artifacts.
        with patch.object(tools, 'cache_root', return_value=cache), tools.admission(), \
                patch.object(tools, 'ROOT', checkout), patch.object(tools, 'cache_policy', return_value=policy):
            first = tools.snapshot(staged, build_cache=cache)
            # B predates A's rustc invocation. A later fails the source fence,
            # but its cached executable and dep-info are already present.
            source.write_text('fn main() { println!("B"); }')
            os.utime(source, ns=(1_000_000_000, 1_000_000_000))
            fresh, _, output = build()
            self.assertFalse(fresh)
            self.assertEqual(output, 'A')
            self.assertNotEqual(first, tools.snapshot())
            anchors = list(target.rglob('dep-bin-freshness_probe'))
            self.assertEqual(len(anchors), 1)
            anchor = anchors[0].stat().st_mtime_ns
            self.assertLess(source.stat().st_mtime_ns, anchor)
            shutil.rmtree(staged)
            second = tools.snapshot(staged, build_cache=cache)
            self.assertGreater((staged / 'src/main.rs').stat().st_mtime_ns, anchor)
            self.assertNotEqual(first, second)
            fresh, executable, output = build()
            self.assertFalse(fresh)
            self.assertEqual(output, 'B')
            before = (executable.stat().st_mtime_ns, tools.sha256(executable))
            source.touch()
            shutil.rmtree(staged)
            self.assertEqual(second, tools.snapshot(staged, build_cache=cache))
            fresh, executable, output = build()
            self.assertTrue(fresh)
            self.assertEqual(output, 'B')
            self.assertEqual(before, (executable.stat().st_mtime_ns, tools.sha256(executable)))

    def test_future_cache_timestamp_refuses_changed_source_reuse(self):
        cache = self.root / 'cache'
        target = cache / 'target'
        target.mkdir(parents=True)
        anchor = target / 'dep-info'
        anchor.write_bytes(b'cached artifact')
        future = time.time_ns() + 60_000_000_000
        os.utime(anchor, ns=(future, future))
        staged = self.root / 'source.rs'
        staged.write_text('changed')
        with self.assertRaisesRegex(ValueError, 'future timestamps'):
            tools.source_freshness([staged], 'a' * 64, cache)
        self.assertFalse((cache / 'source-freshness.json').exists())

    def test_coarse_timestamps_cannot_claim_changed_source_freshness(self):
        cache = self.root / 'cache'
        target = cache / 'target'
        target.mkdir(parents=True)
        anchor = target / 'dep-info'
        anchor.write_bytes(b'cached artifact')
        staged = self.root / 'source.rs'
        staged.write_text('changed')
        os.utime(staged, ns=(1_000_000_000, 1_000_000_000))
        with patch('os.utime'), patch('time.monotonic', side_effect=[0, 3]):
            with self.assertRaisesRegex(ValueError, 'cannot prove source freshness'):
                tools.source_freshness([staged], 'a' * 64, cache)
        self.assertFalse((cache / 'source-freshness.json').exists())

    def test_admission_is_exclusive_and_keeps_the_lock_inode(self):
        with patch.object(tools, 'cache_root', return_value=self.root):
            with tools.admission():
                identity = (self.root / 'build-admission.lock').stat().st_ino
                script = 'import sys; sys.path.insert(0, "scripts"); import build_tools; from pathlib import Path; build_tools.cache_root=lambda:Path(sys.argv[1]);\nwith build_tools.admission(): pass'
                result = subprocess.run([sys.executable, '-c', script, str(self.root)], cwd=tools.ROOT, capture_output=True)
                self.assertNotEqual(result.returncode, 0)
            with tools.admission():
                self.assertEqual((self.root / 'build-admission.lock').stat().st_ino, identity)

    def test_incremental_and_checkout_paths_are_not_normalized_away(self):
        import tomllib
        config = tomllib.loads((tools.ROOT / '.cargo/config.toml').read_text())
        self.assertNotIn('SCCACHE_BASEDIRS', config['env'])
        self.assertNotIn('CARGO_INCREMENTAL', config['env'])
        self.assertNotIn('CC_x86_64_pc_windows_msvc', config['env'])
        self.assertNotIn('CXX_x86_64_pc_windows_msvc', config['env'])

    def test_linux_cpu_affinity_selects_at_most_two_permitted_cpus(self):
        import re
        helper = (tools.ROOT / 'scripts/build-linux-rust-release.sh').read_text()
        line = next(line for line in helper.splitlines() if line.strip().startswith('CPU_SET='))
        code = re.search(r'python3 -c "([^"]+)"', line).group(1)
        for available, expected in [({4, 8, 12, 16}, '4,8\n'), ({7}, '7\n')]:
            output = io.StringIO()
            with patch.object(os, 'sched_getaffinity', return_value=available, create=True), redirect_stdout(output):
                exec(compile(code, 'release-cpu-affinity', 'exec'), {})
            self.assertEqual(output.getvalue(), expected)
        self.assertIn('taskset --cpu-list "$CPU_SET" bash', helper)
        self.assertEqual(helper.count('docker run --rm --init --pull never'), 2)

    @unittest.skipIf(os.name == 'nt', 'Unix compiler-server process groups')
    def test_deadline_reaps_build_and_compiler_server_children(self):
        runner = self.runner_module()
        # Both parents have a descendant that ignores SIGTERM. Killing only the
        # two parent PIDs would leave those processes alive.
        sleeper = 'import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)'
        body = f'''import os, socket, subprocess, sys, time
from pathlib import Path
child = subprocess.Popen([sys.executable, '-c', {sleeper!r}])
name = 'server' if os.environ.get('SCCACHE_START_SERVER') else 'build'
Path(os.environ['PROBE_ROOT'], name + '.pid').write_text(str(child.pid))
if name == 'server':
    sock = socket.socket(socket.AF_UNIX)
    sock.bind(os.environ['SCCACHE_SERVER_UDS'])
    sock.listen()
time.sleep(60)
'''
        server = self.root / 'sccache'
        server.write_text(f'#!{sys.executable}\n' + body)
        server.chmod(0o755)
        build = self.root / 'build.py'
        build.write_text(body)
        env = dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ['PATH'], PROBE_ROOT=str(self.root))
        with patch.object(runner, 'cache_root', return_value=self.root):
            with self.assertRaisesRegex(TimeoutError, 'deadline'):
                runner.run([sys.executable, str(build)], env, 1)
        for name in ('server', 'build'):
            pid = int((self.root / (name + '.pid')).read_text())
            for _ in range(30):
                state = subprocess.run(['ps', '-p', str(pid), '-o', 'stat='], capture_output=True, text=True).stdout.strip()
                if not state or state.startswith('Z'):
                    break
                time.sleep(0.1)
            else:
                self.fail(f'{name} descendant survived the deadline')
        self.assertFalse(list(self.root.glob('run-*.sock')))

    @unittest.skipIf(os.name == 'nt', 'Unix zombie leader identity')
    def test_completed_group_is_cleaned_before_reaping_its_leader(self):
        runner = self.runner_module()
        child = subprocess.Popen([sys.executable, '-c', 'pass'], start_new_session=True)
        for _ in range(100):
            if runner.exit_status(child) is not None:
                break
            time.sleep(0.01)
        else:
            child.kill()
            child.wait()
            self.fail('Fixture did not exit')
        self.assertEqual(runner.exit_status(child), 0)
        # In particular, this must tolerate Darwin's EPERM for zombie-only
        # groups while preserving ordinary permission failures for live ones.
        runner.terminate(child)
        self.assertEqual(child.returncode, 0)

    @unittest.skipIf(os.name == 'nt', 'Unix supervisor death and inherited flock')
    def test_supervisor_sigkill_keeps_admission_until_owned_work_exits(self):
        body = '''import os, socket, time
from pathlib import Path
name = 'server' if os.environ.get('SCCACHE_START_SERVER') else 'build'
Path(os.environ['PROBE_ROOT'], name + '.pid').write_text(str(os.getpid()))
if name == 'server':
    sock = socket.socket(socket.AF_UNIX)
    sock.bind(os.environ['SCCACHE_SERVER_UDS'])
    sock.listen()
time.sleep(60)
'''
        server = self.root / 'sccache'
        server.write_text(f'#!{sys.executable}\n' + body)
        server.chmod(0o755)
        build = self.root / 'build.py'
        build.write_text(body)
        script = '''import importlib.util, os, sys
from pathlib import Path
sys.path.insert(0, 'scripts')
import build_tools
root = Path(os.environ['PROBE_ROOT'])
build_tools.cache_root = lambda: root
spec = importlib.util.spec_from_file_location('runner', 'scripts/rust-dev.py')
runner = importlib.util.module_from_spec(spec)
spec.loader.exec_module(runner)
with build_tools.admission() as lock:
    runner.run([sys.executable, str(root / 'build.py')], os.environ, 30, lock)
'''
        env = dict(os.environ, PATH=str(self.root) + os.pathsep + os.environ['PATH'], PROBE_ROOT=str(self.root))
        supervisor = subprocess.Popen([sys.executable, '-c', script], env=env, cwd=tools.ROOT,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
        try:
            for _ in range(100):
                if (self.root / 'build.pid').exists():
                    break
                if supervisor.poll() is not None:
                    self.fail(supervisor.stderr.read().decode())
                time.sleep(0.05)
            else:
                self.fail('Owned work did not start')
            supervisor.kill()
            supervisor.wait(timeout=5)
            with patch.object(tools, 'cache_root', return_value=self.root):
                with self.assertRaises(BlockingIOError):
                    with tools.admission():
                        self.fail('A second build was admitted after supervisor death')
        finally:
            if supervisor.poll() is None:
                supervisor.kill()
                supervisor.wait(timeout=5)
            supervisor.stderr.close()
            for name in ('build', 'server'):
                marker = self.root / (name + '.pid')
                if marker.exists():
                    try:
                        os.killpg(int(marker.read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
        for _ in range(100):
            try:
                with patch.object(tools, 'cache_root', return_value=self.root), tools.admission():
                    return
            except BlockingIOError:
                time.sleep(0.05)
        self.fail('Admission was not released after reaping owned work')

    @unittest.skipIf(os.name == 'nt', 'Unix release prefix maps')
    def test_release_prefix_maps_preserve_paths_with_spaces(self):
        import shlex
        fake = self.root / 'bin'
        fake.mkdir()
        capture = self.root / 'flags.json'
        cargo = fake / 'cargo'
        cargo.write_text(f'#!{sys.executable}\nimport json,os,sys\nfrom pathlib import Path\n'
                         'Path(os.environ["FLAGS_CAPTURE"]).write_text(json.dumps({key: os.environ[key] for key in ("CFLAGS", "CC_SHELL_ESCAPED_FLAGS")}))\nsys.exit(42)\n')
        cargo.chmod(0o755)
        target = self.root / 'Rust Cache' / "developer's target"
        target.mkdir(parents=True)
        env = dict(os.environ, PATH=str(fake) + os.pathsep + os.environ['PATH'], FLAGS_CAPTURE=str(capture),
                   CARGO_TARGET_DIR=str(target), CFLAGS='', CXXFLAGS='')
        result = subprocess.run(['bash', 'scripts/build-rust-release.sh', 'aarch64-unknown-linux-gnu', 'linux-arm64'],
                                cwd=tools.ROOT, env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 42, result.stderr)
        flags = json.loads(capture.read_text())
        expected = '-ffile-prefix-map=' + str(target) + '=.'
        self.assertIn(expected, shlex.split(flags['CFLAGS']))
        self.assertEqual(flags['CC_SHELL_ESCAPED_FLAGS'], '1')
        source = target / 'fixture.cpp'
        source.write_text('const char *path = __FILE__;\n')
        output = subprocess.check_output(['c++', *shlex.split(flags['CFLAGS']), '-E', '-P', str(source)], text=True)
        self.assertNotIn(str(target), output)
        self.assertRegex(output, r'"(?:\./)?fixture\.cpp"')


if __name__ == '__main__':
    unittest.main()
