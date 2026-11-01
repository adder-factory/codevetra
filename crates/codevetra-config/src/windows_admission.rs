//! Coordinate state-path transitions before resolving paths or admitting file locks.
use crate::NameMigrationError;
use std::{fs, path::Path};
use winsafe::{HEVENT, co, guard::CloseHandleGuard};

/// Short-lived, cross-process admission for Windows state opens and name migration.
/// Active owners remain protected by their original on-disk primary/config locks.
/// An auto-reset event avoids thread-affine or recursively acquired mutex guards.
pub struct StateAdmission(CloseHandleGuard<HEVENT>);

impl StateAdmission {
    /// Admit one state-path transition without waiting or exposing the project path.
    /// # Errors
    /// Refuses another admission or an unavailable canonical root/kernel object.
    pub fn acquire(root: &Path) -> Result<Self, NameMigrationError> {
        let root = fs::canonicalize(root).map_err(|_| NameMigrationError::UnsafePath)?;
        let root = root
            .to_str()
            .ok_or(NameMigrationError::UnsafePath)?
            .to_lowercase();
        let digest = blake3::hash(root.as_bytes());
        let name = format!("Local\\CodeVetra-StateAdmission-{digest}");
        let event = HEVENT::CreateEvent(None, false, true, Some(&name))
            .map_err(|_| NameMigrationError::UnsafePath)?;
        match event.WaitForSingleObject(Some(0)) {
            Ok(co::WAIT::OBJECT_0) => Ok(Self(event)),
            Ok(co::WAIT::TIMEOUT) => Err(NameMigrationError::Busy),
            Ok(_) | Err(_) => Err(NameMigrationError::UnsafePath),
        }
    }
}

impl Drop for StateAdmission {
    fn drop(&mut self) {
        let _released = self.0.SetEvent();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{env, process::Command};
    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn admission_child() -> TestResult {
        let Some(root) = env::var_os("CODEVETRA_ADMISSION_TEST_ROOT") else {
            return Ok(());
        };
        if env::var_os("CODEVETRA_ADMISSION_TEST_CRASH").is_some() {
            let _guard = StateAdmission::acquire(Path::new(&root))?;
            // Simulate process loss: no Rust destructors release the event.
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
            Err(crate::ProjectConfigError::NameMigration(
                NameMigrationError::Busy
            ))
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
        assert!(run(true)?.success());
        drop(StateAdmission::acquire(root.path())?);
        Ok(())
    }
}
