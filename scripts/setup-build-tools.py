#!/usr/bin/env python3
"""Install checksum-pinned build tools without compiling or using system temp."""

import argparse
import os
from pathlib import Path

from build_tools import admission, configure_volume, environment, install, link_local_cache


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, default=Path.home() / '.cargo/bin')
    parser.add_argument('--tools', nargs='+', default=['sccache', 'cargo-nextest'],
                        choices=['sccache', 'cargo-nextest', 'mold', 'wild'])
    parser.add_argument('--github-env', action='store_true')
    parser.add_argument('--require-volume', type=Path)
    parser.add_argument('--cache-root', type=Path)
    args = parser.parse_args()
    if bool(args.require_volume) != bool(args.cache_root):
        parser.error('--require-volume and --cache-root must be supplied together')
    if args.require_volume:
        configure_volume(args.require_volume, args.cache_root)
    with admission():
        install(args.tools, args.bin_dir)
        link_local_cache()
    if args.github_env:
        env = environment()
        with Path(os.environ['GITHUB_PATH']).open('a') as output:
            output.write(str(args.bin_dir.resolve()) + '\n')
        with Path(os.environ['GITHUB_ENV']).open('a') as output:
            for name in ('SCCACHE_DIR', 'SCCACHE_CACHE_SIZE', 'CARGO_BUILD_JOBS', 'RUST_TEST_THREADS',
                         'CODEVETRA_BUILD_CACHE_ROOT', 'TMPDIR', 'TMP', 'TEMP'):
                output.write(name + '=' + env[name] + '\n')
            output.write('CARGO_INCREMENTAL=0\n')


if __name__ == '__main__':
    main()
