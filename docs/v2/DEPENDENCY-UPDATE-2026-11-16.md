# Dependency update — 2026-11-16

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits)

CodeVetra v4.0.6 selects Rust 1.100.0 with edition 2024 and audits all 92
direct external workspace dependencies against the live crates.io index.
The independent Shuttle workspace and pinned verification tools are included.
Direct dependencies use the latest non-yanked stable releases; transitive
versions remain within their upstream consumers' compatibility requirements.

## Selected changes

| Package or tool | Previous | Selected |
| --- | --- | --- |
| Rust | 1.99.1 | 1.100.0 |
| jiff | 0.2.43 | 1.0.0 |
| jiff-core | 0.1.5 | 1.0.0 |
| jiff-static | 0.2.43 | 1.0.0 |
| lz4_flex | 0.14.0 | 0.14.2 |
| bumpalo | 3.20.3 | 3.20.4 |
| mio | 1.2.4 | 1.2.5 |
| notify-types | 2.3.0 | 2.3.1 |
| cargo-deny / CI action | 0.20.2 / 2.1.1 | 0.21.0 / 2.1.2 |
| Sandbox Rustup installer | 1.28.2 | 1.29.3 |

The Jiff migration retains the existing timestamp parsing, printing, ordering,
and Unix-second interfaces. LZ4 still explicitly enables `safe-encode` and
`safe-decode`; the unsafe-decoder defect fixed in 0.14.1 does not affect that
configuration. The 0.14.2 published license matches the distributed notice.

## Rust integration

Production and Shuttle use the same exact stable compiler. `DropGuard` replaces
the request cancellation guard's separate armed flag: cancelled callers still
signal cancellation, delivered results dismiss the guard, and project-owned
tasks remain retained until completion. Regression tests cover request reuse,
worker panic, abandoned callers, and shutdown reaping.

Optional-value handling uses `map_or_default` where the new strict Clippy gate
requires it. Source-line handling uses `trim_suffix` with regression coverage
for CRLF, content carriage returns, UTF-8 byte offsets, and empty lines.
Cargo now rejects unused dependencies and unused workspace
dependency declarations. Binary-only members have no redundant dependency
entry; library path ownership and dependency-direction checks remain enforced.

## CI and verification

The cargo-deny action is pinned to commit
`70f81531115945ba6eb6b941ec9d39d063438eb9`. Other reviewed action releases remain
current. Shuttle 0.9.10, shuttle-tokio 1.0.0, Kani 0.68.0, Verus
0.2026.11.02.8bf68ee, and cargo-llvm-cov 0.9.1 remain current stable releases.
The Verus binary requires Rust 1.99.1 compiler libraries; sandbox setup installs
that separate verifier dependency while retaining Rust 1.100.0 as the default
for production and Shuttle.
Dependabot explicitly monitors the independent Shuttle manifest and lockfile.

| Image | Architecture | Manifest |
| --- | --- | --- |
| rust:1.100.0-trixie | amd64 | `sha256:ebc54bef33ec8f9a5cd05799366010f35efbb73b354fb4f2cb086ec6b9460ef5` |
| rust:1.100.0-trixie | arm64 | `sha256:66431732f567e2fd1301ecfbdc124ac32989c8953afeac895577cb114117454c` |
| debian:13-slim | amd64 | `sha256:7378260c0a098d27ac206225db5e7d92b8c7c84ea7afccc86eeb513756bbd392` |
| debian:13-slim | arm64 | `sha256:24a01011d10eba3a023bc8585b9650b8b29021240d4217c05b905e09ef4a5232` |

The verification sandbox refreshes the Ubuntu 24.04 multi-platform index to
`sha256:73b999393d07f2bf65a40373778a44dc9a6d54ad7718d29247d8ff9413c4d6a8`.
Setup downloads retain architecture-specific SHA-256 checks. The single-run
admission slot, enforced memory/swap/CPU/PID/deadline limits, source binding,
and negative controls remain required for formal and Shuttle checks.

RocksDB 11.12.0, its 0.54.0 Rust binding, the native Tree-sitter 0.27.2 runtime,
and the ABAP facade remain unchanged. Storage format v1, generation V27, and
the frozen extraction corpus retain their contracts; no reindex or migration
is required solely by this dependency update.

Sources: [Rust 1.100](https://blog.rust-lang.org/2026/11/12/Rust-1.100.0/),
[Jiff changelog](https://github.com/BurntSushi/jiff/blob/master/CHANGELOG.md),
[LZ4 changelog](https://github.com/PSeitz/lz4_flex/blob/main/CHANGELOG.md),
[cargo-deny action](https://github.com/EmbarkStudios/cargo-deny-action/releases/tag/v2.1.2),
and [Docker Official Images](https://github.com/docker-library/official-images).
