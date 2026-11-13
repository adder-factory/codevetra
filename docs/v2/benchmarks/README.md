# Verification and benchmark evidence

[Documentation home](../../README.md) · [Project overview](../../../README.md) ·
[Architecture](../ARCHITECTURE.md) · [Performance tuning](../../PERF-TUNING.md) ·
[Changelog](../../RELEASES.md)

These records answer different questions. Read the evidence boundary on each
page before comparing numbers across corpora, versions, machines, or retrieval
tasks.

## Records

| Record | What it answers | Evidence status |
| --- | --- | --- |
| [Bounded-index scaling](INDEX-SCALING.md) | How worker count affects deterministic synthetic indexing | Historical benchmark with committed machine-readable reports |
| [Native real-corpus scaling](NATIVE-CORPUS-SCALING.md) | Whether 1/2/4/8/16 workers preserve one logical result on a real corpus | Historical benchmark with committed digest and resolver records |
| [Large public corpus streaming](LARGE-PUBLIC-CORPUS-STREAMING.md) | How PostgreSQL spill bounds a large checkout and separates parse from publication time | Published v2.1.11 benchmark record; not an exact current-tag rerun |
| [Native patch-task evaluation](PATCH-TASK-EVALUATION.md) | Whether bounded retrieval identifies edits and tests for locked repair tasks | Historical task-quality benchmark with explicit payload accounting |

## How to use these records

- Compare only runs with the same corpus, query/task set, toolchain, database
  contract, hardware class, and measurement definition.
- Treat logical digest identity and gate outcomes as correctness evidence;
  timings are environment-specific observations.
- Distinguish parse time, complete generation publication, retrieval latency,
  response bytes, and estimated content tokens. They are not interchangeable.
- Follow each page's reproduction command rather than reconstructing a command
  from a result table.

> [!NOTE]
> Current release qualification also includes strict Rust gates, PostgreSQL live
> fault suites, platform archive smokes, Sonar, independent review, checksums,
> signed tags, and GitHub provenance. Those release gates complement these
> benchmarks; they do not make a historical measurement current. See
> [release evidence](../ARCHITECTURE.md#release-evidence).

## Retrieval issue regression cases

`crates/codevetra-agent/tests/fixtures/retrieval_quality/issue_228.json` contains
three public, independently labeled implementation queries from issue #228.
They are development regressions, not held-out evidence. Use
`scripts/evaluate-retrieval.py --cases <that file> --binary <binary> --project
<isolated snapshot> --output <new report> --label <before-or-after>` for each
binary against the same source and model configuration. The runner separately
reports BM25 and hybrid observations, preserves reranker/fallback evidence and
rejects stale sources, missing labels or incomplete semantic readiness.

The deterministic regression suite tests production-versus-enum/parameter/fixture
ordering, retained raw channel ranks, a one-result BM25 limit, and exact leaf
lookup. These tests establish those behaviors, not an improvement percentage on
real model rankings. No paired model benchmark is claimed by adding the cases.
