#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ADMITTED=false
if [[ "${1:-}" == --admitted ]]; then
  BUILD_ADMITTED=true
  shift
fi
TARGET="${1:-}"
ASSET_TARGET="${2:-}"
BUILD_IMAGE="${3:-}"
RUNTIME_IMAGE="${4:-}"
BUILD_TOOLCHAIN="$(sed -n 's/^channel = "\([^" ]*\)"$/\1/p' "$ROOT/rust-toolchain.toml")"
if [[ ! "$BUILD_TOOLCHAIN" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "the release compiler must be an exact stable toolchain" >&2
  exit 2
fi

if [[ -z "$TARGET" || -z "$ASSET_TARGET" || -z "$BUILD_IMAGE" || -z "$RUNTIME_IMAGE" ]]; then
  echo "usage: $0 <rust-target-triple> <asset-target> <pinned-build-image> <pinned-runtime-image>" >&2
  exit 2
fi

case "$TARGET:$ASSET_TARGET" in
  x86_64-unknown-linux-gnu:linux-x64) EXPECTED_MACHINE="x86_64" ;;
  aarch64-unknown-linux-gnu:linux-arm64) EXPECTED_MACHINE="aarch64" ;;
  *)
    echo "unsupported Linux release target pairing: $TARGET / $ASSET_TARGET" >&2
    exit 2
    ;;
esac

if [[ ! "$BUILD_IMAGE" =~ ^rust:1\.100\.0-trixie@sha256:[0-9a-f]{64}$ ]]; then
  echo "Linux release build image must pin rust:1.100.0-trixie by digest" >&2
  exit 2
fi
if [[ ! "$RUNTIME_IMAGE" =~ ^debian:13-slim@sha256:[0-9a-f]{64}$ ]]; then
  echo "Linux release runtime image must pin debian:13-slim by digest" >&2
  exit 2
fi
if [[ "$BUILD_ADMITTED" != true ]]; then
  exec python3 "$ROOT/scripts/with-build-cache.py" bash "$0" --admitted "$@"
fi

# Bind every operation to one local endpoint; never send cache paths remotely.
if [[ -n "${DOCKER_CONTEXT:-}" ]]; then
  BUILD_DOCKER_HOST="$(docker context inspect "$DOCKER_CONTEXT" --format '{{.Endpoints.docker.Host}}')"
else
  BUILD_DOCKER_HOST="${DOCKER_HOST:-$(docker context inspect --format '{{.Endpoints.docker.Host}}')}"
fi
[[ "$BUILD_DOCKER_HOST" == unix:///* ]] || { echo 'A local Docker Unix socket is required.' >&2; exit 1; }
unset DOCKER_CONTEXT DOCKER_TLS_VERIFY DOCKER_CERT_PATH
export DOCKER_HOST="$BUILD_DOCKER_HOST"
docker info --format '{{json .}}' | python3 -c '
import json, sys
info = json.load(sys.stdin)
if info.get("OSType") != "linux" or info.get("CgroupVersion") != "2" or not all(info.get(key) for key in ("MemoryLimit", "SwapLimit", "PidsLimit")):
    sys.exit("Linux cgroup v2 with memory, swap and PID enforcement is required")
'

"$ROOT/scripts/pull-pinned-image.sh" "$BUILD_IMAGE"
"$ROOT/scripts/pull-pinned-image.sh" "$RUNTIME_IMAGE"

# Reuse compiler, registry, toolchain and target data on the admitted cache
# filesystem. The local policy refuses a missing/replaced external volume.
CACHE_ROOT="$(python3 "$ROOT/scripts/rust-dev.py" --cache-root)"
IMAGE_DIGEST="${BUILD_IMAGE##*@sha256:}"
CACHE="$(python3 - "$ROOT" "$CACHE_ROOT/linux/$ASSET_TARGET/$BUILD_TOOLCHAIN-${IMAGE_DIGEST:0:16}" <<'PY'
import sys
from pathlib import Path
sys.path.insert(0, str(Path(sys.argv[1]) / 'scripts'))
from build_tools import linux_cache
print(linux_cache(sys.argv[2]))
PY
)"
mkdir -p "$CACHE"
STAGED="$(mktemp -d "$CACHE_ROOT/linux-source.XXXXXXXX")"
CONTAINER_NAME="codevetra-linux-build-$(id -u)"
if docker container inspect "$CONTAINER_NAME" >/dev/null 2>&1; then
  rmdir "$STAGED"
  echo 'A Linux release build or unreaped container already owns this user slot.' >&2
  exit 1
fi
cleanup() {
  if [[ -s "$STAGED/container.id" ]]; then
    container_id="$(cat "$STAGED/container.id")"
    if [[ "$container_id" =~ ^[0-9a-f]{64}$ ]]; then
      if ! docker container rm --force "$container_id" >/dev/null 2>&1; then
        # --rm may have removed a completed container. Confirm daemon reachability
        # and absence before deleting its source mount.
        if ! docker info >/dev/null 2>&1 || docker container inspect "$container_id" >/dev/null 2>&1; then
          echo "Container cleanup unconfirmed; retaining $STAGED" >&2
          exit 1
        fi
      fi
    fi
  fi
  rm -rf "$STAGED"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
mkdir "$STAGED/source"
SOURCE_DIGEST="$(python3 - "$ROOT" "$STAGED/source" "$CACHE" <<'PY'
import sys
from pathlib import Path
sys.path.insert(0, str(Path(sys.argv[1]) / 'scripts'))
from build_tools import snapshot
print(snapshot(Path(sys.argv[2]), build_cache=Path(sys.argv[3])))
PY
)"
# The archive audit queries tracked files. Create a public-only index, without
# copying the developer checkout's Git configuration, hooks or credentials.
git -c init.templateDir= -c core.hooksPath=/dev/null init --quiet "$STAGED/source"
git -C "$STAGED/source" -c core.hooksPath=/dev/null add --all
echo "[rust-release] source snapshot $SOURCE_DIGEST"

docker run --rm --init --pull never \
  --name "$CONTAINER_NAME" --cidfile "$STAGED/container.id" \
  --memory 4g --memory-swap 4g --cpus 2 --pids-limit 256 \
  --volume "$STAGED/source:/workspace" \
  --volume "$CACHE:/cache" \
  --workdir /workspace \
  --env HOME=/cache/home \
  --env CODEVETRA_BUILD_UID="$(id -u)" \
  --env CODEVETRA_BUILD_GID="$(id -g)" \
  --env CARGO_HOME=/cache/cargo \
  --env RUSTUP_HOME=/cache/rustup \
  --env CODEVETRA_RELEASE_TOOLCHAIN="$BUILD_TOOLCHAIN" \
  --env CARGO_TARGET_DIR=/cache/target \
  --env CODEVETRA_BUILD_CACHE_ROOT=/cache/build-tools \
  --env SCCACHE_DIR=/cache/sccache --env SCCACHE_CACHE_SIZE=2G \
  --env CARGO_INCREMENTAL=0 --env CARGO_BUILD_JOBS=2 \
  --env RUSTC_WRAPPER= --env RUSTC_WORKSPACE_WRAPPER= \
  --env TMPDIR=/cache/tmp \
  --env CODEVETRA_RELEASE_TARGET="$TARGET" \
  --env CODEVETRA_RELEASE_ASSET_TARGET="$ASSET_TARGET" \
  --env CODEVETRA_RELEASE_MACHINE="$EXPECTED_MACHINE" \
  --env CODEVETRA_BENCHMARK_LINKERS="${CODEVETRA_BENCHMARK_LINKERS:-0}" \
  --env CODEVETRA_LINKER_REPORT=/cache/linker-comparison.json \
  "$BUILD_IMAGE" \
  timeout --signal=TERM --kill-after=30s 3300s bash -c '
    set -euo pipefail
    apt-get update
    apt-get install -y --no-install-recommends clang lld libclang-dev util-linux python3
    # A CPU quota alone can leave a large host affinity mask visible to worker
    # pools. Restrict affinity too, keeping the existing 256-PID limit intact.
    CPU_SET="$(python3 -c "import os; print(*sorted(os.sched_getaffinity(0))[:2], sep=chr(44))")"
    exec setpriv --reuid "$CODEVETRA_BUILD_UID" --regid "$CODEVETRA_BUILD_GID" --clear-groups \
      taskset --cpu-list "$CPU_SET" bash -c '\''
    set -euo pipefail
    trap "python3 scripts/report-build-resources.py" EXIT
    mkdir -p "$HOME" "$CARGO_HOME" "$CARGO_TARGET_DIR" "$RUSTUP_HOME" "$TMPDIR"
    machine="$(uname -m)"
    if [[ "$machine" != "$CODEVETRA_RELEASE_MACHINE" ]]; then
      echo "release container architecture mismatch: expected $CODEVETRA_RELEASE_MACHINE, got $machine" >&2
      exit 1
    fi
    # The pinned image supplies Debian/build utilities. Install and verify the
    # exact reviewed compiler in the persistent, admitted Rustup cache.
    rustup toolchain install "$CODEVETRA_RELEASE_TOOLCHAIN" --profile minimal \
      --component clippy,rustfmt --no-self-update
    if [[ "$(rustc --version)" != "rustc $CODEVETRA_RELEASE_TOOLCHAIN "* ]]; then
      echo "release container did not provide the pinned Rust compiler" >&2
      rustc --version >&2
      exit 1
    fi
    python3 scripts/setup-build-tools.py --tools sccache --bin-dir "$CARGO_HOME/bin"
    export PATH="$CARGO_HOME/bin:$PATH"
    # sccache 0.19 eagerly hashes Rust inputs through an unbounded blocking
    # pool, exhausting this container's PID budget even with Cargo jobs=2.
    # Empty Rust wrappers override Cargo config; explicit CC/CXX wrappers
    # still cache RocksDB/grammar objects. Cargo's target cache retains Rust.
    echo "[rust-release] Rust: persistent Cargo artifacts; C/C++: sccache"
    python3 scripts/verify-linux-build-cache.py
    if [[ "$CODEVETRA_BENCHMARK_LINKERS" == 1 ]]; then
      python3 scripts/setup-build-tools.py --tools mold wild --bin-dir "$CARGO_HOME/bin"
    fi
    scripts/build-rust-release.sh "$CODEVETRA_RELEASE_TARGET" "$CODEVETRA_RELEASE_ASSET_TARGET"
    sccache --show-stats
    python3 scripts/report-build-resources.py --require-headroom
  '\'''

CURRENT_DIGEST="$(python3 - "$ROOT" <<'PY'
import sys
from pathlib import Path
sys.path.insert(0, str(Path(sys.argv[1]) / 'scripts'))
from build_tools import snapshot
print(snapshot())
PY
)"
[[ "$CURRENT_DIGEST" == "$SOURCE_DIGEST" ]] || { echo 'Source changed during the release build; refusing to publish staged artifacts.' >&2; exit 1; }
mkdir -p "$ROOT/release"
cp "$STAGED/source/release/codevetra-$ASSET_TARGET" "$ROOT/release/"
cp "$STAGED/source/release/codevetra-$ASSET_TARGET.tar.gz" "$ROOT/release/"

VERSION="$(
  sed -n '/^\[workspace\.package\]$/,/^\[/ {
    s/^version = "\([^"]*\)"$/\1/p
  }' "$ROOT/Cargo.toml" | head -n 1
)"
if [[ -z "$VERSION" ]]; then
  echo "could not resolve the workspace release version" >&2
  exit 1
fi

docker run --rm --init --pull never --memory 512m --memory-swap 512m --cpus 1 --pids-limit 64 \
  --volume "$ROOT/release:/release:ro" \
  --env CODEVETRA_EXPECTED_MACHINE="$EXPECTED_MACHINE" \
  --env CODEVETRA_EXPECTED_VERSION="codevetra $VERSION" \
  --env CODEVETRA_RUNTIME_BINARY="/release/codevetra-$ASSET_TARGET" \
  "$RUNTIME_IMAGE" \
  timeout --signal=TERM --kill-after=5s 90s sh -c '
    set -eu
    machine="$(uname -m)"
    if [ "$machine" != "$CODEVETRA_EXPECTED_MACHINE" ]; then
      echo "runtime container architecture mismatch: expected $CODEVETRA_EXPECTED_MACHINE, got $machine" >&2
      exit 1
    fi
    glibc="$(ldd --version 2>&1 | head -n 1)"
    case "$glibc" in
      *" 2.41") ;;
      *)
        echo "runtime smoke did not execute on the Debian 13 glibc 2.41 baseline: $glibc" >&2
        exit 1
        ;;
    esac
    version="$($CODEVETRA_RUNTIME_BINARY --version)"
    if [ "$version" != "$CODEVETRA_EXPECTED_VERSION" ]; then
      echo "runtime smoke version mismatch: expected $CODEVETRA_EXPECTED_VERSION, got $version" >&2
      exit 1
    fi
  '

echo "[rust-release] Debian 13 / glibc 2.41 runtime smoke passed for $ASSET_TARGET"
