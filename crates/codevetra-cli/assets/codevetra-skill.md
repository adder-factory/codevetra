---
name: codevetra
description: Explore a codebase through CodeVetra v4's native RocksDB code graph: intent-aware context, exact/BM25/hybrid search, callers/callees/impact, affected tests, live source, review, freshness, and bounded index administration.
---

# CodeVetra v4

CodeVetra is a native Rust MCP server for coding agents. Its durable graph lives
in each project's `.codevetra/data` RocksDB database. There is no SQLite
runtime, fallback, migration target, or optional feature.

## Existing project

For legacy `.cartograph/` state, stop the existing host and run
`codevetra migrate-name --project-path <project>` before indexing. Follow the
[name migration guide](https://github.com/adder-factory/codevetra/blob/main/docs/CODEVETRA-MIGRATION.md),
reinstall managed host registrations, and restart the host. Preserve the original
state and permission restrictions; never initialize a second index beside it.

When `.codevetra/` or a project-local CodeVetra MCP registration exists:

1. Call `codevetra_status` before trusting graph evidence.
2. Use `codevetra_context` for a coding task. It classifies a typed intent,
   returns immutable-generation provenance, and keeps any stale working-tree
   overlay visibly separate.
3. Narrow with exact `codevetra_find`, `codevetra_node`, or
   `codevetra_graph` calls.
4. After edits, use `codevetra_review` against the intended base and
   `codevetra_affected` for structurally related tests.
5. Run the repository's real test/quality commands; CodeVetra evidence guides
   verification but does not replace it.

Never present stale or unknown-freshness evidence as current. If the index is
stale, use the explicit `codevetra_admin` index/sync action and poll its job
status. Do not delete locks or database state while an owner is alive.

| Tool | Use |
| --- | --- |
| `codevetra_status` | Current generation, counts, and live-source freshness |
| `codevetra_context` | Intent-aware exact/BM25/hybrid evidence, graph context, affected tests, and live overlay |
| `codevetra_find` | Exact name/path/reference, BM25, or hybrid candidates |
| `codevetra_node` | Exact symbol metadata plus bounded source when line provenance is fresh |
| `codevetra_graph` | Bounded callers, callees, or reverse impact |
| `codevetra_affected` | Tests reachable through bounded reverse impact |
| `codevetra_numerical` | Generation-scoped static numerical evidence, coverage, explanation, and non-executing probe plans |
| `codevetra_review` | Git-ref plus staged/unstaged/untracked review evidence |
| `codevetra_admin` | Explicit index/sync/embed job start, status, and cancellation |

The MCP profiles are `coding`, `core`, `full`, `read-only`, and `review`. Use the
narrowest profile that supports the task. MCP requests have hard input/output
limits, deadlines, cancellation, and stable error codes; preserve those errors
rather than retrying an unbounded variant.

If MCP transport is unavailable, say so and use the equivalent native CLI as a
control path. A successful CLI call does not prove an already-open host has
hot-restarted its MCP process.

## New project

If no CodeVetra setup exists, ask:

> I notice this project does not have CodeVetra initialized. Would you like me
> to run `codevetra admin init -i` to build a code knowledge graph?

```sh
codevetra index .
codevetra doctor .
codevetra status .
codevetra context 'explain the primary request flow' --project-path .
codevetra install --yes --target codex --location local --project-path .
```

Setup is ready only after doctor, index, status/freshness, and one real context
or find query pass. Restart the agent host after writing MCP configuration.
One primary owns a project; on Unix other CLI/MCP clients reuse that owner.
Never remove database locks or files while an owner is alive.

## Earlier database cutover

`codevetra db migrate --to rocksdb --project-path .` rebuilds and verifies
source facts, then records migration completion. Legacy PostgreSQL data remains
untouched. Notes, sessions, embeddings, and other legacy-only state are not
imported. No PostgreSQL runtime or SQL query tool remains in this checkout.
