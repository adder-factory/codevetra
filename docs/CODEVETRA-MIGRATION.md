# Cartograph → CodeVetra

CodeVetra 4 changes the product name, repository, executable, Rust packages, MCP
registration and tools. It preserves the native RocksDB durable format and project
identity. The new generation contract is V27; re-index source after upgrading so
operation-local biomarkers and application boundaries are current. Old generations,
configuration, exclusions, SCIP overlays and mutable user state are preserved.

## Existing installations

1. Install the `codevetra` executable using the new installer/release archive. Keep
   the prior Cartograph executable and installation directory for rollback.
2. Close the project's old MCP host/watch service. Migration refuses a live native
   primary or configuration writer; it never removes locks or stops an owner.
3. Run `codevetra migrate-name --project-path /absolute/project --format json`.
4. Run `codevetra index /absolute/project`, then `codevetra doctor /absolute/project
   --format json` and `codevetra status /absolute/project --format json`.
5. Run `codevetra install --yes --target codex --location local --project-path
   /absolute/project` (or the relevant host). Restart the host and verify one
   `codevetra` registration, `codevetra_*` tools, a fresh generation, and real
   context/find/graph/review/biomarker results.

During the 4.x compatibility window, migrated projects use a relative `.codevetra`
link to their existing physical `.cartograph` directory. All project services resolve
only that exact migration alias; arbitrary state symlinks remain invalid. This keeps
one native owner lock and database throughout migration, including if an older binary
tries to reopen the project. No data directory is copied, moved, opened by the migration
command, or deleted. Canonical and legacy independent directories are a conflict;
resolve their ownership deliberately before retrying. On Windows, creating the alias
requires directory-symlink permission (Developer Mode or an appropriately privileged
installation). An unsuccessful alias creation leaves the legacy state intact.

Fresh projects create a real `.codevetra` directory and reserve `.cartograph` as a
small marker file. The marker prevents older executables from creating a second
state directory. Do not replace that marker with a directory or delete locks.

Root `.cartographignore` patterns are copied atomically to `.codevetraignore`; equal
files make repeated migration a no-op, and differing files are a conflict. Nested
legacy ignore files remain supported when no canonical file overrides them. Both
private state directory names are always excluded from discovery. Configured provider
identifiers and `apiKeyEnv` references retain their exact stored values.

If migration is interrupted after the alias is created, rerun the same command. It
checks the original owner/configuration locks and completes the remaining ignore-file
step without creating another database. To roll back an alias migration, first close
all hosts, remove only the `.codevetra` alias, restore the previous host registration
and executable, and use the untouched `.cartograph` state. Preserve any canonical
ignore changes separately. After V27 publication, an older executable may require its
own source rebuild; do not promise it can query a newer generation. Do not delete
retained state to force compatibility.

## Names and compatibility

| Surface | Canonical 4.x name | Legacy handling |
| --- | --- | --- |
| Repository | `adder-factory/codevetra` | GitHub repository rename retains history and redirects |
| CLI/archive binary | `codevetra`, `codevetra.exe` | Old installations remain available for rollback; update scripts/host commands explicitly |
| Rust packages/imports | `codevetra-*`, `codevetra_*` | Major-version source change |
| MCP server/tools | `codevetra`, `codevetra_*` | Canonical catalog only; reinstall/restart hosts and update saved tool calls |
| Project state | `.codevetra` | Explicit validated alias for migrated projects throughout 4.x |
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
