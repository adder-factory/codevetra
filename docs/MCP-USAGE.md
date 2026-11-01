# MCP usage for coding agents

[Documentation home](README.md) · [Project overview](../README.md) ·
[CLI reference](CLI-REFERENCE.md) · [Troubleshooting](TROUBLESHOOTING.md)

Last release audit: 2026-11-01 (`v4.0.0`).

CodeVetra v2 exposes a compact native stdio MCP server. Its core returns
bounded, generation-scoped evidence and never makes the database a source of
truth over the live checkout. The optional `codevetra_ask`, role, summary, and
dead-code-judge branches, and the optional decision-tier (Jev) calls in
`codevetra_explore`, `codevetra_context`, and `codevetra_propose_rename`
(see [Optional Jev navigation](CONFIGURATION.md#optional-jev-navigation)), can
call configured LLM tiers; their model/evidence provenance, failure, and
fallback states remain explicit.

**On this page:** [Fast path](#fast-path) · [Registration](#registration) ·
[Auto-sync and admin jobs](#auto-sync-and-admin-job-reporting) ·
[Profiles](#profiles) ·
[Modern protocol](#modern-protocol-and-dynamic-tool-selection) ·
[Selected high-use tools](#selected-high-use-tools) ·
[Reliable agent loop](#reliable-agent-loop) ·
[Transport contract](#transport-contract) ·
[Modernization order](#modernization-order)

## Fast path

1. Register the project with `codevetra install --yes --target <HOST>
   --location local --project-path .`.
2. Reopen the host so it starts the newly registered MCP child.
3. Call `codevetra_status` and require a fresh current generation.
4. Call `codevetra_context` or `codevetra_find` once and require real
   generation-scoped evidence. Registration and CLI success alone do not prove
   the attached transport.

## Registration

Prefer the native installer because it writes project-local configuration and
pins the absolute stable launcher path for versioned native installs:

```sh
codevetra install --yes --target codex --location local --project-path .
codevetra install --yes --target claude --location local --project-path .
codevetra install --yes --target cursor --location local --project-path .
```

### Manual server definition

```json
{
  "command": "/absolute/path/to/codevetra",
  "args": [
    "serve",
    "--mcp",
    "--project-path",
    "/absolute/path/to/project"
  ]
}
```

Each project opens its own `.codevetra/data`; no database credentials or port
are required. On Unix another CLI or MCP process routes through that project's
same-user owner endpoint. Windows currently requires one process per project.

### Restart and prove the transport

> [!IMPORTANT]
> Restart the host after installation. An already-open host is not assumed to
> hot-reload an upgraded MCP process.

`codevetra doctor --json` keeps on-disk registration and live MCP transport as
separate readiness states. The CLI does not claim either one from a successful
database or generation check: inspect the project-local registration, restart
the host when it changes, then make a fresh MCP status and real query call to
prove the loaded transport.

## Auto-sync and admin job reporting

### Native owner and source catch-up

The server opens one per-project RocksDB primary and starts source catch-up
through its bounded watcher. Source synchronization does not delay the MCP
handshake for an entire initial index. Other Unix CLI/MCP processes reuse the
same-user owner IPC service rather than opening another primary. Windows
currently requires one primary process at a time for a project.

On Unix, `codevetra watch --project-path .` runs the same owner independently of
MCP stdin. On macOS, `codevetra service start --project-path .` installs it as a
project-scoped login service pinned to the invoking executable. MCP processes
become proxies and their disconnect does not stop that owner. `service status`
reports registration, launchd state, and owner availability independently;
`service stop` stops and removes only its verified owned registration. An
already active owner retains the primary while the login service retries.

Status exposes index attempts, publications, no-ops, errors, backoff, capacity
suppression, and independent maintenance outcomes. Native capability readiness
and graph freshness remain separate. Require exact current evidence before
using a result as source truth.

`autoSync.recentFailures` retains the latest 16 redacted failure events across
recovery. `failureActive` reports a current retry condition; `lastErrorCode`,
`lastCleanupFailureCode`, and `lastFailureAt` retain the latest historical event.
`lastIndexProfile` reports the latest successful preparation, pipeline, per-stage,
history, and total timings separately from debounce and query latency.
`sourceManifestReused` means discovery and byte hashing occurred during
preparation and were admitted by the pipeline's read stage; no second discover
stage is reported.

### Automatic retry policy

| Outcome | Code | Handling |
| --- | --- | --- |
| Persistent failure of an unchanged revision | — | Bounded exponential backoff; stops after five automatic attempts until source changes |
| Concurrent edits | `source_changed_during_index` or `parse_source_changed` | Recovery scheduled after 2–30 seconds; does not exhaust the persistent-failure circuit |
| Another admitted native writer | `lease_busy`, before admitting a competing writer | Recovery scheduled after 2–30 seconds; does not exhaust the persistent-failure circuit |
| Obsolete revisions or auxiliary data awaiting a bounded collection batch | `retention_backlog` | Bounded exponential backoff from 30 seconds to 15 minutes; does not exhaust the persistent-failure circuit |
| Generation-capacity failure | — | Five trip a separate circuit that new source revisions cannot bypass; adjust the reported capacity setting and run an explicit index to prove recovery and clear the circuit |
| Native storage unavailable before a revision can be recovered | — | Watcher events share an unknown-revision backoff bucket and retain a capped recovery probe rather than retrying every event or becoming permanently suppressed |

- A retry timer runs even when no new filesystem event arrives; it does not wait
  for the 30-second missed-event reconciliation.
- Every automatic failure also attempts bounded cleanup of terminal failed
  generations.
- When an attempt's own staging cleanup fails after another failure,
  `lastErrorCode` keeps the first failure's code and its retry policy, and the
  additive `lastCleanupFailureCode` reports `index_cleanup_failed`; it is
  omitted otherwise.

`autoSync.lastSyncDurationMillis` reports the last successful structural index
through its awaited maintenance/source checks, excluding debounce and query
transport. `autoSync.lastIncrementalFiles` is positive for a proven local
replacement and zero for full/no-op reconciliation. These fields distinguish
watcher detection time from actual index work.

### Watcher timing and sync flags

Watcher events use a 150 ms quiet window with a default two-second hard
coalescing deadline, then call the indexer's own manifest/no-op fence directly.
Automatic attempts cap native extraction at four workers and omit independent
Git-history enrichment; explicit indexing refreshes those auxiliary channels.
Periodic retention runs concurrently with this event loop under the same
supervised host lifetime. It drains generations and parse-cache rows while
source is unchanged, with independent capped failure backoff and exact native writer
fences. `--no-auto-sync` also disables this periodic maintenance owner.

| `serve` flag | Effect |
| --- | --- |
| `--no-startup-sync` | Suppresses only the initial reconciliation |
| `--no-auto-sync` | Disables both native watching and periodic reconciliation for an operator-controlled recovery host while leaving explicit MCP/CLI operations available |

### Admin index job failures

A failed admin job's terminal status keeps the failure that ended it in
`failure`. Additive objects sit beside it:

| Object | When it appears | Contents |
| --- | --- | --- |
| `fileFailure` | A non-recoverable file-local admin index failure; the terminal job still exposes its stable failure category and the previous published generation remains untouched | The normalized project-relative `path`, fixed `reason`, and credential-safe `description` |
| `cleanupFailure` | An `index`, `sync`, `embed-only`, `scip-import`, or indexing `init` job failed and the bounded cleanup of its own staging generation also failed, or a cancelled job's cleanup was not confirmed by the native owner (see details) | `code: index_cleanup_failed` and `message` |
| `overlayRollbackFailure` | A `scip-import` job's forced index failed or was cancelled and restoring the previous SCIP overlay failed too | `code: scip_overlay_rollback_failed` and `message` |
| `failureDetail` | A terminal generation-capacity failure | Guidance naming `maxGenerationBytes`, its `codevetra_process` scope, and the next action |

Absolute checkout paths, source/parser text, literals, database URLs, and driver
messages are not part of the MCP result. An invalid parser-recovery span or
parser stop without cancellation is a successful partial-file outcome with
`extraction_invalid_span` or `extraction_parser_stopped` in the bounded
degraded-file report.

A cancellation is nonterminal while admitted work is draining. The owner keeps
its source tasks, native futures, and writer admission until actual completion.
A failed cleanup is independent of the original failure and remains explicit;
no timeout is reported as successful cleanup. Atomic publication and the WAL
preserve the last complete graph if an unpublished attempt stops.


> [!WARNING]
> A `scip-import` job whose forced index fails or is cancelled restores the
> project's previous SCIP overlay. When that restore fails too, its terminal
> status adds an optional `overlayRollbackFailure` object of the same shape,
> with `code: scip_overlay_rollback_failed` and `message`. It sits beside
> `failure`, or the cancellation, and any `cleanupFailure`, and replaces none of
> them; a cancelled import still gets the native cleanup check above.
> Nothing retries the restore: `.codevetra/scip/overlay.scip` may still hold
> the requested artifact, which the next index, including an automatic one,
> would use.

## Profiles

Select a profile with `codevetra serve --mcp --profile <PROFILE>`; `core` is
the default. Only `codevetra_admin` is profile-restricted; every other tool
belongs to all five profiles.

| Profile | Advertised tools | Intended use |
| --- | --- | --- |
| `full` | All 36, including `codevetra_admin` | Every advertised tool, including bounded administration |
| `core` (default) | All 36, including `codevetra_admin` | Normal coding tools plus explicit bounded administration |
| `coding` | The 35 tools other than `codevetra_admin` | Lean retrieval, source, graph, test-selection, and review loop |
| `review` | The 35 tools other than `codevetra_admin` | Comparison and verification-oriented surface |
| `read-only` | The 35 tools other than `codevetra_admin` | Retrieval without write/admin operations: `serve` additionally refuses every mutating call branch |

`serve --no-write-tools` applies the same read-only call enforcement under any
profile.

Profiles are immutable authorization ceilings for one server process. Tool
lists are deterministic, and a tool hidden by the selected profile or an exact
`--disable-tool` cannot be called by name. Use the narrowest ceiling that
supports the workflow.

When a second MCP process uses the project's active RocksDB owner, its tool
ceiling also applies to every saved-macro step and project-routed call. The
proxy and owner limits intersect; forwarding cannot enable a tool hidden by
either process. Read-only branch checks still apply before dispatch.

## Modern protocol and dynamic tool selection

CodeVetra is a dual-era stdio server:

- modern clients use MCP `2026-07-28`, begin with `server/discover`, and send
  their protocol version and capabilities on every request;
- existing clients may continue to use the `2024-11-05` initialize handshake;
- modern successful results carry `resultType: "complete"` and server identity;
- modern `tools/list` is private-cacheable for one hour and returns the same
  deterministically ordered authorization-scoped catalog for the lifetime of
  the process.

MCP `2026-07-28` deliberately forbids changing `tools/list` per connection or
as a side effect of another request. CodeVetra therefore does not implement a
generic hidden-tool dispatcher or a session-local activate/deactivate tool.
Those patterns would hide the selected operation's schema and annotations from
the host and weaken host-level confirmation policy.

Dynamic selection belongs in the agent host: it can retain CodeVetra's
complete stable catalog, search the compact name/title/description metadata,
and place only task-relevant full schemas in the model's working context. The
profile remains the call-time authorization ceiling regardless of which schemas
the host currently presents to the model. A host that does not support deferred
schema loading can safely expose the complete profile catalog.

## Selected high-use tools

The full profile advertises 36 tools. The table below highlights the normal
coding loop; see the [complete CLI/MCP alignment inventory](cli-mcp-alignment.md#public-mcp-tools)
for all 36 wire contracts and their CLI families.

| Tool | Purpose |
| --- | --- |
| `codevetra_status` | Current generation, row counts, compact database/schema storage as exact bytes plus readable IEC units, complete supported-source freshness, and auto-sync state |
| `codevetra_context` | Intent-aware exact/BM25/hybrid packet, typed primary edit candidates, graph evidence, affected tests, trust, and live overlay |
| `codevetra_find` | Exact name/path/reference, bounded live-source regex, or BM25/hybrid candidates |
| `codevetra_files` | Bounded current-generation file inventory filtered by directory or language |
| `codevetra_entry_points` | Typed routes, CLI commands, MCP tools, CLI declarations, and public API boundaries with exact totals |
| `codevetra_at_range` | Exact symbols overlapping one source range or diff hunk |
| `codevetra_node` | Exact symbol metadata and bounded source only when indexed line provenance is fresh; batches retain partial results and identify unresolved or ambiguous inputs |
| `codevetra_graph` | Bounded callers/callees/impact, exact edge filters, shortest paths, or model-scoped vector symbol neighbors |
| `codevetra_affected` | Structurally connected test candidates; file and symbol modes both enforce `depth`, `maxNodes`, and `limit`, and file mode reports `impact.nodesTruncated` when its traversal budget is reached |
| `codevetra_numerical` | Generation-scoped static numerical sites, coverage, explanation, and non-executing probe plans with explicit evidence levels and unknowns |
| `codevetra_review` | Git-ref plus committed/staged/unstaged/untracked review packet |
| `codevetra_playbook` | Complete agent workflow, tool-routing map, evidence discipline, and anti-patterns |
| `codevetra_admin` | Start, inspect, or cancel bounded lifecycle, index, semantic, model, and SCIP interchange work |

### Graph traversal fields

Edge-kind, confidence and test-target filters run before retaining the bounded
candidate page. The default calls and impact relation sets follow the same
policy as explicit `edgeKind` filters. Unrelated edges cannot consume the
2,000 matching-edge budget; storage reads still obey their independent work,
deadline and cancellation limits. The current vector-returning storage API
cannot distinguish an exactly full page from an omitted matching candidate,
so reaching 2,000 matching edges conservatively sets `truncated: true`.
Fewer matching edges do not become partial solely because filtered edges exist.


Graph `via.from_symbol_id` and `via.to_symbol_id` describe traversal order:
the preceding and discovered symbols. `via.edge_source_symbol_id` and
`via.edge_target_symbol_id` always describe the stored relation direction.
For `A calls B`, an incoming traversal from B discovers A while retaining
edge source A and edge target B. These endpoint fields also appear in Jev
navigation graph results.

### Numerical analysis

`codevetra_numerical` currently uses the `rust_ast_v1` static analyzer for
parsed or partial Rust files.

| Mode | Returns |
| --- | --- |
| `sites` | Exact source spans and bounded categories without persisting expressions or literal values |
| `coverage` | Supported, analyzed, skipped/failed, and site-bearing files, separately |
| `explain` | Can attach graph-selected tests for one exact owner |
| `plan` | Probe steps without executing project code |

Static `heuristic`, future runtime observation, and formal-proof evidence remain
separate. Observation and formal adapters currently report `not_configured`, and
stale generations report `stale_static_evidence` even when stale reads are
explicitly allowed.

### Review lenses and context mode

`codevetra_review` risk mode binds every lens to the current generation and
returns a `lensStatus` for findings, hotspots, dead-code candidates, and
coverage. Each lens is independently bounded and reports `ready`, `timeout`, or
`unavailable` with stage, limit, and retry guidance. Counts derived from the
returned window are labeled `returned_rows_only`; partial non-Git or large
project evidence is never presented as a complete scan. The structural-findings
lens additionally reports `not_computed` with the explicit refresh action when
its exact relation is absent; an empty result is therefore never mislabeled as
a clean ready lens.

`codevetra_review` context mode accepts the shared `pathFilter` and
`allowStale` fields advertised by its schema:

- `pathFilter` is a validated project-relative segment prefix for both live Git
  comparison and supplied-diff evidence; sibling prefixes do not match.
- `allowStale` is a compatibility no-op in context mode because the Git review
  packet already reports immutable graph freshness separately, while non-Git
  review lenses continue to use it as an explicit stale-evidence opt-in.

`agent-audit` also accepts `pathFilter` as a project-relative segment prefix.
Filtering happens before detector counts and per-detector limits; complete
project totals remain separately labeled as unfiltered. Readiness, totals, and
rows share one native snapshot with its generation and input fingerprint.
Missing biomarker data returns top-level `state: not_computed`, null findings/counts, and the confirmed
refresh action. `state: ready` with zero counts means the computation completed.
An argument unsupported by a review mode names that argument, the selected mode,
and its allowed arguments.

### Biomarkers

- `codevetra_biomarkers` is read-only. When the current fingerprint has no
  stored complete relation it returns `state: not_computed`, `findings: []`, and
  the exact confirmed refresh action without starting detector computation.
- `codevetra_status` preserves the rest of its payload in that state and
  reports an empty inline rollup with `biomarkerRollupState: not_computed`.
- The `codevetra_admin` `biomarkers-refresh` action is dry-run-first; execution
  requires `dryRun: false`, `confirm: true`, and optionally accepts
  `databaseQueryTimeoutMs` from 1 through 300000 as the native operation
  deadline. `timeoutMs` remains an exclusive legacy alias.
- Results name the effective timeout and its source; the MCP client deadline
  always bounds the native operation, including its inner deadline.

### Dead code and digest

`codevetra_dead_code` materializes its deterministic, exempted
`maxCandidates` orphan window before bounded edge/source enrichment. A database
statement timeout is a typed `dead_code_query_timeout`. `codevetra_digest`
isolates all five concurrent sections: `sectionStatus` records `ready`,
`timeout`, or `unavailable`, safe fallback values replace only failed sections,
and `degraded` states whether any section was incomplete.

### Find content mode

`codevetra_find` content mode treats `query` as a bounded Rust regular
expression. Its optional `pathFilter` is a case-sensitive literal substring of
the normalized project-relative path, not a glob or another regular
expression; a basename such as `lib.rs`, a directory segment, or a longer path
fragment therefore scopes the scanned inventory directly. Invalid expressions
return a safe parser category and zero-based byte offset without echoing the
query text.

### Node and graph batches

`codevetra_node` accepts either one `symbol` or a batch of up to 20 exact
`symbols`. A batch preserves every resolvable result, lists missing inputs in
`unresolved`, and lists each ambiguous input in `ambiguous` with up to ten
candidate identities and an explicit `truncated` flag. A single-symbol request
continues to fail closed when its name is ambiguous.

Traversal `codevetra_graph` batches use the same bounded candidate diagnostics
and retain every uniquely resolved root. `resolutionState` is `complete` or
`partial`; an entirely unresolved batch returns `unresolved` with `graph: null`
and instructions to select an exact candidate identity. Path and similarity
queries continue to require unambiguous endpoints.

File-based `codevetra_affected` and `codevetra_tests_for` preserve admitted tests
when traversal is partial. `countsComplete: false` labels incomplete counts.
If the node budget runs out before any test is found, they return
`truncated_before_any_result` with guidance to increase `maxNodes` or split the
input files. A truncated empty search does not establish that no tests are
affected.

### Context intents, Jev ranking, and the live overlay

`codevetra_context` classifies deterministic task intents such as symbol
lookup, implementation trace, change planning, test selection, error diagnosis,
architecture survey, and documentation lookup. Intent selects bounded candidate,
graph, evidence, and affected-test policy and is returned in the packet.

When the project's Jev decision tier lists the `context` feature, the packet's
BM25/semantic candidates are reordered by one metadata-only relevance request
(no source is sent). Anchors and graph expansion keep their positions; judged
items carry an advisory `decision_relevance`, primary edit candidates follow the
relevant judged files (basis `decision_relevance`), and `decision_rank` reports
the model, outcome and judged count in every output format. A provider failure
rebuilds the packet through the configured reranker.
`mode: deterministic` never consults the provider.

Required context failures return `context_retrieval_failed` in text and
`structuredContent.error`, with a redacted `stage`, `cause`, `retryable`, and
`nextAction`. Generation changes, storage timeouts, unavailable storage, invalid
requests, and invalid evidence remain distinct. These diagnostics omit source,
query text, credentials, driver messages, and absolute checkout paths.

When the durable generation is stale, supported changed/untracked files may
contribute a separate live working-tree overlay. Overlay items include path,
Git change kind, exact content digest, line-bounded excerpt, matched terms, and
truncation. They are never relabeled as durable graph evidence.

## Reliable agent loop

1. `codevetra_status`.
2. `codevetra_context` with the concrete coding task and any known exact
   name/path/reference anchors.
3. Orient with `entry_points`, then focus with `files`, `at_range`, `find`,
   `node`, or `graph`; use
   `direction: path` with `toSymbolId` when the question is how two exact
   symbols connect; use `direction: similar` for stored-vector peers and keep
   the returned model ID and score provenance, then read the exact files before
   editing.
4. Make the change.
5. `codevetra_review` against the intended base and `codevetra_affected` for
   verification candidates.
6. Run the project's actual formatter, linter, type, test, and security gates.
7. Re-index only when current graph evidence is needed after source changes.

The modern discovery result and legacy initialize response contain the compact
version of this loop. Call `codevetra_playbook` for the complete on-demand
guide, or run `codevetra guide` outside MCP.

> [!IMPORTANT]
> Always preserve generation ID, freshness, confidence, abstention, component
> ranks, coarse reference precision, multiplicity, truncation, and overlay
> status in downstream reasoning. A candidate is evidence to inspect, not proof
> that a change is correct.

## Transport contract

The server uses newline-delimited JSON-RPC over stdio and writes no diagnostic
text to stdout. It enforces:

- bounded input and serialized output bytes;
- bounded concurrent requests;
- a hard wall-clock request deadline;
- cancellation with worker abort/reaping;
- stable public error codes and redacted internal failures;
- deterministic tool/schema ordering;
- dual-era modern per-request metadata and legacy initialization;
- modern private TTL caching without connection-dependent tool mutation.

> [!WARNING]
> Do not retry by removing bounds or wrapping the server with an unbounded
> queue.

### Long-running admin jobs

For long index work, use `codevetra_admin` to start a job and poll status;
cancel explicitly when the host/user abandons it. While an index job is running,
its job view includes a live `progress` packet with:

- the current stage;
- monotonic completed items/bytes;
- heartbeat count and progress-idle time;
- completed-stage timings and total elapsed time;
- cancellation state.

These counters expose actual work without source paths, source text, SQL, or
database settings.

### Deferred retention

Index reports retain graph, parse-cache, and auxiliary maintenance outcomes.
Busy snapshots and finite row/byte/time budgets may defer work for a later
batch. Phase errors remain visible and contribute to maintenance failure
telemetry; successful publication does not imply successful cleanup. Recheck
usage and reports before requesting another bounded prune. Never steal a live
writer or delete native lock files.

### SCIP interchange jobs

SCIP interchange is also job-based. Use `action: "scip-export"` with a
project-relative `out`, or `action: "scip-import"` with a project-relative
`in`. Import persists a digest-fenced overlay, forces indexing, preserves files
the SCIP artifact does not cover, and reports exact typed-edge versus unresolved
foreign-link counts. The browser visualizer is intentionally absent; graph and
interchange data remain available to agents.

### When the transport closes

If the MCP transport closes, report that limitation and use the equivalent
native CLI as a control path. CLI success alone does not prove the host's MCP
registration or running process was refreshed.

## Modernization order

The next protocol work should be delivered in this order:

1. Add exact `outputSchema` contracts and validate every emitted
   `structuredContent` payload, starting with status, find/context, review, and
   admin job envelopes.
2. Expose immutable-generation resources and resource links for source windows,
   evidence packets, schemas, and durable artifacts. Resource URIs must carry
   project/generation identity and preserve freshness and privacy boundaries.
3. Map durable `codevetra_admin` jobs to the modern
   `io.modelcontextprotocol/tasks` extension with bounded TTL, polling,
   cooperative cancellation, and result retrieval; retain synchronous fallback
   for clients that do not advertise the extension.
4. Add rate-limited `notifications/progress` only for requests that provide a
   progress token, with monotonic phase/total evidence and no query/source text.
5. Add a small prompt surface for user-selected review, diagnosis, and change
   planning workflows where host support makes prompts materially useful.
6. Use multi-round-trip elicitation for non-secret operator choices and exact
   destructive confirmations only when the client advertises it. Credentials,
   tokens, and passwords must never use form elicitation.
7. Consider Streamable HTTP, OAuth, and routing headers only for an explicitly
   authorized remote/hosted product. Local CodeVetra remains private stdio by
   default.

Do not newly build on MCP roots, client sampling, or protocol logging. They are
deprecated in `2026-07-28`; CodeVetra already has explicit project ownership,
direct bounded LLM clients, stderr for local diagnostics, and can add
OpenTelemetry outside the model-facing protocol when structured observability
is needed.

Protocol references: [versioning and dual-era compatibility](https://modelcontextprotocol.io/specification/2026-07-28/basic/versioning),
[modern tools and stable catalogs](https://modelcontextprotocol.io/specification/2026-07-28/server/tools),
[tasks extension](https://modelcontextprotocol.io/extensions/tasks/overview), and
[deprecated features](https://modelcontextprotocol.io/seps/2577-deprecate-roots-sampling-and-logging).

## Cursor navigation and symbol references

`codevetra navigate definition --file src/app.ts --line 12 --column 8`
uses the same contract as `codevetra_navigate` with `operation`, `file`, `line`
and `column`. Lines start at one; columns start at zero and count **UTF-8 bytes**,
not UTF-16 code units. CRLF terminators are excluded from columns. A column in
the middle of a multibyte character is invalid. Occurrence byte ranges are
start-inclusive and end-exclusive; a cursor at the end of a token does not
select that token.

Use `navigate references --symbol-id <ID> --limit 50` after resolution, or
pass the same cursor anchor. Results only include references whose resolved
target is that identity. Definitions and usage spans remain separate; coarse
legacy/SCIP anchors retain their precision and represented site count and never
identify a cursor token by containment alone. Ambiguous resolution returns its
candidate definitions without starting a reference query. Whitespace/comments
have no exact occurrence; an enclosing function is never a replacement target.

`navigate implementation` follows indexed `implements` and `overrides`
relationships, preserving confidence and provenance. Without those relationships
the result explicitly reports unsupported evidence, not proof that no
implementation exists. Resolution depends on the native or imported SCIP
language evidence; unresolved receivers and overloads remain unresolved or
ambiguous.

Reference pages return at most 100 matches and inspect at most 16,384 retained
records per call. A short or empty page can still have `next`; continue with
`--after '<next>'` and the same anchor. The continuation binds the canonical
project, generation and selector. A changed generation rejects it; restart the
lookup. Results include generation, freshness and explicit truncation. Stale
source yields a `stale` disposition and no cursor results. Ordinary source
retrieval remains available through `files` and `node`.

Exploration diagnostics preserve lexical failures when no semantic channel can
answer. Native capacity, stored-record, candidate-metadata, candidate-ranking
and channel-identity failures have distinct source-free cause codes. Check the
code, stage and generation alongside read-only status/doctor before choosing a
repair; an otherwise healthy store can still reject a particular bounded query.

## Source body allocation

Context, exploration and context review share `sourceDetail` (`--source-detail`):
`adaptive` keeps task-selected bodies and their evidenced call-path ancestors,
`full` attempts every selected body, `focused` requests bounded prefixes, and
`outline` returns indexed identities/signatures. Selection uses resolved IDs;
a shared method name does not promote every implementation. Multiple relevant
symbols in the same file remain eligible. Existing `code`, `summary`, and
`lowTokens` source-suppression behavior still applies.

Each source window reports `representation`: `full`, `focused`, `outline`,
`truncated`, or `unavailable`, its indexed file content hash, and half-open UTF-8
`omitted_ranges`. An outline is metadata, not transformed source presented as a
complete body. Verbatim excerpts now carry exact file-byte bounds as well as
line ranges. A focused prefix does not establish that omitted guards or cleanup
were inspected.

Context admits a 128 KiB source-allocation budget; exploration and context review
admit 512 KiB. A body capture has a separate 256 KiB ceiling (8 KiB for focused
prefixes). Selection and allocation are deterministic and require no model.
Accounting measures serialized allocation JSON, including escaped source,
metadata and outlines; it is distinct from the enclosing evidence packet and
MCP text/structured envelope. Protocol and owner output limits remain enforced.
If a required body cannot fit, `incompletePriority` names its identity and
`requiredBodiesComplete` is false. No review should interpret an omitted body as safe.
Review source is labeled as the indexed live generation or the verified patch
base; a supplied patch's post-image is not mislabeled as indexed source.

Expand a body with `codevetra node --symbol-id <ID> --detail full --max-bytes
262144`; use `files --format read --file <path> --line-offset <offset>
--line-limit 500` for subsequent line windows. The allocation's completeness
concerns its selected source bodies; graph/review truncation and unresolved
relationships remain separate limitations.

### Explicit source reuse and byte recovery

`context`, `explore`, context `review`, `node`, and `files` accept an optional
`sourceReuse` mode. Ordinary calls retain their normal source behavior. Opted-in
calls use JSON and account for the complete serialized MCP `ToolResult`, including
both text and structured content; the transport envelope remains separately
bounded. Source-allocation accounting describes allocation before this projection.

1. Request `sourceReuse: "offer"` to receive source plus `sourceHandle` identifiers.
2. On another call, pass `sourceReuse: "reuse"` and `knownSources: ["<handle>"]`
   only for exact source text the current client still holds. Unchanged exact
   ranges may become explicit `sourceReference` objects. New or overlapping but
   unequal ranges are returned normally. Small excerpts remain inline when the
   reference would cost more bytes.
3. After context compaction, reconnect, or transfer to another agent, request
   `sourceReuse: "full"` or `"reset"`, without relying on old acknowledgements.
   Both ignore acknowledgements and emit requested source within normal bounds.
4. Recover an individual handle using
   `files {"format":"read","sourceHandle":"<handle>"}`. Recovery revalidates
   canonical project, generation, content hash, exact half-open UTF-8 byte range,
   and text digest. Expired handles can be renewed through this full read only
   while those identities still match. Changed or evicted generations require a
   current `files` read by path; old coordinates are never silently moved.

Handles are bounded source identifiers, not authorization credentials. The same
project access checks apply to recovery. A call accepts/offers at most 64 handles,
with 8 KiB per handle and a 30-minute acknowledgement lifetime. The daemon retains
**zero consumption history**: two clients cannot suppress each other's source,
failed/capped responses cannot mark source as delivered, and project notes and
sessions do not store this transient state. Only fresh, exact excerpts surviving
final line/byte projection receive handles. Approximate live source, outlines,
and unavailable source do not.

`newSourceBytes` and `reusedSourceBytes` count eligible excerpt UTF-8 bytes;
`emittedResultBytes` counts actual JSON bytes including handles and reference
metadata. `totalResponseBytesSaved`/`Added` compare against the original result
(including its original rendering), so reduced source duplication and reduced
whole-response size remain separate. The opted-in result ceiling is 1.5 MiB;
exceeding it returns an explicit error and retains no history. Reduce symbols or
window size and retry. A source reference asserts client possession, never that
an omitted implementation was inspected or found safe.

For precise recovery of an omitted body or a very long single line:

```sh
codevetra files --format read --file src/example.ts --byte-offset 0 --byte-limit 65536
```

Continue from `excerpt.endByte`. Byte windows are at most 256 KiB and are mutually
exclusive with line windows. The starting offset must be a UTF-8 boundary; the
end rounds down to a boundary. These are current indexed file bytes, not patch
post-image coordinates. CLI equivalents are `--source-reuse`, `--known-sources`,
and `--source-handle`.

### Possible static execution paths

`codevetra_navigate {"operation":"trace","symbolId":"<exact-id>"}` (CLI:
`codevetra navigate trace --symbol-id <id>`) returns a bounded source-backed
trace for JavaScript, JSX, TypeScript and TSX callable bodies. Resolve the entry
identity with `find`/`node` first. `focusSymbolId` marks an exact changed/target
identity without removing surrounding guards, error paths or cleanup.

The result is **possible static execution paths**, never an observed execution
or proof of branch feasibility. `functions` is a definition table, not one linear
journey. Each function's ordered `syntax.steps` has local `id`/`parent` links,
exact half-open UTF-8 byte spans, one-based lines, a bounded verbatim preview and
an explicit preview-truncation flag. These links preserve nested guards, cases,
loops, catch paths and finally cleanup. Source order does not establish expression
evaluation order, loop iteration counts, or ordering between concurrent tasks.
Return/throw/break/continue remain explicit abrupt exits; statements later in the
source are not assertions that execution reaches them.

`calls` attaches exact indexed occurrence evidence, including resolver confidence
and provenance. A unique exact target can link to another entry in `functions`;
`expanded` says whether that source is present. Repeated definitions and recursion
stop at explicit table links instead of unrolling indefinitely. Unresolved,
ambiguous, coarse and truncated occurrence scans never select an arbitrary target.
An awaited call is labeled separately. Callback declarations are deferred and
not executed inline by this trace. Scheduler bindings, callback invocation order,
dynamic dispatch and unsupported constructs remain unknown.

The initial slice covers conditionals, switch cases (with possible fallthrough),
loops, abrupt exits, await and try/catch/finally. Other languages return explicit
unsupported coverage. Limits are 512 syntax steps total, 16 definitions, call
depth 4, syntax depth 24, 64 resolved call sites per definition, 256 KiB source per
body, and 16–512 KiB serialized report JSON. Defaults are 128 steps, call depth 2,
and 128 KiB. Step/depth/byte limits return explicit partial evidence. Cancellation
uses the request's native-worker cancellation and reaping boundary. A changed
source manifest returns no trace; a publication race fails instead of mixing
generations. Review/context clients can use the same structured report and concise
text summary for a selected changed function; graph depth is never substituted
for statement order.

### Application boundaries and navigation

Graph callers/callees/path/impact and affected-test selection include `http_request`,
`queue_dispatch`, `event_dispatch`, and `navigates` relations by default. An explicit
`edgeKind: "calls"` still requests only call relations. Boundary resource nodes retain
channel, operation, adapter, scope, destination, and generation-owned resolution state.
The path is owner → source site → registration → handler. Named callback bindings may
use an ordinary call edge on the final hop; edge provenance distinguishes that binding
from the inferred boundary. Coordinates are original UTF-8 byte ranges and one-based
lines, including route registration and navigation sites. These are possible static
relationships, not observed execution, delivery guarantees, path feasibility, or test
coverage. A reverse impact query finds evidenced sources and their callers.

| Adapter | Supported evidence | Explicit limits and unsupported forms |
| --- | --- | --- |
| `next-app-v1` HTTP | Client-module literal `fetch` methods; imported Axios constant client created with exact `baseURL: '/'`; exported App Router HTTP handlers | Same application only; per-call Axios configuration, server-relative requests, arbitrary bases, rewrites and Next configuration abstain |
| `next-app-v1` navigation | Imported Link, anchor href, imported router push/replace and redirects; default App Router page declarations; route groups and simple `[parameter]` segments | Catch-all/parallel routes, unknown configuration, computed/external paths and ambiguous routes do not resolve |
| `react-router-jsx-v1` | Imported BrowserRouter/Routes/Route, Link/NavLink/Navigate, locally bound useNavigate; nested literal paths, literal basename, simple parameters; imported `lazy(() => import('./Screen'))` to an executable default export | Explicit JSX router instance; source must be inside that router or a uniquely registered local function component. Multiple scopes, data routers, cross-file source-component propagation, relative navigation, optional/splat/named routes, spreads and dynamic attributes abstain |
| `node-events-v1` | Imported EventEmitter constant instance, literal emit/on/once/addListener and exact callback binding | Same receiver instance and lexical scope; reassignment, aliases, cross-file identity and dynamic names unproved |
| `bull-instance-v1` | Imported Bull constant Queue instance, named add/process jobs | Both instance and job name must match; BullMQ and separate Redis-backed instances are not inferred |
| `set-observer-v1` | Native constant Set, add(callback), narrow forEach callback invocation | No custom observer abstractions, alias propagation or arbitrary iteration inference |

`boundaryCoverage` accompanies graph/context/explore/review/affected cursor packets.
Resolution states distinguish unknown identity, external destinations, absent supported
registrations, ambiguous registrations, routing configuration and fan-out limits.
Publication derives sites and edges together; editing either endpoint re-derives them.
There are at most 512 optional sites per file, 256 receiver bindings, 64 JSX routers,
8 event subscribers and 64 parameterized route patterns per application/method group.
Overflow abstains or reports the existing optional-facts omission diagnostic; empty
results never imply complete runtime coverage. Vue/Nuxt, SvelteKit, Angular, Expo and
TanStack navigation joins remain unsupported; their existing route facts are separate
from a proven navigation connection. Adapter extensions must supply scoped registration,
source evidence, matching semantics and independent negative fixtures.

For example, use `codevetra_graph` with `start: "Checkout"`, `direction: "callees"`
and `depth: 6`, or reverse `direction: "impact"` from a handler or screen. Path queries
retain intermediate source/registration nodes, confidence and provenance. Review and
context evidence retains boundary metadata on admitted nodes; normal budgets still
apply. Expand an omitted path explicitly rather than interpreting omission as absence.
Static traces attach exact boundary sites to each selected body without placing the
subscriber/worker in the caller's execution order. Syntax cancellation retains a
partial prefix. Agent-level cancellation retains completed definitions with `partial`
set and `fresh` false because the final generation check was not completed. Transport
cancellation returns the protocol's cancelled request after native work is reaped;
clients must not interpret either outcome as a complete trace.
