//! Explicit canonical-root changes retain the existing native identity and mutable state.

use std::{
    path::{Component, Path, PathBuf},
    time::Duration,
};

use codevetra_config::{ProjectConfigError, read_project_config, update_project_config};
use codevetra_db::rocks::StorageRequest;
use codevetra_domain::ContentDigest;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ProjectError, RocksProjectError, RocksProjectRuntime, project_identity_digest,
    run_source_worker,
};

const BINDING_KEY: &str = "nativeRootIdentity";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum BindingContract {
    V1,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RootBinding {
    contract: BindingContract,
    source: ContentDigest,
    location: ContentDigest,
    storage: ContentDigest,
}

fn binding(config: Option<&Value>) -> Result<Option<RootBinding>, ProjectConfigError> {
    config
        .and_then(|value| value.get(BINDING_KEY))
        .map(|value| {
            serde_json::from_value(value.clone()).map_err(|_| ProjectConfigError::InvalidConfig)
        })
        .transpose()
}

pub(super) fn resolved_identity(root: &Path) -> Result<ContentDigest, ProjectError> {
    let location = project_identity_digest(root);
    let config = read_project_config(root)?;
    match binding(config.as_ref())? {
        None => Ok(location),
        Some(binding) if binding.location == location => Ok(binding.storage),
        Some(_) => Err(ProjectConfigError::InvalidConfig.into()),
    }
}

/// Result of an explicit relocation; no source, credentials or absolute paths are returned.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectRelocationReport {
    /// The current canonical root can open its preserved native state.
    pub completed: bool,
    /// Project, graph and mutable-state identities were retained.
    pub identity_preserved: bool,
    /// An identical relocation had already completed.
    pub already_current: bool,
}

struct RelocationPlan {
    root: PathBuf,
    observed: Option<Value>,
    binding: RootBinding,
    already_current: bool,
}

fn commit(plan: RelocationPlan, request: &StorageRequest) -> Result<(), ProjectError> {
    request
        .check()
        .map_err(|_| ProjectError::RequestCancelled)?;
    if plan.already_current {
        if read_project_config(&plan.root)? != plan.observed {
            return Err(ProjectConfigError::ConcurrentModification.into());
        }
        return Ok(());
    }
    update_project_config::<_, ProjectError>(&plan.root, |current| {
        request
            .check()
            .map_err(|_| ProjectError::RequestCancelled)?;
        if current != plan.observed {
            return Err(ProjectConfigError::ConcurrentModification.into());
        }
        let mut config = current.unwrap_or_else(|| serde_json::json!({}));
        let value =
            serde_json::to_value(plan.binding).map_err(|_| ProjectConfigError::InvalidConfig)?;
        config
            .as_object_mut()
            .ok_or(ProjectConfigError::InvalidConfig)?
            .insert(BINDING_KEY.to_owned(), value);
        Ok((config, ()))
    })
}

fn plan(root: &Path, previous: &Path) -> Result<RelocationPlan, ProjectError> {
    if !previous.is_absolute()
        || previous.as_os_str().as_encoded_bytes().len() > 4096
        || previous
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(ProjectError::ProjectRootUnavailable);
    }
    let root = root
        .canonicalize()
        .map_err(|_| ProjectError::ProjectRootUnavailable)?;
    match previous.canonicalize() {
        Ok(prior) if prior != root => return Err(ProjectError::ProjectRootUnavailable),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(ProjectError::ProjectRootUnavailable);
        }
        _ => (),
    }
    let state = codevetra_config::project_state_directory(&root)
        .map_err(|_| ProjectError::ProjectRootUnavailable)?;
    for name in ["CURRENT", "CARTOGRAPH_OWNER"] {
        if !std::fs::symlink_metadata(state.join("data").join(name))
            .is_ok_and(|meta| meta.is_file() && !meta.file_type().is_symlink())
        {
            return Err(ProjectError::ProjectRootUnavailable);
        }
    }
    let location = project_identity_digest(&root);
    let source = project_identity_digest(previous);
    if source == location {
        return Err(ProjectError::ProjectRootUnavailable);
    }
    let observed = read_project_config(&root)?;
    let prior = binding(observed.as_ref())?;
    let already_current = prior
        .as_ref()
        .is_some_and(|prior| prior.location == location && prior.source == source);
    let storage = match prior {
        None => source.clone(),
        Some(prior) if prior.location == source || already_current => prior.storage,
        Some(_) => return Err(ProjectConfigError::InvalidConfig.into()),
    };
    Ok(RelocationPlan {
        root,
        observed,
        binding: RootBinding {
            contract: BindingContract::V1,
            source,
            location,
            storage,
        },
        already_current,
    })
}

/// Adopt a physically relocated native project without changing its durable identities.
///
/// The previous absolute canonical root is explicit. A surviving independent original,
/// a live native owner, incompatible storage, or a concurrent configuration change fails
/// closed. The native primary remains held until the bounded atomic config commit.
/// Run name migration first when the project still has legacy-only state.
/// # Errors
/// Rejects unsafe paths, foreign storage, active owners and changed configuration.
pub async fn relocate_project(
    root: PathBuf,
    previous: PathBuf,
) -> Result<ProjectRelocationReport, RocksProjectError> {
    let plan = run_source_worker(move || plan(&root, &previous)).await?;
    apply(plan, || Ok(())).await
}

async fn apply(
    plan: RelocationPlan,
    before_commit: impl FnOnce() -> Result<(), ProjectError> + Send + 'static,
) -> Result<ProjectRelocationReport, RocksProjectError> {
    let request = StorageRequest::new(Duration::from_secs(60))?;
    let runtime = RocksProjectRuntime::open_with_identity(
        plan.root.clone(),
        plan.binding.storage.clone(),
        &request,
    )
    .await?;
    let already_current = plan.already_current;
    let result = runtime
        .owned_operation(&request, move |owner, control| async move {
            run_source_worker(move || {
                let _owner = owner;
                before_commit()?;
                commit(plan, &control)
            })
            .await
            .map_err(RocksProjectError::from)
        })
        .await;
    let shutdown = runtime.shutdown().await;
    result?;
    shutdown?;
    Ok(ProjectRelocationReport {
        completed: true,
        identity_preserved: true,
        already_current,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex};

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn fixture_plan(root: &Path, already_current: bool) -> RelocationPlan {
        RelocationPlan {
            root: root.to_path_buf(),
            observed: Some(serde_json::json!({"custom": "original"})),
            binding: RootBinding {
                contract: BindingContract::V1,
                source: ContentDigest::from_bytes([1; 32]),
                location: ContentDigest::from_bytes([2; 32]),
                storage: ContentDigest::from_bytes([3; 32]),
            },
            already_current,
        }
    }

    #[test]
    fn concurrent_configuration_and_cancellation_never_commit_a_binding() -> TestResult {
        let root = tempfile::tempdir()?;
        let request = StorageRequest::new(Duration::from_secs(30))?;
        update_project_config::<_, ProjectConfigError>(root.path(), |_| {
            Ok((serde_json::json!({"custom": "concurrent"}), ()))
        })?;
        for already_current in [false, true] {
            assert!(matches!(
                commit(fixture_plan(root.path(), already_current), &request),
                Err(ProjectError::ProjectConfiguration(
                    ProjectConfigError::ConcurrentModification
                ))
            ));
        }
        request.cancel();
        assert_eq!(
            commit(fixture_plan(root.path(), false), &request),
            Err(ProjectError::RequestCancelled)
        );
        assert_eq!(
            read_project_config(root.path())?,
            Some(serde_json::json!({"custom": "concurrent"}))
        );
        Ok(())
    }

    #[test]
    fn malformed_unknown_and_foreign_bindings_fail_closed() -> TestResult {
        let root = tempfile::tempdir()?;
        let canonical = root.path().canonicalize()?;
        let record = serde_json::to_value(fixture_plan(&canonical, false).binding)?;
        let mut unknown = record.clone();
        unknown["contract"] = serde_json::json!("v2");
        let mut extra = record.clone();
        extra["unexpected"] = serde_json::json!(true);
        for value in [Value::Null, serde_json::json!({}), unknown, extra, record] {
            update_project_config::<_, ProjectConfigError>(&canonical, |_| {
                Ok((serde_json::json!({BINDING_KEY: value}), ()))
            })?;
            assert!(resolved_identity(&canonical).is_err());
        }
        assert!(plan(&canonical, &canonical).is_err());
        assert!(plan(&canonical, Path::new("relative")).is_err());
        assert!(plan(&canonical, &canonical.join("missing")).is_err());
        Ok(())
    }

    struct ReleaseGate(Arc<(Mutex<bool>, Condvar)>);

    impl Drop for ReleaseGate {
        fn drop(&mut self) {
            if let Ok(mut released) = self.0.0.lock() {
                *released = true;
                self.0.1.notify_all();
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_commit_retains_primary_until_blocking_work_is_reaped() -> TestResult {
        let directory = tempfile::tempdir()?;
        let old = directory.path().canonicalize()?.join("old");
        let new = directory.path().canonicalize()?.join("new");
        std::fs::create_dir(&old)?;
        let original =
            RocksProjectRuntime::open(&old, &StorageRequest::new(Duration::from_secs(10))?).await?;
        original.shutdown().await?;
        drop(original);
        std::fs::rename(&old, &new)?;
        let plan = plan(&new, &old)?;
        let gate = ReleaseGate(Arc::new((Mutex::new(false), Condvar::new())));
        let worker_gate = Arc::clone(&gate.0);
        let (started, entered) = tokio::sync::oneshot::channel();
        let caller = tokio::spawn(apply(plan, move || {
            let _ = started.send(());
            let released = worker_gate
                .0
                .lock()
                .map_err(|_| ProjectError::SourceScanFailed)?;
            let (released, _) = worker_gate
                .1
                .wait_timeout_while(released, Duration::from_secs(10), |value| !*value)
                .map_err(|_| ProjectError::SourceScanFailed)?;
            if !*released {
                return Err(ProjectError::SourceScanFailed);
            }
            Ok(())
        }));
        tokio::time::timeout(Duration::from_secs(5), entered).await??;
        caller.abort();
        assert!(caller.await.is_err());
        let primary = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(new.join(".codevetra/native-primary.lock"))?;
        assert!(matches!(
            primary.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert!(read_project_config(&new)?.is_none());
        drop(gate);
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match primary.try_lock() {
                    Ok(()) => break Ok::<(), Box<dyn std::error::Error>>(()),
                    Err(std::fs::TryLockError::WouldBlock) => tokio::task::yield_now().await,
                    Err(error) => break Err(error.into()),
                }
            }
        })
        .await??;
        primary.unlock()?;
        assert!(read_project_config(&new)?.is_none());
        Ok(())
    }
}
