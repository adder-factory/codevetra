# Cartograph → CodeVetra

CodeVetra 4 changes the product name, repository, executable, Rust packages, MCP
registration and tools. It preserves the native RocksDB durable format and project
identity. The new generation contract is V27; re-index source after upgrading so
operation-local biomarkers and application boundaries are current. Old generations,
configuration, exclusions, SCIP overlays and mutable user state are preserved.

## Existing installations

1. Install the `codevetra` executable using the new installer/release archive. Retire old
   launchers and update pinned host commands before moving state. Preserve old
   release archives separately if needed.
2. Close the project's old MCP host/watch service. Migration refuses a live native
   primary or configuration writer; it never removes locks or stops an owner.
3. Run `codevetra migrate-name --project-path /absolute/project --format json`.
4. Run `codevetra index /absolute/project`, then `codevetra doctor /absolute/project
   --format json` and `codevetra status /absolute/project --format json`.
5. Run `codevetra install --yes --target codex --location local --project-path
   /absolute/project` (or the relevant host). Restart the host and verify one
   `codevetra` registration, `codevetra_*` tools, a fresh generation, and real
   context/find/graph/review/biomarker results.

Starting with v4.0.3, migration renames the existing physical directory to
`.codevetra`. It also converts the `.codevetra -> .cartograph` layout created by
v4.0.0–v4.0.2. Directory identity, permissions, database contents and original
owner/configuration locks are preserved; the database is neither copied nor rebuilt.
The command refuses an independent canonical directory. On macOS/Linux it holds
both original locks throughout the move. On Windows it first acquires a process-shared
state-admission guard and verifies both original file locks. It then closes those
handles for the directory move while the admission guard prevents new native owners
and configuration writers from resolving state. Existing owners still block migration. No symlink privilege is needed. Fresh projects create only
`.codevetra`; existing exact legacy marker files are removed during migration.

Old binaries that only understand `.cartograph` must no longer run against a migrated
project: without the legacy directory or marker, they could initialize independent
state. Update managed registrations and services, then restart hosts using the new
executable. Never bypass ownership by removing locks.

Root `.cartographignore` patterns and permissions are copied atomically to
`.codevetraignore`, then the verified equal legacy file is removed. Differing files
are a conflict. Nested legacy ignore files remain supported when no canonical file
overrides them. Both state directory names remain excluded from source discovery.
Provider identifiers and `apiKeyEnv` references retain their exact stored values.

If interrupted after removing an old alias, moving state, or publishing the canonical
ignore file, rerun the same command. It resumes from the preserved directory while
checking the original locks and ignore contents. Do not create a second index to
recover. Downgrading to a pre-CodeVetra executable requires stopping every owner,
explicitly moving the preserved state back, and restoring matching registrations;
a newer generation may require a source rebuild. Preserve canonical ignore changes
and retained state when assessing a downgrade.

## Physically moved project directories

Product-name migration does not change the canonical checkout path bound to an
existing native index. A directory move can therefore make the index unavailable;
an old-name symlink does not restore its original canonical path.

Builds with the `relocate` command support an explicit identity-preserving move:

```sh
codevetra migrate-name --project-path /absolute/new-project --format json
codevetra relocate --project-path /absolute/new-project \
  --from-project-path /absolute/previous-project --format json
codevetra index /absolute/new-project
codevetra doctor /absolute/new-project --format json
```

Stop the project's native owner first. Supply the previous **canonical** absolute
path as it was before the move. The operation refuses an independent original that
still exists, a live owner, incompatible native state, or concurrent configuration
changes. It validates the existing database with its original identity and records
an atomic `nativeRootIdentity` binding in project configuration. Only digests are
stored in that binding. The database, project ID, graph IDs, notes, sessions and
embeddings retain their identities. Opening a copied or subsequently moved binding
at another path still fails until another explicit relocation is performed.

Repeating the same completed relocation is a no-op. Reinstall registrations and
restart hosts against the new directory, then verify fresh status and real queries.
Older executables without relocation support cannot open the relocated index.

## Names and compatibility

| Surface | Canonical 4.x name | Legacy handling |
| --- | --- | --- |
| Repository | `adder-factory/codevetra` | GitHub repository rename retains history and redirects |
| CLI/archive binary | `codevetra`, `codevetra.exe` | Old installations remain available for rollback; update scripts/host commands explicitly |
| Rust packages/imports | `codevetra-*`, `codevetra_*` | Major-version source change |
| MCP server/tools | `codevetra`, `codevetra_*` | Canonical catalog only; reinstall/restart hosts and update saved tool calls |
| Project state | `.codevetra` | Physical rename; v4.0.0–v4.0.2 aliases are accepted as migration input |
| Ignore rules | `.codevetraignore` | Legacy fallback; root migration checks conflicts |
| Runtime environment | `CODEVETRA_*` | Corresponding `CARTOGRAPH_*` read only when canonical variable is absent; an empty/invalid canonical value never falls back |
| User models/caches | `~/.codevetra` | Run the explicit name migration with `--project-path "$HOME"` when `~/.cartograph` contains existing user state; do not index the home directory |
| Installation root | `~/.codevetra-cli` or Windows `codevetra-cli` | Preserve old installation until registrations and rollback needs are settled |

The standalone installers use the same canonical-before-legacy precedence.
Empty repository or installation-directory overrides are rejected before any
download or installation; the Unix installer also rejects an empty bin directory.
An empty version selects the latest release, and an empty checksum-skip setting
keeps checksum verification enabled.

The runtime environment compatibility window is 4.x. Third-party provider environment
names remain provider-owned. Legacy access restrictions are read through the same
fallback boundary. Installer migration keeps wrapper programs, extra server arguments,
environment and unrelated host entries. Independent old/new registrations are a
conflict, not an instruction to merge two configurations. Managed instruction markers
and permissions migrate together; deny/ask permissions are retained alongside allows.
For an old macOS login service, use the old executable's `service uninstall` after
closing the owner, then install the new CodeVetra service explicitly.

On Windows, `.codevetra.lock` is a persistent sibling admission file. It inherits
project directory access controls, rejects reparse points, and coordinates ordinary
state opens/config writers with physical migration. Never delete or replace it:
openers must share one lock identity. Ordinary callers wait within their bounded
operation/config deadline. Migration verifies both original state locks, closes
them because Windows prevents moving a directory with open descendants, and keeps
the sibling admission locked through the move. Retire legacy binaries before this
operation; they do not participate in this admission. The OS releases the lock on
process loss, even when another process already has the file open.

## Intentional old-name exceptions

- Frozen v1 captures, source corpora, exact identity/disposition evidence and historical
  benchmark/release/dependency records retain their original content and filenames.
- Existing hash domains for project/file/symbol/document identities, source manifests,
  model/cache fingerprints and digest versions V1–V26 retain their original bytes.
  V27 introduces its own new hash domain. Renaming a hash input would detach stored
  state from its identity and is not a branding change.
- Serialized resolver markers such as `cartograph.explicit-receiver::` and
  `cartograph.native-module-alias::`, the structural-role model identifier, and
  fixture fingerprint domains remain exact compatibility values. They are internal
  evidence identities, not public command or tool aliases.
- RocksDB format `cartograph-rocks-v1`, ownership marker `CARTOGRAPH_OWNER`, its header,
  merge-operator name, IPC root hash domain, and persisted SCIP root URIs are unchanged.
  Public crate/type names and protobuf Rust identifiers change; protobuf field numbers
  and durable values do not.
- Legacy environment fallback, migration parsing, installer recognition, marker tests,
  source-exclusion rules and the maintainer-only v1 oracle capture command explicitly
  retain Cartograph spellings. The verified v1 binary still uses its original commands.
- Local active v3 owners, private state, old installations, unmodified saved user
  configurations and historical issue/release records remain separate deployment state.

These exceptions are compatibility contracts, not additional storage engines or a
second product namespace for new installations.
