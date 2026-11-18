#!/usr/bin/env python3
"""Replay the real CLI link with Linux linkers, then retain the LLD artifact.

Used only by CODEVETRA_BENCHMARK_LINKERS=1 in the bounded Linux build helper.
Rustc owns the input files for this invocation, so every trial sees identical
objects. Failed alternatives are recorded and never become release artifacts.
"""

import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess
import sys
import time


TRIAL_COUNT = 3
LINK_TIMEOUT_SECONDS = 180
SMOKE_TIMEOUT_SECONDS = 120
EXECUTABLE_MODE = 0o755


def main():
    if sys.platform != 'linux' or os.environ.get('CODEVETRA_BENCHMARK_LINKERS') != '1':
        raise SystemExit('Linker comparison requires the explicit bounded Linux build mode')
    args = sys.argv[1:]
    output_index = args.index('-o') + 1
    output = Path(args[output_index])
    directory = output.parent / 'linker-comparison'
    directory.mkdir(exist_ok=True)
    results = {}
    # Pass the explicit linker to Clang instead of relying on PATH aliases.
    for name, executable in [('bfd', 'ld.bfd'), ('lld', 'ld.lld'), ('mold', 'mold'), ('wild', 'wild')]:
        linker = shutil.which(executable)
        if linker is None:
            raise RuntimeError(f'Missing benchmark linker: {executable}')
        times = []
        binary = directory / name
        candidate_args = [arg for arg in args if not arg.startswith('-fuse-ld=')]
        candidate_args[candidate_args.index('-o') + 1] = str(binary)
        for _ in range(TRIAL_COUNT):
            started = time.monotonic()
            completed = subprocess.run(['clang', '--ld-path=' + linker, *candidate_args], timeout=LINK_TIMEOUT_SECONDS)
            elapsed = time.monotonic() - started
            if completed.returncode:
                break
            subprocess.run([str(Path(__file__).with_name('smoke-rust-release.sh')), str(binary)], check=True, timeout=SMOKE_TIMEOUT_SECONDS)
            times.append(elapsed)
        results[name] = {'successful_trials': len(times), 'link_seconds': times,
                         'median_seconds': statistics.median(times) if times else None}
        if name == 'lld' and len(times) != TRIAL_COUNT:
            raise RuntimeError('The selected LLD linker failed its comparison')
    shutil.copyfile(directory / 'lld', output)
    output.chmod(EXECUTABLE_MODE)
    report = Path(os.environ['CODEVETRA_LINKER_REPORT'])
    report.write_text(json.dumps(results, indent=2) + '\n')
    print('[linker-comparison] ' + json.dumps(results), flush=True)


if __name__ == '__main__':
    main()
