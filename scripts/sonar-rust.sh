#!/usr/bin/env bash

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SONAR_OUTPUT_ROOT="${CODEVETRA_SONAR_OUTPUT_ROOT:-$ROOT/target}"
CLIPPY_DIRECTORY="$SONAR_OUTPUT_ROOT/sonar"
CLIPPY_REPORT="$CLIPPY_DIRECTORY/clippy.json"
CLIPPY_TEMPORARY="$CLIPPY_REPORT.tmp.$$"
COVERAGE_DIRECTORY="$SONAR_OUTPUT_ROOT/llvm-cov"
COVERAGE_REPORT="$COVERAGE_DIRECTORY/lcov.info"

cleanup() {
  rm -f "$CLIPPY_TEMPORARY"
}
trap cleanup EXIT

coverage_test() {
  run_cargo test --locked --quiet --all-features "$@"
}

run_cargo() {
  python3 "$ROOT/scripts/rust-dev.py" "$@"
}

cd "$ROOT"
# Keep instrumented artifacts on the same verified target volume and under
# the same admission, memory, process-ownership, and deadline guards.
mkdir -p "$CLIPPY_DIRECTORY" "$COVERAGE_DIRECTORY"
rm -f "$CLIPPY_REPORT"

run_cargo clippy --locked --workspace --all-targets --all-features \
  --message-format=json -- -D warnings >"$CLIPPY_TEMPORARY"
mv "$CLIPPY_TEMPORARY" "$CLIPPY_REPORT"
COVERAGE_BUILD_DIRECTORY="$(PYTHONPATH="$ROOT/scripts" python3 -c 'from build_tools import prepare_coverage_target; print(prepare_coverage_target())')"

# The actual native store is exercised by ordinary process/database tests.
# Keep obsolete endpoint variables absent for deterministic negative inputs.
unset CODEVETRA_DATABASE_URL CODEVETRA_DATABASE_SCHEMA CODEVETRA_TEST_DATABASE_URL

export CARGO_LLVM_COV_TARGET_DIR="$COVERAGE_BUILD_DIRECTORY"
COVERAGE_ENVIRONMENT="$(run_cargo llvm-cov show-env --sh)"
eval "$COVERAGE_ENVIRONMENT"
CARGO_LLVM_COV_DENY_WARNINGS=1 run_cargo llvm-cov clean --workspace
run_cargo test --locked --quiet --workspace --all-features

coverage_test -p codevetra-db --test rocks_graph \
  -- --ignored --nocapture --test-threads=1
coverage_test -p codevetra-indexer --test live_supervisor \
  -- --ignored --nocapture --test-threads=1

coverage_test -p codevetra-cli --test rocks_owner \
  -- --ignored --nocapture --test-threads=1

run_cargo llvm-cov report --locked --lcov \
  --output-path "$COVERAGE_REPORT"

sonar scan \
  -Dsonar.rust.clippyReport.reportPaths="$CLIPPY_REPORT" \
  -Dsonar.rust.lcov.reportPaths="$COVERAGE_REPORT" \
  -Dsonar.qualitygate.wait=true \
  -Dsonar.qualitygate.timeout="${SONAR_QUALITYGATE_TIMEOUT:-300}" \
  "$@"
