#!/usr/bin/env python3
"""Run the real installer with isolated network and filesystem command doubles."""

import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


ROOT = Path(__file__).resolve().parent.parent
COMMANDS = ("uname", "ldd", "mktemp", "curl", "mkdir", "rm", "tar", "ln")
DOUBLE = r'''
import hashlib, json, os, pathlib, sys
command = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ['INSTALL_TEST_LOG'], 'a') as log:
    log.write(json.dumps([command, *args]) + '\n')
if command == 'uname':
    print('Linux' if args == ['-s'] else 'x86_64')
elif command == 'ldd':
    print('ldd (GNU libc) 2.41')
elif command == 'mktemp':
    print(os.environ['INSTALL_TEST_STAGE'])
elif command == 'curl':
    if '-fsSLI' in args:
        print('https://github.com/resolved/repository/releases/tag/v9.9.9', end='')
    else:
        target = pathlib.Path(args[args.index('-o') + 1])
        assert target.parent == pathlib.Path(os.environ['INSTALL_TEST_STAGE'])
        if target.name == 'SHA256SUMS':
            digest = hashlib.sha256(b'fixture archive bytes').hexdigest()
            if os.environ.get('INSTALL_TEST_BAD_CHECKSUM') == '1':
                digest = '0' * 64
            target.write_text(digest + '  codevetra-linux-x64.tar.gz\n')
        else:
            target.write_bytes(b'fixture archive bytes')
# mkdir/rm/tar/ln only record their arguments, including deliberate empty-path
# cases. No command can write an installation, change PATH or access a network.
'''


def run_case(root, name, variables, selected, verifies, corrupt=False, rejected=False):
    case = root / name
    case.mkdir()
    stage = case / "download"
    stage.mkdir()
    log = case / "commands.jsonl"
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith(("CODEVETRA_", "CARTOGRAPH_", "INSTALL_TEST_"))
    }
    environment.update(variables)
    environment.update(
        PATH=str(root / "commands") + os.pathsep + os.environ["PATH"],
        INSTALL_TEST_STAGE=str(stage),
        INSTALL_TEST_LOG=str(log),
        INSTALL_TEST_BAD_CHECKSUM="1" if corrupt else "0",
    )
    result = subprocess.run(
        ["sh", str(ROOT / "install.sh")],
        env=environment,
        capture_output=True,
        text=True,
        timeout=20,
        check=False,
    )
    calls = [json.loads(line) for line in log.read_text().splitlines()] if log.exists() else []
    if rejected:
        assert result.returncode != 0 and "must not be empty" in result.stderr, (name, result)
        assert not calls, (name, calls)
        return
    urls = [arg for call in calls if call[0] == "curl" for arg in call if arg.startswith("https:")]
    repository, version, install_dir, bin_dir = selected
    assert f"https://github.com/{repository}/releases/download/{version}/codevetra-linux-x64.tar.gz" in urls, (name, urls)
    assert any(url.endswith("/SHA256SUMS") for url in urls) == verifies, (name, urls)
    if corrupt and verifies:
        assert result.returncode != 0 and "CHECKSUM MISMATCH" in result.stderr, (name, result)
        assert not any(call[0] in ("mkdir", "tar", "ln") for call in calls), (name, calls)
    else:
        assert result.returncode == 0, (name, result.stderr)
        assert ["mkdir", "-p", f"{install_dir}/versions/{version}"] in calls, (name, calls)
        assert ["ln", "-sf", f"{install_dir}/current/bin/codevetra", f"{bin_dir}/codevetra"] in calls, (name, calls)


with tempfile.TemporaryDirectory(prefix="codevetra-installer-contract-") as directory:
    root = Path(directory)
    commands = root / "commands"
    commands.mkdir()
    for command in COMMANDS:
        script = commands / command
        script.write_text(f"#!{sys.executable}\n" + DOUBLE)
        script.chmod(0o755)
    legacy = dict(
        CARTOGRAPH_REPO="legacy/repository",
        CARTOGRAPH_VERSION="1.2.3",
        CARTOGRAPH_INSTALL_DIR=str(root / "legacy-install"),
        CARTOGRAPH_BIN_DIR=str(root / "legacy-bin"),
        CARTOGRAPH_SKIP_CHECKSUM="1",
    )
    canonical = dict(
        CODEVETRA_REPO="canonical/repository",
        CODEVETRA_VERSION="4.0.0",
        CODEVETRA_INSTALL_DIR=str(root / "canonical-install"),
        CODEVETRA_BIN_DIR=str(root / "canonical-bin"),
        CODEVETRA_SKIP_CHECKSUM="0",
    )
    legacy_selected = ("legacy/repository", "v1.2.3", legacy["CARTOGRAPH_INSTALL_DIR"], legacy["CARTOGRAPH_BIN_DIR"])
    canonical_selected = ("canonical/repository", "v4.0.0", canonical["CODEVETRA_INSTALL_DIR"], canonical["CODEVETRA_BIN_DIR"])
    run_case(root, "legacy-fallback", legacy, legacy_selected, False, corrupt=True)
    run_case(root, "canonical-precedence", legacy | canonical, canonical_selected, True)
    for suffix in ("REPO", "INSTALL_DIR", "BIN_DIR"):
        run_case(root, f"empty-canonical-{suffix}", legacy | canonical | {"CODEVETRA_" + suffix: ""}, canonical_selected, True, rejected=True)
    run_case(root, "empty-canonical-version", legacy | canonical | {"CODEVETRA_VERSION": ""}, ("canonical/repository", "v9.9.9", canonical_selected[2], canonical_selected[3]), True)
    run_case(root, "legacy-checksum-required", legacy | {"CARTOGRAPH_SKIP_CHECKSUM": "0"}, legacy_selected, True, corrupt=True)
    run_case(root, "canonical-checksum-required", legacy | canonical, canonical_selected, True, corrupt=True)
    run_case(root, "empty-canonical-checksum-required", legacy | canonical | {"CODEVETRA_SKIP_CHECKSUM": ""}, canonical_selected, True, corrupt=True)

print("Unix installer fallback, precedence, empty values and checksum refusals passed (9 cases).")
