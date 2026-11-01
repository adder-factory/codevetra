//! Validated project configuration and native storage migration markers.

mod name_migration;
mod numeric;
mod project;
mod storage;

pub use storage::{StorageMigrationGuard, record_rocksdb_migration, storage_migration_guard};

pub use name_migration::{
    NameMigrationError, NameMigrationReport, StatePlacement, migrate_project_name,
    prepare_project_state, project_state_directory,
};
pub use numeric::{BoundedU64Field, optional_bounded_u64};

pub use project::{
    ProjectConfigError, ProjectConfigSnapshot, ProjectGenerationStorage, ProjectSourceSettings,
    load_project_source_settings, read_project_config, read_project_config_snapshot,
    update_project_config, write_project_config_if_unchanged, write_project_max_file_size,
};

/// Read a canonical product environment variable, falling back to its legacy name
/// only when the canonical name is absent. Provider-owned names are unchanged.
/// # Errors
/// Preserves non-Unicode/presence errors and never substitutes for an invalid canonical value.
pub fn product_var(key: &str) -> Result<String, std::env::VarError> {
    match std::env::var(key) {
        Err(std::env::VarError::NotPresent) => match key.strip_prefix("CODEVETRA_") {
            Some(suffix) => std::env::var(format!("CARTOGRAPH_{suffix}")),
            None => Err(std::env::VarError::NotPresent),
        },
        result => result,
    }
}

/// OS-string equivalent of [`product_var`], including an explicitly empty canonical value.
#[must_use]
pub fn product_var_os(key: &str) -> Option<std::ffi::OsString> {
    std::env::var_os(key).or_else(|| {
        key.strip_prefix("CODEVETRA_")
            .and_then(|suffix| std::env::var_os(format!("CARTOGRAPH_{suffix}")))
    })
}

#[cfg(test)]
mod environment_tests {
    use super::{product_var, product_var_os};
    use std::{env, process::Command};

    #[test]
    fn environment_child() {
        let Ok(expected) = env::var("CODEVETRA_TEST_ENV_EXPECTED") else {
            return;
        };
        assert_eq!(
            product_var("CODEVETRA_TEST_POLICY").as_deref(),
            Ok(expected.as_str())
        );
        assert_eq!(
            product_var_os("CODEVETRA_TEST_POLICY"),
            Some(expected.into())
        );
        assert_eq!(product_var("PROVIDER_OWNED_KEY").as_deref(), Ok("provider"));
        assert!(product_var("CODEVETRA_TEST_ABSENT").is_err());
    }

    #[test]
    fn canonical_presence_controls_legacy_environment_fallback()
    -> Result<(), Box<dyn std::error::Error>> {
        for (canonical, expected) in [
            (None, "legacy-policy"),
            (Some("canonical-policy"), "canonical-policy"),
            (Some(""), ""),
        ] {
            let mut command = Command::new(env::current_exe()?);
            command
                .args([
                    "--exact",
                    "environment_tests::environment_child",
                    "--nocapture",
                ])
                .env_remove("CODEVETRA_TEST_POLICY")
                .env_remove("CODEVETRA_TEST_ABSENT")
                .env_remove("CARTOGRAPH_TEST_ABSENT")
                .env("CARTOGRAPH_TEST_POLICY", "legacy-policy")
                .env("CODEVETRA_TEST_ENV_EXPECTED", expected)
                .env("PROVIDER_OWNED_KEY", "provider");
            if let Some(value) = canonical {
                command.env("CODEVETRA_TEST_POLICY", value);
            }
            let output = command.output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stdout)
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_canonical_value_does_not_fall_back() -> Result<(), Box<dyn std::error::Error>> {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt as _};
        if env::var_os("CODEVETRA_TEST_INVALID_CHILD").is_some() {
            assert!(matches!(
                product_var("CODEVETRA_TEST_POLICY"),
                Err(env::VarError::NotUnicode(_))
            ));
            assert_eq!(
                product_var_os("CODEVETRA_TEST_POLICY"),
                Some(OsString::from_vec(vec![255]))
            );
            return Ok(());
        }
        let output = Command::new(env::current_exe()?)
            .args([
                "--exact",
                "environment_tests::non_unicode_canonical_value_does_not_fall_back",
                "--nocapture",
            ])
            .env("CODEVETRA_TEST_INVALID_CHILD", "1")
            .env("CODEVETRA_TEST_POLICY", OsString::from_vec(vec![255]))
            .env("CARTOGRAPH_TEST_POLICY", "legacy-policy")
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        Ok(())
    }
}
