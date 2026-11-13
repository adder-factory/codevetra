# Formal verification

CodeVetra uses Kani 0.68.0 and Verus 0.2026.11.02.8bf68ee for pure contracts
at the production admission, storage, source, protocol and retrieval boundaries.
Required builds still use the exact stable compiler in `rust-toolchain.toml`.
Verifier compilers and solvers are development tools, never runtime dependencies.
Shuttle 0.9.10 additionally checks selected production async interleavings; its
separate locked harness is also absent from the shipped dependency graph.

## Scope and proof status

`verification/contracts.json` is the reviewed inventory. It binds function
signatures and Verus postconditions to live production sources and accounts for
every Kani harness as either required or explicitly experimental. Inventory
drift, missing proof summaries and unexpected proof counts fail the runner.
A successful required gate does **not** promote an experimental proof.

| Production owner | Required properties |
| --- | --- |
| MCP transport | Byte admission accepts exactly the representable sum within budget, including zero and machine boundaries |
| Source domain and configuration | Ordered nonempty spans, positive bounded values, Windows drive-prefix recognition |
| Extraction and task scope | Exact reservation arithmetic, overflow rejection, nonzero task-byte admission |
| Native storage | Worker/change/retention budget arithmetic, policy bounds, exact 32-byte writer fences, base-generation equality, retention eligibility and orphan grace |
| Source context and CLI | Clipped byte/line windows and overflow-free ceiling token estimates |
| SCIP protobuf | Complete header-plus-payload admission, cursor bounds, field tags, varint lengths, bounded decoder acceptance and rejection |
| Graph storage and MCP profiles | Radix geometry, prefix bits, lowercase hex decoding and explicit profile membership |
| Retrieval | Integer rank validation; floating-point proof status is recorded separately below |

Integer proofs use full machine domains unless an input bound is explicit in
the harness. The varint decoder checks arbitrary byte arrays of length 0–10
and arbitrary starting positions. Writer fences cover all 32 nonce bytes.
Scalar arithmetic is not a proof that a resource estimate matches actual heap
consumption. Retention/fence predicates are not proofs of locking, clocks,
transaction ordering, atomic publication or crash recovery.

The unfinished structural experiments cover path normalization up to 8 bytes,
key round trips up to 4 bytes with arbitrary 64-bit sequences, and malformed
key inputs up to 12 bytes. Those bounds do not cover the 4096-byte production
input limit. Allocation-heavy compositions have exceeded the 4 GiB solver
budget; they remain unproven, with unwinding assertions enabled. The required
prefix-successor proof covers all prefixes up to 4 bytes and arbitrary 4-byte
suffixes; it does not establish that property at every production length.

Score rejection and exact endpoints are checked separately from interior
floating-point division. Complete normalization and reciprocal-rank division
remain outside the required passing set. The general interior SAT run reached
its 60-second diagnostic deadline; no complete proof is claimed.

### Floating-point backend diagnosis

The initial Z3 interior run reported an assertion failure, but it used an
incorrect CBMC 6.11.0 floating-point encoding. The reduced input
`f32::from_bits(0x2480_0000)` is exactly `2^-54`. Binary64 subtraction
`1.0 - 2^-54` must round to `1.0` (nearest, ties to even). That exact assertion
fails in Kani's Z3 route and produces the concrete input bytes
`[0, 0, 128, 36]`; it passes with CaDiCaL and native stable Rust. The actual
production function with score `0.5`, minimum `2^-54`, maximum `1.0` also
passes the required SAT regression and returns `0.49999999999999994` natively.

[CBMC's upstream correction](https://github.com/diffblue/cbmc/commit/6866a974b9666902669f7d71cdcb8ef44de5ed0b)
replaces a 32-bit shift in `float_bvt::round_fraction` with arbitrary-precision
integer arithmetic. The old non-FPA SMT encoding can put a rounding carry at
bit 20 instead of bit 52, producing a tiny subnormal instead of the correctly
rounded value. This reproduces a false counterexample in the relevant
subtraction path and invalidates the original SMT result as production-bug
evidence; the original large harness did not yield its own concrete witness.

Kani therefore accepts only the pinned SAT backends (`cadical` or `kissat`).
Per-harness solver annotations are rejected to prevent bypassing that choice.
Verus still uses its pinned Z3 because it does not use CBMC's faulty lowering.
Do not change production arithmetic or weaken assertions to satisfy this model.
Before enabling SMT again, review a corrected verifier pin, rerun both carry
regressions, and complete the original general proofs under the same bounds.

Kani compiles production modules by path. Verus receives unchanged executable
function bodies extracted by `scripts/prepare-formal.py`, plus independently
specified integer postconditions. A changed signature or ambiguous function
fails extraction. The generated code has no `external_body`, unchecked proof,
or handwritten implementation copy; Verus runs with `--no-cheating`. The
compiler, solvers, verifier specifications and extraction bridge remain trusted
components. This follows the complete success/failure specification approach
in [Amazon's Verus article](https://www.amazon.science/blog/developing-provably-correct-rust-code-with-verus).
The required run also mutates admission to always reject in its disposable
snapshot and requires both verifiers to report a completed assertion failure.
An infrastructure error cannot satisfy these negative controls.

This expansion identified fixes for hidden Windows drive prefixes after path
normalization, SCIP output limits that omitted protobuf headers, ceiling token
estimation at `usize::MAX`, and finite extreme reranker logits whose subtraction
overflowed in `f32`. Production regressions exercise the path, codec and score
fixes; arithmetic proofs cover the token boundary. Ordinary and live tests still
own UTF-8 handling, filesystem identity, RocksDB, async cancellation/reaping,
provider behavior, parser/extractor semantics, determinism and retrieval quality.
This is a contract inventory, not a whole-program correctness claim.

## Randomized async schedules

`scripts/verify-formal.sh shuttle` runs eight production-bound scenarios through
Shuttle's randomized and depth-three PCT schedulers: 1,000 executions per
scheduler per scenario, or 16,000 required schedules. Each execution has a
10,000-step fail limit; early termination cannot count as a passing run.

The harness compiles the storage request/cancellation definitions from
`rocks/runtime.rs` and the complete staging barrier from `runtime/group.rs`.
The build extracts their live bodies verbatim; ambiguous source boundaries fail.
Shuttle replaces synchronization and Tokio primitives in that harness only.
Its separate locked dependency graph has its own advisory, license, source and
duplicate-version gate in `verification/shuttle/deny.toml`; the production
dependency policy remains in the repository root.
Tests cover multiple cancellation waiters, independent and racing parent/owner
cancellation, watch cancellation,
completion versus drain, registration versus closure, failed work, cancelled
drain retries, and abandoned wait retries. Two disposable mutations must expose
a replayable lost notification and a premature drain; compilation errors,
timeouts and missing scenario summaries cannot satisfy those controls.

This is schedule exploration, not an exhaustive proof of async correctness.
Shuttle does not model weak memory or wall-clock deadlines here. Native I/O,
RocksDB transactions, the full worker pool/indexer supervisor and error formatting
remain covered by their real runtime tests. A small error-value adapter supplies
the production variants used by the bound bodies; it does not replace their
cancellation or barrier logic. The pinned Tokio wrapper's modeled primitives
and the source extraction bridge are trusted components. A harness-only timer
adapter keeps `sleep_until` pending: the wrapper's default `Sleep` can complete
immediately for a future deadline, which would inject unrelated timeout exits
into these cancellation scenarios. Production deadline checks are unchanged;
the real runtime suite continues to test timeout behavior.

Failed schedules are printed into the persistent sandbox output. Replay one
with `scripts/verify-formal.sh shuttle --case <name> --replay <hex-schedule>`.
Replay and single-case runs retain the same kernel limits and step bound; they
do not count as the complete required suite. The [upstream release](https://github.com/awslabs/shuttle/releases/tag/v0.9.10)
includes notification/channel scheduling-point fixes required by these tests.

## Run locally

Use an existing local Docker engine with Linux cgroup v2 and at least 8 GiB
VM memory. The runner does not start, resize or restart a VM. On macOS this
keeps solver memory behind both the VM boundary and Linux's enforced cgroup
limit. Docker is used only for development verification, never by the shipped
CodeVetra executable or its RocksDB backend.

```sh
scripts/verify-formal.sh setup
scripts/verify-formal.sh
scripts/verify-formal.sh shuttle
```

Setup installs the pinned verifier versions in a bounded container and saves
a local image keyed by the setup recipe. Ubuntu is pinned by image digest;
Rustup, Kani and Verus downloads have architecture-specific SHA-256 checks.
The setup recipe supports Linux arm64 and x86_64. Verifier compiler toolchains
remain isolated from the production Rust toolchain on the host.

The required manifest proofs execute sequentially in one bounded invocation. To investigate one other
harness, use the same boundary and select its exact name:

```sh
scripts/verify-formal.sh kani verification/kani/contracts.rs --harness source_coordinates
scripts/verify-formal.sh --timeout-seconds 60 kani verification/kani/structures.rs --harness storage_key_round_trip
```

Do not invoke native `kani`, `cbmc`, `verus` or solver binaries directly.
`KANI_BIN` and `VERUS_BIN` host overrides are rejected. Extra solver flags,
multiple harnesses and parallel jobs are not accepted by the experimental
entrypoint. The optional solver selection is `cadical` or `kissat`; Z3 is rejected for Kani.
`--counterexample` prints diagnostic playback inside the same sandbox; it
never changes host source or relaxes the resource limits.

## Resource containment

The November 13, 2026 macOS incident involved overlapping structural Kani/CBMC
proofs, roughly 202 GiB of reported process-memory accounting on a 36 GiB
machine, compressor segment exhaustion, and a watchdog panic. Small symbolic
inputs and loop bounds did not bound solver memory. This runner therefore
enforces resources independently of proof complexity:

| Resource | Enforced bound |
| --- | --- |
| All compiler/solver descendants together | At most 4 GiB memory, zero container swap; `--memory-mib 128..4096` can request less |
| CPU and processes/threads | Two CPUs, 128 tasks |
| Concurrent runs | One per user across worktrees; fixed container name also fences orphaned runs |
| Proof duration | 300 seconds for the complete invocation, reducible to 1–300 seconds |
| Tool provisioning | 900 seconds, under the same memory/CPU/task bounds |
| Temporary proof storage | 1 GiB workspace and 256 MiB temporary files, charged to the memory limit |
| Logs | 16 MiB host log; bounded Docker log rotation |

A smaller requested memory limit remains a real aggregate cgroup limit; it
also lowers the required VM headroom to that budget plus 1 GiB. This lets a
small diagnostic run safely alongside existing VM workloads without resizing
the VM or stopping other containers. The 4 GiB ceiling cannot be raised.

The entrypoint checks actual `memory.max`, `memory.swap.max`, `pids.max`, and
`cpu.max` before doing work. It also requires available VM memory for the
budget plus 1 GiB headroom. Missing support or insufficient headroom fails
closed. An in-container deadline survives loss of the host client. Normal
exit, failure and cancellation remove the exact owned container, terminating
its PID namespace and descendants. If the host supervisor is forcibly killed,
the deadline still stops work and the retained container name prevents another
run until the old container has been inspected and explicitly removed.

Proofs receive an immutable, read-only snapshot of Rust sources, the manifest and binding scripts; generated
Kani artifacts, graph data and host credentials are excluded. The root
filesystem is read-only, network is disabled, and capabilities are dropped.
Logs, source digest, exit status, OOM state and cleanup result remain under
`~/.cache/codevetra-formal/runs/`. A timeout, OOM, lost transport or unconfirmed
cleanup is an incomplete/failed run, never a proof. Do not automatically retry
or raise limits: inspect the log and split or simplify the expensive harness
without weakening its assertions.

The complete required `quality` job runs inventory/admission tests and real low-budget
containment probes, provisions the tools, then runs the same proof entrypoint.
Documentation-only checks retain their existing behavior. CI execution remains
separate evidence from local successful proofs. The safeguards can be checked
without running a proof:

```sh
python3 scripts/test-formal-sandbox.py
python3 scripts/test-formal-sandbox.py --live
```

For a verifier upgrade, review upstream changes, update the version checks and
archive checksum together, and rerun the complete required suite plus the production gates.
Never weaken postconditions or suppress verification failures to accept an
upgrade.
