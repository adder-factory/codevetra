# Dependency update — 2026-10-31

[Documentation home](../README.md) · [Changelog](../RELEASES.md#dependency-audits)

CodeVetra v4.0.0 audits all 90 direct external dependencies and upgrades the
lockfile within upstream compatibility requirements. Rust remains exactly 1.99.1
with edition 2024. The application is native Rust; Python 3.15.2 is used for
maintainer audit helpers and is not a product runtime dependency.

The direct changes include ignore 0.4.34, globset 0.4.21, signal-hook 0.4.6,
rustix 1.1.6, tree-sitter-ada 0.9.2 and `rust-rocksdb` 0.54.0. The RocksDB binding
moves from the previous immutable Git pin to its exact registry release and
bundled RocksDB 11.12.0. The obsolete cargo-deny Git-source exception is removed.

## Lockfile changes

| Package | Previous | Selected |
| --- | --- | --- |
| globset | 0.4.20 | 0.4.21 |
| ignore | 0.4.33 | 0.4.34 |
| js-sys | 0.3.106 | 0.3.107 |
| libz-sys | 1.1.30 | 1.1.31 |
| log | 0.4.34 | 0.4.35 |
| notify-types | 2.1.0 | 2.2.0 |
| openssl-probe | 0.2.1 | 0.2.2 |
| rust-librocksdb-sys | 0.48.1+11.8.1 | 0.49.0+11.12.0 |
| rust-rocksdb | 0.53.0 | 0.54.0 |
| rustix | 1.1.5 | 1.1.6 |
| signal-hook | 0.4.5 | 0.4.6 |
| tree-sitter-ada | 0.9.1 | 0.9.2 |
| utf8_iter | 1.0.4 | 1.1.0 |
| utf_types | added | 1.0.0 |
| wasm-bindgen | 0.2.129 | 0.2.130 |
| wasm-bindgen-futures | 0.4.79 | 0.4.80 |
| wasm-bindgen-macro | 0.2.129 | 0.2.130 |
| wasm-bindgen-macro-support | 0.2.129 | 0.2.130 |
| wasm-bindgen-shared | 0.2.129 | 0.2.130 |
| web-sys | 0.3.106 | 0.3.107 |

Incompatible transitive major versions remain constrained by their upstream
consumers. This update does not patch those consumers to force unsupported APIs.

## RocksDB binding decision

The more mature general-purpose baseline is the long-running
[`rust-rocksdb/rust-rocksdb`](https://github.com/rust-rocksdb/rust-rocksdb) project.
Its broader maintenance history makes it the preferred migration target.
This release retains the tested
[`zaidoon1/rust-rocksdb`](https://github.com/zaidoon1/rust-rocksdb) fork: the
published mainstream wrapper does not expose our bounded value-read soft limit
or index-shortening control, and its macOS full-fsync build fix remains under
[upstream review](https://github.com/rust-rocksdb/rust-rocksdb/pull/1106).
A future switch requires API parity and durability/platform proof first.
[TiKV's binding](https://github.com/tikv/rust-rocksdb) is maintained around TiKV
and its engine extensions, making it a less direct general-purpose replacement.
CodeVetra owns its graph/storage application layer, not a separate RocksDB FFI wrapper.

## CI and build images

The pinned action audit selects checkout 7.0.1, cache 6.1.0, attest-build-provenance
4.2.2, upload-artifact 7.0.2, download-artifact 8.0.2 and cargo-deny-action 2.1.1.
Linux build and smoke images use verified platform manifests:

| Image | Architecture | Manifest |
| --- | --- | --- |
| rust:1.99.1-trixie | amd64 | `sha256:d841af61827b29b3efa46f5363e8f2e972b379cc0463c319e8f320d9f0c8eb0c` |
| rust:1.99.1-trixie | arm64 | `sha256:921d0846059e80eb1723e542a5090a76a245945511e4f59ea168e461febe3baf` |
| debian:13-slim | amd64 | `sha256:1e1a22175fbf1b32157ddaf0219e1db4e5401fab2b94ed285add64d18587cd2d` |
| debian:13-slim | arm64 | `sha256:1470991b497d26788dc4d6f438bf5690c6ba0d499235d237e55f4b43caea7a75` |

## Generation compatibility

Generation V27 records the actual language analyzer for symbol-health metrics,
operation-local safety evidence and supported application boundaries. An ordinary
source reindex is required. Frozen source/captures remain unchanged. Independent
canonical encoding first reproduces V26, then derives V27 pins from reviewed
fact changes; merely copying a failing test's actual digest is not validation.
Existing project, file, symbol, model, registry and older digest hash domains
retain their historical bytes through the product rename.

Sources: [crates.io](https://crates.io/),
[Rust stable manifest](https://static.rust-lang.org/dist/channel-rust-stable.toml),
[binding releases](https://github.com/zaidoon1/rust-rocksdb/releases), and
[Docker Official Images](https://github.com/docker-library/official-images).
