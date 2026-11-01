//! Explicit relocation preserves native identity and mutable state after a physical move.

mod dependency_ownership;

use serde_json::Value;
use std::{error::Error, fs, path::Path, process::Command};

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

fn call(root: &Path, arguments: &[&str]) -> TestResult<std::process::Output> {
    Ok(Command::new(env!("CARGO_BIN_EXE_codevetra"))
        .current_dir(root)
        .args(arguments)
        .output()?)
}

fn json(root: &Path, arguments: &[&str]) -> TestResult<Value> {
    let result = call(root, arguments)?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    Ok(serde_json::from_slice(&result.stdout)?)
}

#[test]
fn physical_relocation_preserves_identity_generation_notes_and_configuration() -> TestResult {
    let directory = tempfile::tempdir()?;
    let old = directory.path().canonicalize()?.join("original");
    let new = directory.path().canonicalize()?.join("renamed");
    fs::create_dir(&old)?;
    fs::write(
        old.join("lib.rs"),
        "pub fn target() {}\npub fn caller() { target(); }\n",
    )?;
    json(&old, &["index", "--format", "json"])?;
    fs::write(
        old.join(".codevetra/config.json"),
        r#"{"custom":{"preserve":true}}"#,
    )?;
    let before = json(&old, &["status", "--json"])?;
    let note = json(
        &old,
        &["note", "add", "--text", "retain this investigation"],
    )?;
    let notes = json(&old, &["note", "list"])?;
    let marker = fs::read(old.join(".codevetra/data/CARTOGRAPH_OWNER"))?;
    let database_identity = fs::read(old.join(".codevetra/data/IDENTITY"))?;
    assert!(!note.is_null());
    // Exercise the same physical legacy layout as an existing Cartograph project.
    fs::remove_file(old.join(".cartograph"))?;
    fs::rename(old.join(".codevetra"), old.join(".cartograph"))?;
    fs::rename(&old, &new)?;
    json(&new, &["migrate-name", "--format", "json"])?;
    assert!(!call(&new, &["status", "--json"])?.status.success());
    let from = old.to_str().ok_or("fixture path is not UTF-8")?;
    refusals_preserve_configuration(&old, &new, from)?;
    let report = json(
        &new,
        &["relocate", "--from-project-path", from, "--format", "json"],
    )?;
    assert_eq!(report["completed"], true);
    assert_eq!(report["identityPreserved"], true);
    let after = json(&new, &["status", "--json"])?;
    assert_eq!(
        before["project"]["snapshot"]["project_id"],
        after["project"]["snapshot"]["project_id"]
    );
    assert_eq!(
        before["project"]["snapshot"]["current"],
        after["project"]["snapshot"]["current"]
    );
    assert_eq!(after["project"]["fresh"], true);
    assert_eq!(notes, json(&new, &["note", "list"])?);
    assert_eq!(
        marker,
        fs::read(new.join(".codevetra/data/CARTOGRAPH_OWNER"))?
    );
    assert_eq!(
        database_identity,
        fs::read(new.join(".codevetra/data/IDENTITY"))?
    );
    let config: Value = serde_json::from_slice(&fs::read(new.join(".codevetra/config.json"))?)?;
    assert_eq!(config["custom"]["preserve"], true);
    let config_bytes = fs::read(new.join(".codevetra/config.json"))?;
    json(
        &new,
        &["relocate", "--from-project-path", from, "--format", "json"],
    )?;
    assert_eq!(config_bytes, fs::read(new.join(".codevetra/config.json"))?);
    assert_eq!(
        json(&new, &["index", "--format", "json"])?["published"],
        false
    );
    // A later physical move needs its own explicit binding and preserves the same graph.
    let third = directory.path().canonicalize()?.join("third");
    fs::rename(&new, &third)?;
    assert!(!call(&third, &["status", "--json"])?.status.success());
    json(
        &third,
        &[
            "relocate",
            "--from-project-path",
            new.to_str().ok_or("UTF-8 path")?,
        ],
    )?;
    assert_eq!(notes, json(&third, &["note", "list"])?);
    fs::write(
        third.join("lib.rs"),
        "pub fn target() {}\npub fn caller() { target(); }\npub fn edited_after_move() {}\n",
    )?;
    assert_eq!(
        json(&third, &["index", "--format", "json"])?["published"],
        true
    );
    assert_eq!(
        before["project"]["snapshot"]["project_id"],
        json(&third, &["status", "--json"])?["project"]["snapshot"]["project_id"]
    );
    Ok(())
}

fn refusals_preserve_configuration(old: &Path, new: &Path, from: &str) -> TestResult {
    let original_config = fs::read(new.join(".codevetra/config.json"))?;
    for invalid in [
        "relative",
        "/missing-parent/../original",
        "/unrelated-missing-project",
    ] {
        let failure = call(new, &["relocate", "--from-project-path", invalid])?;
        assert!(!failure.status.success());
        assert!(!String::from_utf8_lossy(&failure.stderr).contains(invalid));
        assert_eq!(
            original_config,
            fs::read(new.join(".codevetra/config.json"))?
        );
    }
    // A surviving original location cannot be silently adopted as a move.
    fs::create_dir(old)?;
    assert!(
        !call(new, &["relocate", "--from-project-path", from])?
            .status
            .success()
    );
    fs::remove_dir(old)?;
    // A held primary is refused even though the old path is gone.
    let primary = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(new.join(".codevetra/native-primary.lock"))?;
    primary.lock()?;
    assert!(
        !call(new, &["relocate", "--from-project-path", from])?
            .status
            .success()
    );
    primary.unlock()?;
    assert_eq!(
        original_config,
        fs::read(new.join(".codevetra/config.json"))?
    );
    Ok(())
}
