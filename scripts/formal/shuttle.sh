#!/usr/bin/env bash
set -euo pipefail
export PATH="/root/.cargo/bin:$PATH"
export CARGO_BUILD_JOBS=1
export RAYON_NUM_THREADS=1
unset RUSTFLAGS CARGO_ENCODED_RUSTFLAGS CODEVETRA_SHUTTLE_MUTATION
cp -R /source/crates /source/verification /work/
cd /work
exec python3 /source/scripts/formal/shuttle.py "$@"
