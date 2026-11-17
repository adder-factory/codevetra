"""Pinned development tools and checkout-local cache placement policy."""

import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.request
from contextlib import contextmanager

ROOT = Path(__file__).resolve().parent.parent
PINS = ROOT / 'scripts/build-tools.json'
POLICY = ROOT / '.codevetra/build-tools-local.json'
MAX_DOWNLOAD = 256 * 1024 * 1024


def cache_path(path, policy):
    resolved = Path(path).expanduser().resolve()
    if 'requiredVolume' in policy:
        volume = Path(policy['requiredVolume']).resolve()
        if not resolved.is_relative_to(volume):
            raise ValueError('Cache path escapes the required build-cache volume')
        ancestor = resolved
        while not ancestor.exists():
            ancestor = ancestor.parent
        if ancestor.stat().st_dev != volume.stat().st_dev:
            raise ValueError('Cache path is on a different filesystem')
    return resolved


def platform_key():
    system = platform.system().lower()
    machine = platform.machine().lower()
    arch = {'arm64': 'aarch64', 'amd64': 'x86_64'}.get(machine, machine)
    return f'{system}-{arch}'


def validate_volume(policy):
    """Check before creating directories; an absent volume must never fall back."""
    if 'requiredVolume' not in policy:
        return
    volume = Path(policy['requiredVolume'])
    if not volume.is_mount():
        raise ValueError('Required build-cache volume is not mounted with its recorded identity')
    if platform.system() != 'Darwin':
        raise ValueError('A local macOS volume policy cannot be used on another platform')
    info = plistlib.loads(subprocess.check_output(['diskutil', 'info', '-plist', str(volume)], timeout=10))
    if info.get('VolumeUUID') != policy['volumeUUID'] or info.get('MountPoint') != str(volume):
        raise ValueError('Required build-cache volume identity does not match')
    if volume.stat().st_dev == Path.home().stat().st_dev:
        raise ValueError('Required build-cache volume resolves to the home filesystem')
    for key in ('cacheRoot', 'targetRoot'):
        cache_path(policy[key], policy)
    target = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target')).resolve()
    if target != Path(policy['targetRoot']).resolve():
        raise ValueError('Cargo target directory differs from its recorded cache policy')
    for directory in (Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo')),
                      Path(os.environ.get('RUSTUP_HOME', Path.home() / '.rustup'))):
        if not directory.resolve().is_relative_to(volume.resolve()):
            raise ValueError('Cargo and Rustup must remain on the required cache volume')


def cache_policy():
    if POLICY.exists():
        policy = json.loads(POLICY.read_text())
    else:
        if platform.system() == 'Darwin' and os.environ.get('GITHUB_ACTIONS') != 'true':
            raise ValueError('Local macOS builds require an explicit cache-volume policy; run setup-build-tools.py --require-volume ... --cache-root ...')
        root = Path(os.environ.get('CODEVETRA_BUILD_CACHE_ROOT', ROOT / 'target/build-cache'))
        target = Path(os.environ.get('CARGO_TARGET_DIR', ROOT / 'target'))
        policy = {'cacheRoot': str(root.resolve()), 'targetRoot': str(target.resolve())}
    validate_volume(policy)
    return policy


def configure_volume(volume, root):
    """Record an existing external-cache layout; never relocate live caches."""
    volume = volume.expanduser().resolve()
    if platform.system() != 'Darwin' or not volume.is_mount():
        raise ValueError('Volume pinning requires an existing mounted macOS volume')
    info = plistlib.loads(subprocess.check_output(['diskutil', 'info', '-plist', str(volume)], timeout=10))
    policy = {'requiredVolume': str(volume), 'volumeUUID': info['VolumeUUID'],
              'cacheRoot': str(root.expanduser().resolve()),
              'targetRoot': str((ROOT / 'target').resolve())}
    validate_volume(policy)
    POLICY.parent.mkdir(parents=True, exist_ok=True)
    pending = POLICY.with_suffix('.pending')
    pending.write_text(json.dumps(policy, indent=2) + '\n')
    os.replace(pending, POLICY)


def cache_root():
    return Path(cache_policy()['cacheRoot'])


def linux_cache(path):
    policy = cache_policy()
    root = cache_path(path, policy)
    children = [root / name for name in ('cargo', 'rustup', 'target', 'sccache', 'tmp', 'home', 'build-tools')]
    for child in [*children, root / 'source-freshness.json']:
        resolved = cache_path(child, policy)
        if not resolved.is_relative_to(root):
            raise ValueError('Linux cache child escapes its Docker bind mount')
    for child in children:
        child.mkdir(parents=True, exist_ok=True)
    return root


@contextmanager
def admission():
    root = cache_root()
    root.mkdir(parents=True, exist_ok=True)
    # Keep this inode stable: another checkout may already hold the lock.
    with (root / 'build-admission.lock').open('a+b') as lock:
        lock.seek(0)
        if os.name == 'nt':
            import msvcrt
            if lock.read(1) == b'':
                lock.write(b'0')
                lock.flush()
            lock.seek(0)
            msvcrt.locking(lock.fileno(), msvcrt.LK_NBLCK, 1)
        else:
            import fcntl
            fcntl.flock(lock.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
        try:
            yield lock
        finally:
            if os.name == 'nt':
                lock.seek(0)
                msvcrt.locking(lock.fileno(), msvcrt.LK_UNLCK, 1)
            else:
                fcntl.flock(lock.fileno(), fcntl.LOCK_UN)


def sha256(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def download(url, destination, expected):
    if not url.startswith('https://github.com/') or '/releases/download/' not in url:
        raise ValueError('Build tools require an exact HTTPS GitHub release asset')
    total = 0
    with urllib.request.urlopen(url, timeout=60) as response, destination.open('wb') as target:
        while block := response.read(1024 * 1024):
            total += len(block)
            if total > MAX_DOWNLOAD:
                raise ValueError('Build tool download exceeded the byte limit')
            target.write(block)
    if sha256(destination) != expected:
        raise ValueError('Build tool archive checksum mismatch')


def extract_binary(archive, name, destination):
    """Copy one regular file; never extract archive paths or links."""
    with tarfile.open(archive, 'r:gz') as source:
        members = []
        for member in source:
            if len(members) >= 512:
                raise ValueError('Build tool archive has too many members')
            members.append(member)
        # mold also ships a lib/mold directory; only non-directory entries can
        # be executable candidates. Links remain candidates and are rejected.
        matches = [member for member in members if Path(member.name).name == name and not member.isdir()]
        if len(matches) != 1 or not matches[0].isfile() or matches[0].size > MAX_DOWNLOAD:
            raise ValueError('Build tool archive must contain one bounded regular executable')
        with source.extractfile(matches[0]) as executable, destination.open('wb') as target:
            shutil.copyfileobj(executable, target, 1024 * 1024)
    destination.chmod(0o755)


def install(names, bin_dir):
    pins = json.loads(PINS.read_text())['tools']
    root = cache_root()
    # Tool downloads and temporary extraction stay beside the approved cache.
    policy = cache_policy()
    temporary = cache_path(root / 'bootstrap', policy)
    temporary.mkdir(parents=True, exist_ok=True)
    bin_dir = cache_path(bin_dir, policy)
    bin_dir.mkdir(parents=True, exist_ok=True)
    for name in names:
        pin = pins[name]
        if platform_key() not in pin['platforms']:
            raise ValueError(f'{name} has no reviewed binary for {platform_key()}')
        asset = pin['platforms'][platform_key()]
        executable = name + ('.exe' if os.name == 'nt' else '')
        destination = bin_dir / executable
        receipt = bin_dir / f'.{executable}.codevetra.json'
        if receipt.exists() and destination.is_file():
            previous = json.loads(receipt.read_text())
            if previous.get('archiveSha256') == asset['sha256'] and previous.get('binarySha256') == sha256(destination):
                print(f'{name} {pin["version"]}: verified installed binary', flush=True)
                continue
        with tempfile.TemporaryDirectory(dir=temporary) as directory:
            archive = Path(directory) / 'tool.tar.gz'
            candidate = Path(directory) / executable
            download(asset['url'], archive, asset['sha256'])
            extract_binary(archive, executable, candidate)
            # Version-check before replacing an existing tool. Paths are argv, never shell text.
            result = subprocess.check_output([str(candidate), '--version'], text=True, timeout=30)
            if pin['version'] not in result.split():
                raise ValueError(f'{name} executable version does not match its pin')
            with tempfile.NamedTemporaryFile(dir=bin_dir, prefix=f'.{executable}.', delete=False) as staged:
                pending = Path(staged.name)
                with candidate.open('rb') as source:
                    shutil.copyfileobj(source, staged)
            try:
                pending.chmod(0o755)
                os.replace(pending, destination)
            finally:
                pending.unlink(missing_ok=True)
            with tempfile.NamedTemporaryFile(mode='w', dir=bin_dir, prefix='.receipt.', delete=False) as staged:
                pending = Path(staged.name)
                staged.write(json.dumps({'archiveSha256': asset['sha256'], 'binarySha256': sha256(destination)}) + '\n')
            try:
                os.replace(pending, receipt)
            finally:
                pending.unlink(missing_ok=True)
            print(f'{name} {pin["version"]}: installed verified binary', flush=True)


def environment():
    policy = cache_policy()
    root = Path(policy['cacheRoot'])
    directories = {name: root / name for name in ('sccache', 'tmp')}
    # Git fixtures must not discover the checkout above their temporary root.
    # Hosted runners provide an isolated temp root outside the repository;
    # a recorded local volume policy always takes precedence.
    if os.environ.get('GITHUB_ACTIONS') == 'true' and 'requiredVolume' not in policy:
        directories['tmp'] = Path(os.environ['RUNNER_TEMP']) / 'codevetra-build-tmp'
    for directory in directories.values():
        cache_path(directory, policy)
        directory.mkdir(parents=True, exist_ok=True)
    result = dict(os.environ)
    result.update(CARGO_BUILD_JOBS='2', RUST_TEST_THREADS='2',
                  CARGO_TARGET_DIR=policy['targetRoot'],
                  CARGO_BUILD_BUILD_DIR=policy['targetRoot'],
                  SCCACHE_DIR=str(directories['sccache']),
                  SCCACHE_CACHE_SIZE='2G' if os.environ.get('GITHUB_ACTIONS') == 'true' else '8G',
                  TMPDIR=str(directories['tmp']), TMP=str(directories['tmp']), TEMP=str(directories['tmp']),
                  CODEVETRA_BUILD_CACHE_ROOT=str(root))
    return result


def link_local_cache():
    """Keep plain Cargo's repository-relative default on the configured volume."""
    if not POLICY.exists():
        return
    policy = cache_policy()
    target = ROOT / 'target'
    if target.resolve() != Path(policy['targetRoot']).resolve():
        raise ValueError('Checkout target directory differs from its recorded cache policy')
    destination = cache_path(cache_root() / 'sccache', policy)
    destination.mkdir(parents=True, exist_ok=True)
    link = target / 'sccache'
    if link.exists() or link.is_symlink():
        if link.resolve() != destination.resolve():
            raise ValueError('Existing compiler cache differs from the configured cache; refusing to replace it')
    else:
        link.symlink_to(destination, target_is_directory=True)


def source_freshness(paths, digest, build_cache):
    """Reuse source timestamps only for the same staged bytes and permissions.

    The caller holds build admission until Cargo exits. Check every cached
    artifact's timestamp so clock skew and filesystem rounding fail closed.
    """
    policy = cache_policy()
    root = cache_path(build_cache, policy)
    receipt = cache_path(root / 'source-freshness.json', policy)
    target = cache_path(root / 'target', policy)
    if not receipt.is_relative_to(root) or not target.is_relative_to(root):
        raise ValueError('Source freshness state escapes its build cache')
    previous = None
    if receipt.exists():
        with receipt.open() as source:
            data = source.read(4097)
        if len(data) > 4096:
            raise ValueError('Source freshness receipt exceeds its byte limit')
        previous = json.loads(data)
        if (not isinstance(previous, dict) or previous.get('version') != 1
                or not isinstance(previous.get('sourceDigest'), str)
                or len(previous['sourceDigest']) != 64
                or any(c not in '0123456789abcdef' for c in previous['sourceDigest'])
                or type(previous.get('mtimeNs')) is not int or not 0 < previous['mtimeNs'] < 2**63):
            raise ValueError('Invalid source freshness receipt')
    unchanged = previous is not None and previous['sourceDigest'] == digest
    anchor = 0
    if not unchanged:
        entries = 0
        for directory, directories, files in os.walk(target):
            entries += len(directories) + len(files)
            if entries > 100_000:
                raise ValueError('Build cache exceeds the source freshness metadata bound')
            if any((Path(directory) / name).is_symlink() for name in [*directories, *files]):
                raise ValueError('Build cache freshness anchors must not be symlinks')
            for name in files:
                path = Path(directory) / name
                anchor = max(anchor, path.stat().st_mtime_ns)
    deadline = time.monotonic() + 2
    while True:
        stamp = previous['mtimeNs'] if unchanged else time.time_ns()
        if not unchanged and anchor > stamp + 2_000_000_000:
            raise ValueError('Build cache has future timestamps; source freshness is unproved')
        for path in paths:
            os.utime(path, ns=(stamp, stamp))
        actual = {path.stat().st_mtime_ns for path in paths}
        if len(actual) == 1 and (actual == {stamp} if unchanged else min(actual) > anchor):
            stamp = actual.pop()
            break
        if unchanged or time.monotonic() >= deadline:
            raise ValueError('Filesystem timestamps cannot prove source freshness')
        time.sleep(0.05)
    root.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(mode='w', dir=root, prefix='.source-freshness.', delete=False) as output:
        pending = Path(output.name)
        json.dump({'version': 1, 'sourceDigest': digest, 'mtimeNs': stamp}, output)
        output.flush()
        os.fsync(output.fileno())
    try:
        os.replace(pending, receipt)
    finally:
        pending.unlink(missing_ok=True)


def snapshot(destination=None, *, build_cache=None):
    """Stage public source without exposing credentials, graph state or caches."""
    names = subprocess.check_output(['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z'], cwd=ROOT)
    digest = hashlib.sha256()
    staged = []
    for name in sorted(set(filter(None, names.split(b'\0')))):
        relative = Path(os.fsdecode(name))
        source = ROOT / relative
        if relative.is_absolute() or '..' in relative.parts or source.is_symlink() or not source.resolve().is_relative_to(ROOT):
            raise ValueError('Build input escaped the checkout')
        if not source.is_file():
            raise ValueError('Build input disappeared while staging source')
        content = source.read_bytes()
        executable = bool(source.stat().st_mode & 0o111)
        digest.update(name + b'\0' + hashlib.sha256(content).digest())
        digest.update(bytes([executable]))
        if destination is not None:
            target = destination / relative
            target.parent.mkdir(parents=True, exist_ok=True, mode=0o755)
            target.write_bytes(content)
            target.chmod(0o755 if executable else 0o644)
            staged.append(target)
    if build_cache is not None:
        if not staged:
            raise ValueError('Source freshness requires a nonempty staged snapshot')
        source_freshness(staged, digest.hexdigest(), build_cache)
    return digest.hexdigest()
