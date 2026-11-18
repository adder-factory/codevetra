#!/usr/bin/env python3
"""Exercise real Rust/C++ cache hits, changed-input misses and path identities."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from build_tools import admission, cache_root, environment


def compile_probe():
    root = Path.cwd()
    env = dict(os.environ, CARGO_MANIFEST_DIR=str(root / 'checkout-a'))
    rust = root / 'fixture.rs'
    rust.write_text('pub fn path() -> &' + "'static str { env!(\"CARGO_MANIFEST_DIR\") }\n")
    cpp = root / 'fixture.cpp'
    cpp.write_text('int value() { return 41; }\n')
    subprocess.run(['sccache', '--zero-stats'], check=True, stdout=subprocess.DEVNULL)
    rust_args = ['sccache', 'rustc', '--crate-name', 'cache_fixture', '--crate-type', 'rlib',
                 '--emit=link,dep-info', '--out-dir', str(root), str(rust)]
    for _ in range(2):
        (root / 'libcache_fixture.rlib').unlink(missing_ok=True)
        subprocess.run(rust_args, env=env, check=True)
    main = root / 'main.rs'
    main.write_text('fn main() { print!("{}", cache_fixture::path()); }')
    def check_path(expected):
        binary = root / ('probe.exe' if os.name == 'nt' else 'probe')
        subprocess.run(['rustc', '--crate-name', 'probe', str(main), '--extern',
                        'cache_fixture=' + str(root / 'libcache_fixture.rlib'), '-o', str(binary)], check=True)
        actual = subprocess.check_output([str(binary)], text=True)
        if actual != expected:
            raise RuntimeError('Compiler cache substituted a different checkout identity')
    check_path(env['CARGO_MANIFEST_DIR'])
    env['CARGO_MANIFEST_DIR'] = str(root / 'checkout-b')
    subprocess.run(rust_args, env=env, check=True)
    check_path(env['CARGO_MANIFEST_DIR'])
    if os.name == 'nt':
        cpp_args = ['sccache', 'cl.exe', '/nologo', '/c', str(cpp), '/Fo' + str(root / 'fixture.obj')]
    else:
        cpp_args = ['sccache', 'c++', '-c', str(cpp), '-o', str(root / 'fixture.o')]
    for _ in range(2):
        subprocess.run(cpp_args, check=True)
    cpp.write_text('int value() { return 42; }\n')
    subprocess.run(cpp_args, check=True)
    stats = json.loads(subprocess.check_output(['sccache', '--show-stats', '--stats-format', 'json'], text=True))['stats']
    hits = sum(stats['cache_hits']['counts'].values())
    misses = sum(stats['cache_misses']['counts'].values())
    if hits < 2 or misses < 4:
        raise RuntimeError(f'Expected Rust and C++ hits plus input misses; got {hits} hits and {misses} misses')
    print(json.dumps({'hits': hits, 'misses': misses, 'checkout_identity_preserved': True}))


def main():
    if sys.argv[1:] == ['--probe']:
        compile_probe()
        return
    # Load the runner without making an invalid Python module name importable.
    import importlib.util
    spec = importlib.util.spec_from_file_location('rust_dev', Path(__file__).with_name('rust-dev.py'))
    runner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runner)
    env = environment()
    with admission() as lock, tempfile.TemporaryDirectory(prefix='cache-probe-', dir=cache_root()) as directory:
        root = Path(directory)
        env['SCCACHE_DIR'] = str(root / 'cache')
        script = Path(__file__).resolve()
        # run() normally binds Cargo to the checkout. This probe owns its fixture.
        runner.ROOT = root
        result = runner.BuildRun([sys.executable, str(script), '--probe'], env, 180).run(lock)
        if result:
            raise SystemExit(result)


if __name__ == '__main__':
    main()
