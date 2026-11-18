//! Requested run policy remains paired through scanning and publication.

use super::{
    GraphPublicationInput, Path, ProjectError, ProjectSourcePolicy, RocksError, StoredRevision,
    project_source_policy_with_excludes,
};

/// This is a requested policy, not proof of source or storage admission.
#[derive(Clone, Default)]
pub(super) struct SourceAdmission {
    pub(super) excludes: Vec<String>,
    pub(super) ceiling: Option<usize>,
}

impl SourceAdmission {
    pub(super) fn attached(input: &GraphPublicationInput) -> Self {
        let (excludes, ceiling) = input.source_admission();
        Self {
            excludes: excludes.to_vec(),
            ceiling,
        }
    }

    pub(super) fn inherited(revision: Option<StoredRevision>) -> Self {
        revision.map_or_else(Self::default, |revision| Self {
            excludes: revision.run_excludes,
            ceiling: revision.maximum_file_bytes,
        })
    }

    pub(super) fn matches(&self, revision: &StoredRevision) -> bool {
        revision.run_excludes == self.excludes && revision.maximum_file_bytes == self.ceiling
    }

    pub(super) fn policy(&self, root: &Path) -> Result<ProjectSourcePolicy, ProjectError> {
        let mut policy = project_source_policy_with_excludes(root, &self.excludes)?;
        if let Some(ceiling) = self.ceiling {
            if !(1..=32 * 1_048_576).contains(&ceiling) {
                return Err(ProjectError::InvalidOptions);
            }
            policy.maximum_file_bytes = Some(
                policy
                    .maximum_file_bytes
                    .map_or(ceiling, |configured| configured.min(ceiling)),
            );
        }
        Ok(policy)
    }

    pub(super) fn attach(
        &self,
        input: GraphPublicationInput,
    ) -> Result<GraphPublicationInput, RocksError> {
        input.with_source_admission(self.excludes.clone(), self.ceiling)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rocks::{
        GraphBase, GraphBuild, GraphPolicy, NativeBuildProgress, RocksIndexOptions,
        RocksProjectError, RocksProjectRuntime, StorageRequest,
    };
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn source_verification_uses_attached_policy_and_reloads_configuration()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        std::fs::write(root.path().join("first.rs"), "pub fn first() {}\n")?;
        let request = StorageRequest::new(Duration::from_secs(30))?;
        let runtime = RocksProjectRuntime::open(root.path(), &request).await?;
        let admission = SourceAdmission::default();
        let (source, policy) = runtime
            .scan_with_admission(&request, false, admission.clone())
            .await?;
        let (input, _, _) = runtime
            .build_graph(GraphBuild {
                source,
                policy,
                options: &RocksIndexOptions::default(),
                base: GraphBase::Empty,
                request: &request,
                progress: &NativeBuildProgress::default(),
                admission: &admission,
                publication_policy: GraphPolicy::default(),
            })
            .await?;
        let (source, _) = runtime
            .scan_with_admission(&request, false, admission)
            .await?;
        let input = input.with_source_admission(vec!["first.rs".to_owned()], None)?;
        assert!(matches!(
            runtime.verify_graph_source(&input, &source, &request).await,
            Err(RocksProjectError::SourceChanged)
        ));
        let input = input.with_source_admission(Vec::new(), None)?;
        runtime
            .verify_graph_source(&input, &source, &request)
            .await?;
        std::fs::write(
            root.path().join(".codevetra/config.json"),
            r#"{"maxFileSize":1}"#,
        )?;
        assert!(matches!(
            runtime.verify_graph_source(&input, &source, &request).await,
            Err(RocksProjectError::SourceChanged)
        ));
        runtime.shutdown().await?;
        Ok(())
    }
}
