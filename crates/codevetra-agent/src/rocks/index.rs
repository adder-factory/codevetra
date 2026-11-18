//! One admitted index operation owns its request and clock through completion.

use super::{
    ContentDigest, GenerationDigestVersion, GraphBase, GraphBuild, GraphInfo, GraphWriteReport,
    IndexProfile, Maintenance, NativePipelineReport, PreparedGraph, RocksIndexOptions,
    RocksIndexReport, RocksProjectError, RocksProjectRuntime, StorageRequest, StoredRevision,
    graph_publication_policy, incremental, monotonic_millis, retention_outcome,
};

pub(super) struct IndexRun<'a> {
    runtime: &'a RocksProjectRuntime,
    request: &'a StorageRequest,
    started: std::time::Instant,
}

struct IndexCompletion {
    native: Option<NativePipelineReport>,
    workers: u16,
    incremental_files: u16,
    cache_contract: ContentDigest,
    publication: codevetra_db::rocks::GraphPublication,
    counts: GraphInfo,
    profile: IndexProfile,
}

impl<'a> IndexRun<'a> {
    pub(super) fn new(runtime: &'a RocksProjectRuntime, request: &'a StorageRequest) -> Self {
        Self {
            runtime,
            request,
            started: std::time::Instant::now(),
        }
    }

    pub(super) async fn execute(
        &self,
        options: RocksIndexOptions,
        warm: &mut Option<incremental::WarmGraph>,
    ) -> Result<RocksIndexReport, RocksProjectError> {
        let runtime = self.runtime;
        let request = self.request;
        let started = self.started;
        let current = runtime.current(request).await?;
        let inherited = current.as_ref().map(|(revision, _)| revision);
        let admission = options.source_admission(inherited)?;
        let (source, policy) = runtime
            .scan_with_admission(request, true, admission.clone())
            .await?;
        let cache_contract =
            codevetra_indexer::native_parse_cache_contract_digest(policy.index.maximum_ast_depth);
        let preparation_millis = monotonic_millis(started.elapsed());
        let progress = options.progress.clone().unwrap_or_default();
        if !options.force
            && let Some(current) = current.as_ref().filter(|(revision, _)| {
                revision.source_revision == source.digest
                    && revision.digest_version == Some(GenerationDigestVersion::CURRENT)
                    && admission.matches(revision)
            })
        {
            return self
                .unchanged_report(current, cache_contract, preparation_millis)
                .await;
        }
        let base = current.as_ref().map_or(GraphBase::Empty, |(revision, _)| {
            GraphBase::Current(revision.generation_id.clone())
        });
        let pipeline_started = std::time::Instant::now();
        let index_policy = policy.index;
        let scip_overlay = source.scip_overlay.is_some();
        let observed_source = source.native_manifest.is_some();
        let PreparedGraph {
            input,
            native,
            workers,
            incremental_files,
        } = runtime
            .prepare_graph(
                warm,
                GraphBuild {
                    source,
                    policy,
                    options: &options,
                    base,
                    request,
                    progress: &progress,
                    admission: &admission,
                    publication_policy: graph_publication_policy(),
                },
            )
            .await?;
        let counts = input.info()?;
        let retained_facts = input.shared_facts();
        if incremental_files > 0 {
            progress.publishing_incremental().await?;
        } else {
            progress.publishing().await?;
        }
        let publication_store = runtime.store.with_request(request);
        let publication = publication_store.publish_graph(input).await?;
        *warm = retained_facts.map(|facts| incremental::WarmGraph {
            generation: publication.revision.generation_id.clone(),
            policy: index_policy,
            scip_overlay,
            facts,
        });
        progress.finish(true, false).await;
        let pipeline_millis = monotonic_millis(pipeline_started.elapsed());
        self.complete_index(IndexCompletion {
            native,
            workers,
            incremental_files,
            cache_contract,
            publication,
            counts,
            profile: IndexProfile {
                preparation_millis,
                source_manifest_reused: observed_source && incremental_files == 0,
                pipeline_millis,
                pipeline_stages: progress.status().await.stage_timings().to_vec(),
                ..IndexProfile::default()
            },
        })
        .await
    }

    async fn complete_index(
        &self,
        completed: IndexCompletion,
    ) -> Result<RocksIndexReport, RocksProjectError> {
        let request = self.request;
        let started = self.started;
        let IndexCompletion {
            native,
            workers,
            incremental_files,
            cache_contract,
            publication,
            counts,
            mut profile,
        } = completed;
        let (retention_progress, retention_error) = retention_outcome(&publication.retention);
        let (
            parse_cache_retention,
            parse_cache_retention_error,
            auxiliary_retention,
            auxiliary_retention_error,
        ) = Maintenance::new(self.runtime, request)
            .auxiliary(&cache_contract)
            .await;
        let (fresh, source_verification_error) = self
            .runtime
            .verify_publication(&publication.revision.generation_id, request)
            .await;
        let parse_cache = native.as_ref().map(NativePipelineReport::parse_cache);
        profile.total_millis = monotonic_millis(started.elapsed());
        Ok(RocksIndexReport {
            native: native.map(Into::into),
            workers,
            parse_cache_contract_digest: cache_contract,
            profile,
            parse_cache,
            parse_cache_retention,
            parse_cache_retention_error,
            auxiliary_retention,
            auxiliary_retention_error,
            unchanged: false,
            incremental_files,
            revision: publication.revision,
            counts,
            writes: publication.report,
            fresh,
            retention_completed: Some(retention_progress.is_some_and(|report| report.complete)),
            retention_progress,
            retention_error,
            source_verification_error,
        })
    }

    async fn unchanged_report(
        &self,
        current: &(StoredRevision, GraphInfo),
        cache_contract: ContentDigest,
        preparation_millis: u64,
    ) -> Result<RocksIndexReport, RocksProjectError> {
        let request = self.request;
        let started = self.started;
        let (revision, counts) = current;
        let (retention_progress, retention_error) =
            Maintenance::new(self.runtime, request).graph().await;
        let (
            parse_cache_retention,
            parse_cache_retention_error,
            auxiliary_retention,
            auxiliary_retention_error,
        ) = Maintenance::new(self.runtime, request)
            .auxiliary(&cache_contract)
            .await;
        let fresh = self.runtime.status_inner(request).await?.fresh;
        Ok(RocksIndexReport {
            native: None,
            workers: 0,
            parse_cache_contract_digest: cache_contract,
            profile: IndexProfile {
                preparation_millis,
                total_millis: monotonic_millis(started.elapsed()),
                ..IndexProfile::default()
            },
            parse_cache: None,
            parse_cache_retention,
            parse_cache_retention_error,
            auxiliary_retention,
            auxiliary_retention_error,
            unchanged: true,
            incremental_files: 0,
            revision: revision.clone(),
            counts: counts.clone(),
            writes: GraphWriteReport::default(),
            fresh,
            retention_completed: Some(retention_progress.is_some_and(|report| report.complete)),
            retention_progress,
            retention_error,
            source_verification_error: None,
        })
    }
}
