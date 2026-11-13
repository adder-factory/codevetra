#!/usr/bin/env bash
set -euo pipefail
export PATH="/root/.cargo/bin:$PATH"
# Expose the checksum-pinned solver shipped with Verus. Kani uses only SAT;
# CBMC 6.11.0's SMT floating-point lowering has a reproduced rounding defect.
verus_directory="$(dirname "$(readlink -f "$(command -v verus)")")"
export PATH="$verus_directory:$PATH"
export CARGO_BUILD_JOBS=1
export RAYON_NUM_THREADS=1
export RUSTFLAGS='--edition=2024'
kani --version | grep -Fx 'Kani Rust Verifier 0.68.0 (standalone)'
verus --version | grep -F '0.2026.11.02.8bf68ee'
cp -R /source/crates /source/verification /work/
cd /work
# The outer parser admits only the required suite, Verus, or one exact Kani
# harness and allowlisted solver. The suite rejects missing proof summaries.
exec python3 /source/scripts/formal/suite.py "$@"
