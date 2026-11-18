#!/usr/bin/env python3
"""Prove the constrained release's Rust target and native compiler cache policy."""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shlex
import shutil
import subprocess
import sys
import tempfile
import tomllib

from build_tools import admission, cache_root, environment


SOURCE_FILE_COUNT = 1024
PROBE_TIMEOUT_SECONDS = 120
NATIVE_RUN_TIMEOUT_SECONDS = 30


def run(arguments, **kwargs):
    return subprocess.run(arguments, check=True, timeout=PROBE_TIMEOUT_SECONDS, **kwargs)


def verify(root, probe_environment, config):
    target = probe_environment['CODEVETRA_RELEASE_TARGET']
    source = root / 'src'
    source.mkdir()
    (root / '.cargo').mkdir()
    shutil.copyfile(config, root / '.cargo/config.toml')
    (root / 'Cargo.toml').write_text(
        '[workspace]\n[package]\nname="release_cache_probe"\nversion="0.0.0"\nedition="2024"\n')
    # Many source files exercise the input shape that triggered Rust hash
    # fan-out; a restored Rust wrapper also fails the statistics check below.
    for index in range(SOURCE_FILE_COUNT):
        (source / f'part{index}.rs').write_text(f'pub const VALUE: usize = {index};\n')
    main = source / 'main.rs'
    main.write_text(''.join(f'mod part{i};\n' for i in range(SOURCE_FILE_COUNT)) +
                    'fn main() { println!("{}", [' +
                    ','.join(f'part{i}::VALUE' for i in range(SOURCE_FILE_COUNT)) + '].iter().sum::<usize>()); }\n')
    env = dict(probe_environment, CARGO_TARGET_DIR=str(root / 'target'),
               CARGO_BUILD_BUILD_DIR=str(root / 'target'))
    cargo = ['cargo', '+' + env['CODEVETRA_RELEASE_TOOLCHAIN']]
    run([*cargo, 'generate-lockfile', '--offline'], cwd=root, env=env)
    build = [*cargo, 'build', '--offline', '--locked', '--quiet']
    run(build, cwd=root, env=env)
    binary = root / 'target/debug/release_cache_probe'
    def output(arguments):
        return subprocess.check_output(arguments, env=env, text=True, timeout=PROBE_TIMEOUT_SECONDS).strip()
    if output([str(binary)]) != '523776':
        raise RuntimeError('Rust fixture produced an unexpected result')
    timestamp = binary.stat().st_mtime_ns
    run(build, cwd=root, env=env)
    if binary.stat().st_mtime_ns != timestamp:
        raise RuntimeError('Unchanged Rust fixture did not reuse Cargo artifacts')
    (source / 'part0.rs').write_text('pub const VALUE: usize = 100000;\n')
    run(build, cwd=root, env=env)
    if output([str(binary)]) != '623776':
        raise RuntimeError('Changed Rust input reused a stale artifact')
    compiler = shlex.split(tomllib.loads(config.read_text())['env']['CXX_' + target.replace('-', '_')])
    cpp = root / 'native.cpp'
    obj = root / 'native.o'
    native = root / 'native'
    cpp.write_text('int main() { return 41; }\n')
    for _ in range(2):
        obj.unlink(missing_ok=True)
        run([*compiler, '-c', str(cpp), '-o', str(obj)], env=env)
    cpp.write_text('int main() { return 42; }\n')
    run([*compiler, '-c', str(cpp), '-o', str(obj)], env=env)
    run(['c++', str(obj), '-o', str(native)], env=env)
    if subprocess.run([str(native)], timeout=NATIVE_RUN_TIMEOUT_SECONDS).returncode != 42:
        raise RuntimeError('Changed native input reused a stale object')
    stats = json.loads(output(['sccache', '--show-stats', '--stats-format', 'json']))['stats']
    hits = sum(stats['cache_hits']['counts'].values())
    misses = sum(stats['cache_misses']['counts'].values())
    if stats['compile_requests'] != 3 or hits != 1 or misses != 2:
        raise RuntimeError(f'Expected only three native requests, one hit and two misses: {stats["compile_requests"]}, {hits}, {misses}')
    print(json.dumps({'rust_target_reused': True, 'changed_inputs_verified': True,
                      'native_hits': hits, 'native_misses': misses}), flush=True)


def main():
    if any(os.environ.get(key) != '' for key in ('RUSTC_WRAPPER', 'RUSTC_WORKSPACE_WRAPPER')):
        raise RuntimeError('The bounded Linux release must explicitly clear both Rust wrappers')
    config = Path(__file__).resolve().parent.parent / '.cargo/config.toml'
    if sys.argv[1:] == ['--probe']:
        verify(Path.cwd(), dict(os.environ), config)
        return
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--timeout', type=int, default=PROBE_TIMEOUT_SECONDS)
    args = parser.parse_args()
    if not 1 <= args.timeout <= PROBE_TIMEOUT_SECONDS:
        parser.error('timeout must be between 1 and 120 seconds')
    spec = importlib.util.spec_from_file_location('rust_dev', Path(__file__).with_name('rust-dev.py'))
    runner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runner)
    env = environment()
    with admission() as lock, tempfile.TemporaryDirectory(prefix='native-cache-probe-', dir=cache_root()) as directory:
        root = Path(directory)
        env['SCCACHE_DIR'] = str(root / 'cache')
        runner.ROOT = root
        result = runner.BuildRun([sys.executable, str(Path(__file__).resolve()), '--probe'],
                                 env, args.timeout).run(lock)
        if result:
            raise SystemExit(result)


if __name__ == '__main__':
    main()
