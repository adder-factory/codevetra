//! Explicit product-name migration preserves the directory and native owner lock identities.
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use thiserror::Error;

const CANONICAL: &str = ".codevetra";
const LEGACY: &str = ".cartograph";
const TOMBSTONE: &[u8] =
    b"CodeVetra state is in .codevetra; use codevetra instead of legacy Cartograph.\n";

/// Secret-safe outcomes for the explicit name transition.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum NameMigrationError {
    /// Legacy state must be acknowledged before a new executable can initialize.
    #[error(
        "legacy Cartograph state exists; stop its host and run codevetra migrate-name --project-path ."
    )]
    Required,
    /// Independent old and new state must never be silently selected or merged.
    #[error(
        "conflicting Cartograph/CodeVetra state or ignore files; preserve both and resolve the conflict before migration"
    )]
    Conflict,
    /// Another primary/configuration writer still holds the original shared lock.
    #[error(
        "project owner or configuration writer is active; close its host before name migration"
    )]
    Busy,
    /// An untrusted path or failed operation left existing state intact.
    #[error("name migration path is unavailable or unsafe; existing state was preserved")]
    UnsafePath,
}

/// Physical storage placement reported by name migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatePlacement {
    /// Fresh canonical state, or an uninitialized project with no legacy data.
    Canonical,
}

/// Migration changes names while preserving the physical state and its native lock.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NameMigrationReport {
    /// The canonical path is available or already migrated.
    pub completed: bool,
    /// Graph/config/overlay contents and the physical directory identity are preserved.
    pub state_preserved: bool,
    /// Completed migrations use a real canonical directory.
    pub placement: StatePlacement,
    /// Restart hosts after updating their managed registration with `codevetra install`.
    pub host_restart_required: bool,
}

/// Resolve only a real canonical directory or the exact migration-owned legacy alias.
/// # Errors
/// Rejects legacy-only state, independent dual directories, unexpected links and files.
pub fn project_state_directory(root: &Path) -> Result<PathBuf, NameMigrationError> {
    let canonical = root.join(CANONICAL);
    let legacy = root.join(LEGACY);
    let new = metadata(&canonical)?;
    let old = metadata(&legacy)?;
    if new
        .as_ref()
        .is_some_and(|meta| meta.file_type().is_symlink())
    {
        if fs::read_link(&canonical).map_err(|_| NameMigrationError::UnsafePath)?
            == Path::new(LEGACY)
            && old
                .as_ref()
                .is_some_and(|meta| meta.is_dir() && !meta.file_type().is_symlink())
        {
            return Ok(legacy);
        }
        return Err(NameMigrationError::UnsafePath);
    }
    if new.as_ref().is_some_and(|meta| !meta.is_dir()) {
        return Err(NameMigrationError::UnsafePath);
    }
    if let Some(old) = old {
        if old.is_file()
            && !old.file_type().is_symlink()
            && read_bounded(&legacy, 256)? == TOMBSTONE
        {
            return Ok(canonical);
        }
        return Err(if new.is_some() {
            NameMigrationError::Conflict
        } else {
            NameMigrationError::Required
        });
    }
    Ok(canonical)
}

/// Resolve canonical state without creating a legacy compatibility marker.
/// # Errors
/// Rejects conflicting state, unsafe aliases, and unmigrated legacy directories.
pub fn prepare_project_state(root: &Path) -> Result<PathBuf, NameMigrationError> {
    project_state_directory(root)
}

/// Move existing state into a real `.codevetra` directory while holding both
/// original locks. Existing migration aliases and exact legacy markers are retired.
/// Stop legacy hosts first; old binaries that only know `.cartograph` must no longer run.
/// # Errors
/// Refuses active owners, independent directories, unsafe paths and ignore conflicts.
pub fn migrate_project_name(root: &Path) -> Result<NameMigrationReport, NameMigrationError> {
    let root = fs::canonicalize(root).map_err(|_| NameMigrationError::UnsafePath)?;
    let legacy = root.join(LEGACY);
    let canonical = root.join(CANONICAL);
    let state = match project_state_directory(&root) {
        Ok(state) => state,
        Err(NameMigrationError::Required) => legacy.clone(),
        Err(error) => return Err(error),
    };
    let Some(meta) = metadata(&state)? else {
        IgnoreMigration::inspect(&root)?.finish(&root)?;
        retire_marker(&legacy)?;
        sync_directory(&root)?;
        return Ok(report());
    };
    if !meta.is_dir() || meta.file_type().is_symlink() {
        return Err(NameMigrationError::UnsafePath);
    }
    #[cfg(unix)]
    validate_owner(&meta)?;
    let _primary = lock(&state.join("native-primary.lock"))?;
    let _config = lock(&state.join("config.lock"))?;
    let ignore = IgnoreMigration::inspect(&root)?;
    if state == legacy {
        if metadata(&canonical)?.is_some() {
            if project_state_directory(&root)? != legacy {
                return Err(NameMigrationError::Conflict);
            }
            remove_alias(&canonical)?;
        }
        rename_exclusive(&legacy, &canonical)?;
    } else if metadata(&legacy)?.is_some() {
        retire_marker(&legacy)?;
    }
    ignore.finish(&root)?;
    sync_directory(&root)?;
    Ok(report())
}

fn retire_marker(path: &Path) -> Result<(), NameMigrationError> {
    if metadata(path)?.is_some() {
        if read_bounded(path, 256)? != TOMBSTONE {
            return Err(NameMigrationError::Conflict);
        }
        fs::remove_file(path).map_err(|_| NameMigrationError::UnsafePath)?;
    }
    Ok(())
}

fn report() -> NameMigrationReport {
    NameMigrationReport {
        completed: true,
        state_preserved: true,
        placement: StatePlacement::Canonical,
        host_restart_required: true,
    }
}

struct IgnoreMigration(Option<(Vec<u8>, fs::Permissions)>);
impl IgnoreMigration {
    fn inspect(root: &Path) -> Result<Self, NameMigrationError> {
        let old = root.join(".cartographignore");
        let new = root.join(".codevetraignore");
        let Some(meta) = metadata(&old)? else {
            return Ok(Self(None));
        };
        let bytes = read_bounded(&old, 1024 * 1024)?;
        if metadata(&new)?.is_some() && read_bounded(&new, 1024 * 1024)? != bytes {
            return Err(NameMigrationError::Conflict);
        }
        Ok(Self(Some((bytes, meta.permissions()))))
    }

    fn finish(self, root: &Path) -> Result<(), NameMigrationError> {
        let Some((bytes, permissions)) = self.0 else {
            return Ok(());
        };
        let old = root.join(".cartographignore");
        let new = root.join(".codevetraignore");
        if read_bounded(&old, 1024 * 1024)? != bytes {
            return Err(NameMigrationError::Conflict);
        }
        if metadata(&new)?.is_none() {
            match write_new_atomic(&new, &bytes, permissions) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(_) => return Err(NameMigrationError::UnsafePath),
            }
        }
        if read_bounded(&new, 1024 * 1024)? != bytes || read_bounded(&old, 1024 * 1024)? != bytes {
            return Err(NameMigrationError::Conflict);
        }
        fs::remove_file(&old).map_err(|_| NameMigrationError::UnsafePath)
    }
}

fn remove_alias(path: &Path) -> Result<(), NameMigrationError> {
    #[cfg(windows)]
    let result = fs::remove_dir(path);
    #[cfg(not(windows))]
    let result = fs::remove_file(path);
    result.map_err(|_| NameMigrationError::UnsafePath)
}

fn rename_exclusive(old: &Path, new: &Path) -> Result<(), NameMigrationError> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    let result = rustix::fs::renameat_with(
        rustix::fs::CWD,
        old,
        rustix::fs::CWD,
        new,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .map_err(std::io::Error::from);
    // Windows directory rename refuses an existing destination.
    #[cfg(windows)]
    let result = fs::rename(old, new);
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let result = {
        let _ = (old, new);
        Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
    };
    result.map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            NameMigrationError::Conflict
        } else {
            NameMigrationError::UnsafePath
        }
    })
}

fn sync_directory(path: &Path) -> Result<(), NameMigrationError> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| NameMigrationError::UnsafePath)?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}
fn metadata(path: &Path) -> Result<Option<fs::Metadata>, NameMigrationError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(NameMigrationError::UnsafePath),
    }
}
fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, NameMigrationError> {
    let meta = metadata(path)?.ok_or(NameMigrationError::UnsafePath)?;
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > limit {
        return Err(NameMigrationError::UnsafePath);
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| NameMigrationError::UnsafePath)?;
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| NameMigrationError::UnsafePath)?;
    if bytes.len() as u64 > limit {
        return Err(NameMigrationError::UnsafePath);
    }
    Ok(bytes)
}
fn write_new_atomic(
    path: &Path,
    bytes: &[u8],
    permissions: fs::Permissions,
) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file().set_permissions(permissions)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(path)
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

struct Lock(File);
impl Drop for Lock {
    fn drop(&mut self) {
        let _released = self.0.unlock();
    }
}
fn lock(path: &Path) -> Result<Lock, NameMigrationError> {
    if metadata(path)?.is_some_and(|meta| !meta.is_file() || meta.file_type().is_symlink()) {
        return Err(NameMigrationError::UnsafePath);
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .map_err(|_| NameMigrationError::UnsafePath)?;
    match file.try_lock() {
        Ok(()) => Ok(Lock(file)),
        Err(std::fs::TryLockError::WouldBlock) => Err(NameMigrationError::Busy),
        Err(_) => Err(NameMigrationError::UnsafePath),
    }
}
#[cfg(unix)]
fn validate_owner(meta: &fs::Metadata) -> Result<(), NameMigrationError> {
    use std::os::unix::fs::MetadataExt as _;
    // Compare to a descriptor owned by this process without unsafe getuid calls.
    let file = tempfile::tempfile().map_err(|_| NameMigrationError::UnsafePath)?;
    if file
        .metadata()
        .map_err(|_| NameMigrationError::UnsafePath)?
        .uid()
        != meta.uid()
    {
        return Err(NameMigrationError::UnsafePath);
    }
    Ok(())
}
#[cfg(all(test, unix))]
fn directory_alias(target: &Path, alias: &Path) -> Result<(), NameMigrationError> {
    std::os::unix::fs::symlink(target, alias).map_err(|_| NameMigrationError::UnsafePath)
}
#[cfg(all(test, windows))]
fn directory_alias(target: &Path, alias: &Path) -> Result<(), NameMigrationError> {
    std::os::windows::fs::symlink_dir(target, alias).map_err(|_| NameMigrationError::UnsafePath)
}

#[cfg(test)]
mod tests {
    use super::*;
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const PRESERVED_FILES: &[(&str, &[u8])] = &[
        ("config.json", b"{\"restricted\":true}"),
        ("data/current", b"retained-generation"),
        ("overlays/scip", b"retained-overlay"),
        ("native-primary.lock", b"original-primary-lock"),
        ("config.lock", b"original-config-lock"),
    ];

    #[test]
    fn migration_uses_a_real_canonical_directory_and_retires_legacy_paths() -> TestResult {
        let root = migration_fixture()?;
        let original = fs::metadata(root.path().join(LEGACY))?;
        let report = migrate_project_name(root.path())?;
        assert_eq!(report.placement, StatePlacement::Canonical);
        let canonical = root.path().join(CANONICAL);
        assert!(!fs::symlink_metadata(&canonical)?.file_type().is_symlink());
        assert!(metadata(&root.path().join(LEGACY))?.is_none());
        assert!(metadata(&root.path().join(".cartographignore"))?.is_none());
        for (path, bytes) in PRESERVED_FILES {
            assert_eq!(fs::read(canonical.join(path))?, *bytes);
        }
        assert_eq!(
            fs::read(root.path().join(".codevetraignore"))?,
            b"private/**\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let moved = fs::metadata(&canonical)?;
            assert_eq!(
                (original.dev(), original.ino(), original.mode()),
                (moved.dev(), moved.ino(), moved.mode())
            );
        }
        #[cfg(not(unix))]
        let _ = original;
        assert!(migrate_project_name(root.path())?.completed);
        Ok(())
    }

    #[test]
    fn fresh_state_never_creates_a_legacy_marker() -> TestResult {
        let root = tempfile::tempdir()?;
        assert_eq!(
            prepare_project_state(root.path())?,
            root.path().join(CANONICAL)
        );
        assert!(metadata(&root.path().join(LEGACY))?.is_none());
        Ok(())
    }

    #[test]
    fn interrupted_physical_migration_resumes_at_each_durable_boundary() -> TestResult {
        for renamed in [false, true] {
            let root = migration_fixture()?;
            directory_alias(Path::new(LEGACY), &root.path().join(CANONICAL))?;
            remove_alias(&root.path().join(CANONICAL))?;
            if renamed {
                rename_exclusive(&root.path().join(LEGACY), &root.path().join(CANONICAL))?;
            }
            migrate_project_name(root.path())?;
            assert_preserved(root.path())?;
            assert!(metadata(&root.path().join(LEGACY))?.is_none());
            assert!(metadata(&root.path().join(".cartographignore"))?.is_none());
        }
        Ok(())
    }

    #[test]
    fn canonical_owner_blocks_marker_retirement_and_ignore_completion() -> TestResult {
        let root = migration_fixture()?;
        rename_exclusive(&root.path().join(LEGACY), &root.path().join(CANONICAL))?;
        fs::write(root.path().join(LEGACY), TOMBSTONE)?;
        let owner = lock(&root.path().join(CANONICAL).join("native-primary.lock"))?;
        assert!(matches!(
            migrate_project_name(root.path()),
            Err(NameMigrationError::Busy)
        ));
        assert_eq!(fs::read(root.path().join(LEGACY))?, TOMBSTONE);
        assert!(metadata(&root.path().join(".codevetraignore"))?.is_none());
        drop(owner);
        migrate_project_name(root.path())?;
        assert_preserved(root.path())?;
        assert!(metadata(&root.path().join(LEGACY))?.is_none());
        Ok(())
    }

    #[test]
    fn ignore_only_migration_preserves_permissions_and_retires_only_exact_markers() -> TestResult {
        let root = tempfile::tempdir()?;
        fs::write(root.path().join(LEGACY), b"unrecognized legacy file")?;
        assert!(migrate_project_name(root.path()).is_err());
        assert_eq!(
            fs::read(root.path().join(LEGACY))?,
            b"unrecognized legacy file"
        );
        fs::write(root.path().join(LEGACY), TOMBSTONE)?;
        let old_ignore = root.path().join(".cartographignore");
        fs::write(&old_ignore, b"private/**\n")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&old_ignore, fs::Permissions::from_mode(0o640))?;
        }
        migrate_project_name(root.path())?;
        assert!(metadata(&root.path().join(LEGACY))?.is_none());
        assert!(metadata(&old_ignore)?.is_none());
        assert!(metadata(&root.path().join(CANONICAL))?.is_none());
        assert_eq!(
            fs::read(root.path().join(".codevetraignore"))?,
            b"private/**\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                fs::metadata(root.path().join(".codevetraignore"))?
                    .permissions()
                    .mode()
                    & 0o777,
                0o640
            );
        }
        Ok(())
    }

    #[test]
    fn exclusive_rename_and_ignore_publication_preserve_concurrent_destinations() -> TestResult {
        let root = migration_fixture()?;
        let pending = IgnoreMigration::inspect(root.path())?;
        fs::write(root.path().join(".codevetraignore"), b"concurrent policy")?;
        assert_eq!(
            pending.finish(root.path()),
            Err(NameMigrationError::Conflict)
        );
        assert_eq!(
            fs::read(root.path().join(".codevetraignore"))?,
            b"concurrent policy"
        );
        fs::create_dir(root.path().join(CANONICAL))?;
        assert!(rename_exclusive(&root.path().join(LEGACY), &root.path().join(CANONICAL)).is_err());
        assert_preserved(root.path())?;
        Ok(())
    }

    fn migration_fixture() -> Result<tempfile::TempDir, Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let state = root.path().join(LEGACY);
        fs::create_dir_all(state.join("data"))?;
        fs::create_dir_all(state.join("overlays"))?;
        for (path, bytes) in PRESERVED_FILES {
            fs::write(state.join(path), bytes)?;
        }
        fs::write(root.path().join(".cartographignore"), b"private/**\n")?;
        Ok(root)
    }

    fn assert_preserved(root: &Path) -> TestResult {
        let state = if root.join(LEGACY).is_dir() {
            root.join(LEGACY)
        } else {
            root.join(CANONICAL)
        };
        for (path, bytes) in PRESERVED_FILES {
            assert_eq!(fs::read(state.join(path))?, *bytes);
        }
        let ignore = if root.join(".cartographignore").is_file() {
            ".cartographignore"
        } else {
            ".codevetraignore"
        };
        assert_eq!(fs::read(root.join(ignore))?, b"private/**\n");
        Ok(())
    }

    #[test]
    fn configuration_writer_blocks_migration_without_changing_state() -> TestResult {
        let root = migration_fixture()?;
        let writer = lock(&root.path().join(LEGACY).join("config.lock"))?;
        assert!(matches!(
            migrate_project_name(root.path()),
            Err(NameMigrationError::Busy)
        ));
        assert_preserved(root.path())?;
        assert!(metadata(&root.path().join(CANONICAL))?.is_none());
        assert!(metadata(&root.path().join(".codevetraignore"))?.is_none());
        // A failed attempt also releases the primary lock it acquired first.
        drop(lock(&root.path().join(LEGACY).join("native-primary.lock"))?);
        drop(writer);
        assert!(migrate_project_name(root.path())?.completed);
        assert_preserved(root.path())
    }

    #[test]
    fn ignore_conflict_preserves_both_policies_and_never_creates_an_alias() -> TestResult {
        let root = migration_fixture()?;
        fs::write(root.path().join(".codevetraignore"), b"different/**\n")?;
        assert!(matches!(
            migrate_project_name(root.path()),
            Err(NameMigrationError::Conflict)
        ));
        assert_preserved(root.path())?;
        assert_eq!(
            fs::read(root.path().join(".codevetraignore"))?,
            b"different/**\n"
        );
        assert!(metadata(&root.path().join(CANONICAL))?.is_none());
        drop(lock(&root.path().join(LEGACY).join("native-primary.lock"))?);
        drop(lock(&root.path().join(LEGACY).join("config.lock"))?);
        Ok(())
    }

    #[test]
    fn interrupted_alias_creation_resumes_ignore_copy_and_shares_both_locks() -> TestResult {
        let root = migration_fixture()?;
        // Reproduce the durable state after alias creation but before ignore copy.
        directory_alias(Path::new(LEGACY), &root.path().join(CANONICAL))?;
        assert!(metadata(&root.path().join(".codevetraignore"))?.is_none());
        for _ in 0..2 {
            assert_eq!(
                migrate_project_name(root.path())?.placement,
                StatePlacement::Canonical
            );
            assert_preserved(root.path())?;
            assert_eq!(
                fs::read(root.path().join(".codevetraignore"))?,
                b"private/**\n"
            );
            assert!(
                !fs::symlink_metadata(root.path().join(CANONICAL))?
                    .file_type()
                    .is_symlink()
            );
            assert!(metadata(&root.path().join(LEGACY))?.is_none());
        }
        for name in ["native-primary.lock", "config.lock"] {
            let original = lock(&root.path().join(CANONICAL).join(name))?;
            assert!(matches!(
                lock(&root.path().join(CANONICAL).join(name)),
                Err(NameMigrationError::Busy)
            ));
            drop(original);
        }
        Ok(())
    }

    #[test]
    fn unexpected_aliases_never_select_or_modify_another_state_directory() -> TestResult {
        let root = migration_fixture()?;
        let other = migration_fixture()?;
        directory_alias(&other.path().join(LEGACY), &root.path().join(CANONICAL))?;
        assert_eq!(
            project_state_directory(root.path()),
            Err(NameMigrationError::UnsafePath)
        );
        assert!(matches!(
            migrate_project_name(root.path()),
            Err(NameMigrationError::UnsafePath)
        ));
        assert_preserved(root.path())?;
        assert_preserved(other.path())?;
        assert!(metadata(&root.path().join(".codevetraignore"))?.is_none());

        let linked_legacy = tempfile::tempdir()?;
        directory_alias(
            &other.path().join(LEGACY),
            &linked_legacy.path().join(LEGACY),
        )?;
        assert!(matches!(
            migrate_project_name(linked_legacy.path()),
            Err(NameMigrationError::UnsafePath)
        ));
        assert!(metadata(&linked_legacy.path().join(CANONICAL))?.is_none());
        assert_preserved(other.path())
    }

    #[cfg(unix)]
    #[test]
    fn owner_validation_rejects_foreign_metadata_without_mutating_it() -> TestResult {
        use std::os::unix::fs::MetadataExt as _;
        let foreign = fs::metadata("/")?;
        let owned = tempfile::tempdir()?;
        let current = fs::metadata(owned.path())?;
        // Root-run test environments cannot construct a foreign owner without
        // privileged mutation. Ordinary macOS and Linux CI users exercise refusal.
        if current.uid() != foreign.uid() {
            assert_eq!(
                validate_owner(&foreign),
                Err(NameMigrationError::UnsafePath)
            );
            assert_eq!(fs::metadata("/")?.uid(), foreign.uid());
        }
        assert_eq!(validate_owner(&current), Ok(()));
        Ok(())
    }

    #[test]
    fn legacy_migration_preserves_state_is_idempotent_and_refuses_owners() -> TestResult {
        let root = tempfile::tempdir()?;
        let state = root.path().join(LEGACY);
        fs::create_dir(&state)?;
        fs::write(state.join("config.json"), b"{\"restricted\":true}")?;
        fs::write(root.path().join(".cartographignore"), b"private/**\n")?;
        assert_eq!(
            project_state_directory(root.path()),
            Err(NameMigrationError::Required)
        );
        let owner = lock(&state.join("native-primary.lock"))?;
        assert!(matches!(
            migrate_project_name(root.path()),
            Err(NameMigrationError::Busy)
        ));
        assert!(!root.path().join(CANONICAL).exists());
        drop(owner);
        assert_eq!(
            migrate_project_name(root.path())?.placement,
            StatePlacement::Canonical
        );
        assert!(migrate_project_name(root.path())?.completed);
        assert_eq!(
            project_state_directory(root.path())?,
            root.path().join(CANONICAL)
        );
        assert_eq!(
            fs::read(root.path().join(CANONICAL).join("config.json"))?,
            b"{\"restricted\":true}"
        );
        assert_eq!(
            fs::read(root.path().join(".codevetraignore"))?,
            b"private/**\n"
        );
        let owner = lock(&root.path().join(CANONICAL).join("native-primary.lock"))?;
        assert!(matches!(
            lock(&root.path().join(CANONICAL).join("native-primary.lock")),
            Err(NameMigrationError::Busy)
        ));
        drop(owner);
        Ok(())
    }
    #[test]
    fn fresh_state_uses_only_canonical_name_and_conflicts_never_merge() -> TestResult {
        let root = tempfile::tempdir()?;
        assert_eq!(
            prepare_project_state(root.path())?,
            root.path().join(CANONICAL)
        );
        assert!(metadata(&root.path().join(LEGACY))?.is_none());
        let conflict = tempfile::tempdir()?;
        fs::create_dir(conflict.path().join(LEGACY))?;
        fs::create_dir(conflict.path().join(CANONICAL))?;
        assert_eq!(
            project_state_directory(conflict.path()),
            Err(NameMigrationError::Conflict)
        );
        assert!(matches!(
            migrate_project_name(conflict.path()),
            Err(NameMigrationError::Conflict)
        ));
        Ok(())
    }
}
