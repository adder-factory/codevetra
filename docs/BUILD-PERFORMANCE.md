# Rust build and test workflow

CodeVetra already separates ownership into 11 Cargo workspace members. Work on
the owning crate first, then run the complete gates before merging. Adding
crates solely to increase the count can increase linking and coordination cost.

Install the checksum-pinned prebuilt tools before the first build:

```sh
python3 scripts/setup-build-tools.py
python3 scripts/rust-dev.py nextest run --locked -p codevetra-domain
python3 scripts/rust-dev.py nextest run --locked -p codevetra-extract
python3 scripts/rust-dev.py clippy --locked -p codevetra-indexer --all-targets --all-features -- -D warnings
```

The installer checks SHA-256 and executable versions before replacing tools.
The reviewed versions and platform assets are in `scripts/build-tools.json`.
It needs Python 3.11 or newer and downloads release binaries instead of building
another toolchain. Windows uses `python` in place of `python3`.

## Cache placement on the development Mac

Cargo home, Rustup home and this checkout's `target` are on the mounted
`/Volumes/RUST CACHE` drive. Keep them there. Do not create replacement Rust
caches on the main drive when the volume is absent. The checkout-local policy
also puts compiler caches, downloads, Linux build caches, staging and compiler
temporary files on that drive. Configure or re-establish the policy with:

```sh
python3 scripts/setup-build-tools.py \
  --require-volume '/Volumes/RUST CACHE' \
  --cache-root '/Volumes/RUST CACHE/codevetra-build'
```

This records the mounted volume UUID in ignored `.codevetra/build-tools-local.json`.
It validates the existing Cargo, Rustup and target paths; it does not move or
delete them. Local macOS builds require this explicit policy; a missing policy
is an error. The runner rejects an absent/replaced volume, escaping cache
symlinks, target/output overrides or inline configuration that could redirect
artifacts. A new worktree needs its own external target
directory and the same explicit volume policy. Share the compiler cache, not
one Cargo target directory between unrelated active checkouts.

Use `scripts/rust-dev.py` for local Cargo builds and tests. The runner checks
normal macOS memory pressure, admits one build per cache root, limits Cargo
and tests to two workers, owns its compiler server, and terminates that server
and the Cargo process group on cancellation or deadline. Its default deadline
is one hour (maximum two). Unix runs also sample aggregate descendant RSS and
stop above 8 GiB. This sampled guard is not a kernel memory limit; formal
verification must continue through the separate cgroup-limited
`scripts/verify-formal.sh` supervisor. Never run Kani or Verus directly on the
Mac. Hosted CI uses each runner's explicitly restored ephemeral cache.
On Unix, a compiler-server wrapper and Cargo retain the admission lock: killing the
supervisor cannot admit another build while those processes are still alive.
Inspect surviving owned work before retrying; never delete the lock file.
The owned server's idle timeout exceeds the admitted command deadline by
30 seconds, allowing long tests while keeping orphaned idle servers finite.

## Compiler caching

The repository Cargo configuration wraps Rust and native C/C++ compilers with
sccache, including RocksDB and grammar builds. Local compiler-cache storage is
capped at 8 GiB; CI uses 2 GiB. Cargo target artifacts have their usual lifecycle
and are not included in that compiler-cache cap.

Normal local incremental Rust builds remain enabled. sccache 0.19 passes those
through; it caches eligible non-incremental libraries and C/C++ objects. CI
disables incremental compilation to use the shared compiler cache. Binaries,
procedural macros and crates needing system linking are not Rust cache hits.
Checkout paths and `CARGO_MANIFEST_DIR` are retained in keys, so a cache hit
cannot silently substitute a fixture path from another worktree. See the
[pinned sccache Rust limitations](https://github.com/mozilla/sccache/blob/v0.19.0/docs/Rust.md).

Linux release builds persist registry, toolchain, target and compiler caches
under a key containing architecture, exact compiler and build-image digest.
Inside the bounded Linux release container, Rust uses Cargo's persistent target
cache and C/C++ uses sccache. Rust sccache is explicitly disabled there because
version 0.19 eagerly hashes Rust inputs through a blocking pool without a
configurable thread limit, which exhausted the 256-PID budget. Other builds
retain Rust sccache. A real compiler probe checks this split, native cache hits
and changed-input behavior; reaching the container's PID limit fails the build.
They stage public source without copying private graph or Git state, check for
source changes before publishing artifacts, and retain all archive/privacy
and Debian runtime checks. Identical public snapshots reuse a staged timestamp
bound to their content and executable permissions; changed snapshots must be
newer than cached artifacts. This avoids unnecessary binary rebuilds while
preserving [Cargo's source freshness checks](https://doc.rust-lang.org/stable/nightly-rustc/cargo/compiler/fingerprint/index.html#mtime-comparison).
Clock skew or filesystem precision that prevents proving freshness is an error.
A local Docker endpoint with cgroup v2 is required;
build containers have 4 GiB RAM, no swap, two CPUs, 256 PIDs and a 55-minute
deadline. A surviving build container fences the next run until inspected.

## Test execution

```sh
python3 scripts/rust-dev.py nextest run --locked --workspace --all-features --profile ci
python3 scripts/rust-dev.py test --locked --workspace --all-features --doc
```

Nextest runs individual tests in separate processes with two concurrent tests,
zero retries and a five-minute per-test deadline. Process/owner suites share
a one-test group. The explicitly selected 30-minute soak retains a 35-minute
deadline. Ordinary runs still skip ignored tests; CI's separate native live,
fault, determinism and soak commands remain required. Doctests always run
separately because nextest does not execute them. `cargo test-fast -p <crate>`
is a convenience alias; on the development Mac prefer the guarded runner.

CI restores and saves the existing Rust dependency/target cache and includes
the build-tool manifest in its cache key. Hosted CI keeps temporary files in
the runner's temporary directory outside the checkout, so Git test fixtures
cannot accidentally discover the parent repository. Tool and workflow contract tests
protect the unfiltered nextest suite, separate doctests, resource settings,
asset integrity and external-volume admission.

## Linux linking

The reviewed Rust compiler already uses LLD by default on x86_64 Linux.
ARM Linux explicitly uses Clang with LLD; install both when building directly
on Linux. The release container installs them. macOS keeps Apple's linker and
Windows keeps its MSVC toolchain.

The pinned installer also supports `--tools mold wild` on Linux. To compare
them against BFD and LLD using the actual optimized CLI objects, set
`CODEVETRA_BENCHMARK_LINKERS=1` when invoking `scripts/build-linux-rust-release.sh`
with its usual pinned image arguments. The comparison runs three sequential
links per candidate, smoke-tests every successful output and records timings
in the persistent Linux cache's `linker-comparison.json`. It keeps the LLD
artifact for the normal archive/privacy/runtime gates. An alternative linker
must pass those gates and demonstrate a measured benefit before becoming the
default; installing a tool alone is not evidence of a faster build.
