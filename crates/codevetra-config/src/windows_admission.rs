//! Coordinate Windows state-path transitions with a stable sibling file lock.
use crate::NameMigrationError;
use std::{
    fs::{self, File, OpenOptions},
    os::windows::fs::{MetadataExt as _, OpenOptionsExt as _},
    path::Path,
    thread,
    time::{Duration, Instant},
};

/// Short-lived admission before resolving state paths or acquiring original locks.
/// The sibling `.codevetra.lock` stays in place across migration and process loss.
/// Never delete it: another process may already have opened the same lock identity.
pub struct StateAdmission {
    _file: File,
}

impl StateAdmission {
    /// Admit a migration without waiting for another state-path transition.
    /// # Errors
    /// Refuses contention, reparse points, and unavailable admission storage.
    pub fn acquire(root: &Path) -> Result<Self, NameMigrationError> {
        Self::acquire_until(root, Instant::now())
    }

    /// Wait for ordinary owner/config admission within the caller's deadline.
    /// # Errors
    /// Refuses timed-out contention, reparse points, and unavailable storage.
    pub fn acquire_until(root: &Path, deadline: Instant) -> Result<Self, NameMigrationError> {
        let path = root.join(".codevetra.lock");
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            // Open the reparse point itself, and deny deletion/replacement while open.
            .custom_flags(0x0020_0000) // FILE_FLAG_OPEN_REPARSE_POINT
            .share_mode(0x0000_0003) // FILE_SHARE_READ | FILE_SHARE_WRITE
            .open(path)
            .map_err(|_| NameMigrationError::AdmissionUnavailable)?;
        let metadata = file
            .metadata()
            .map_err(|_| NameMigrationError::AdmissionUnavailable)?;
        if !metadata.is_file() || metadata.file_attributes() & 0x0000_0400 != 0 {
            return Err(NameMigrationError::AdmissionUnavailable);
        }
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    thread::sleep(
                        Duration::from_millis(10)
                            .min(deadline.saturating_duration_since(Instant::now())),
                    );
                }
                Err(fs::TryLockError::WouldBlock) => return Err(NameMigrationError::Busy),
                Err(_) => return Err(NameMigrationError::AdmissionUnavailable),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, process::Command};
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn name_migration_admission_refuses_reparse_points_without_touching_target() -> TestResult {
        let root = tempfile::tempdir()?;
        let target = root.path().join("protected");
        fs::write(&target, b"preserve")?;
        std::os::windows::fs::symlink_file(&target, root.path().join(".codevetra.lock"))?;
        assert!(matches!(
            StateAdmission::acquire(root.path()),
            Err(NameMigrationError::AdmissionUnavailable)
        ));
        assert_eq!(fs::read(target)?, b"preserve");
        Ok(())
    }

    #[test]
    fn name_migration_admission_serializes_ordinary_config_writers() -> TestResult {
        let root = tempfile::tempdir()?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(4));
        let mut writers = Vec::new();
        for _ in 0..4 {
            let root = root.path().to_owned();
            let barrier = std::sync::Arc::clone(&barrier);
            writers.push(thread::spawn(move || {
                barrier.wait();
                for _ in 0..10 {
                    crate::update_project_config::<_, crate::ProjectConfigError>(
                        &root,
                        |previous| {
                            let count = previous
                                .as_ref()
                                .and_then(|value| value.get("count"))
                                .and_then(serde_json::Value::as_u64)
                                .unwrap_or(0);
                            Ok((serde_json::json!({"count":count + 1}), ()))
                        },
                    )?;
                }
                Ok::<_, crate::ProjectConfigError>(())
            }));
        }
        for writer in writers {
            writer.join().map_err(|_| "writer panicked")??;
        }
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(root.path().join(".codevetra/config.json"))?)?;
        assert_eq!(value["count"], 40);
        Ok(())
    }

    #[test]
    fn admission_child() -> TestResult {
        let Some(root) = env::var_os("CODEVETRA_ADMISSION_TEST_ROOT") else {
            return Ok(());
        };
        if env::var_os("CODEVETRA_ADMISSION_TEST_CRASH").is_some() {
            let _guard = StateAdmission::acquire(Path::new(&root))?;
            // Simulate process loss: no Rust destructors release the file lock.
            std::process::exit(0);
        }
        assert!(matches!(
            StateAdmission::acquire(Path::new(&root)),
            Err(NameMigrationError::Busy)
        ));
        let writer: Result<(), crate::ProjectConfigError> =
            crate::update_project_config(Path::new(&root), |_| Ok((serde_json::json!({}), ())));
        assert!(matches!(
            writer,
            Err(crate::ProjectConfigError::ConcurrentModification)
        ));
        Ok(())
    }

    #[test]
    fn name_migration_admission_excludes_processes_and_recovers_after_process_loss() -> TestResult {
        let root = tempfile::tempdir()?;
        let guard = StateAdmission::acquire(root.path())?;
        assert!(matches!(
            StateAdmission::acquire(root.path()),
            Err(NameMigrationError::Busy)
        ));
        let run = |crash| {
            let mut child = Command::new(env::current_exe()?);
            child
                .args([
                    "--exact",
                    "windows_admission::tests::admission_child",
                    "--nocapture",
                ])
                .env("CODEVETRA_ADMISSION_TEST_ROOT", root.path());
            if crash {
                child.env("CODEVETRA_ADMISSION_TEST_CRASH", "1");
            }
            Ok::<_, Box<dyn std::error::Error>>(child.status()?)
        };
        assert!(run(false)?.success());
        assert!(!root.path().join(".codevetra").exists());
        drop(guard);
        let observer = File::open(root.path().join(".codevetra.lock"))?;
        assert!(run(true)?.success());
        drop(StateAdmission::acquire(root.path())?);
        drop(observer);
        Ok(())
    }
}
