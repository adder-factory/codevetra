//! Bounded graph, parse-cache and auxiliary cleanup under the existing project owner.

use std::time::Duration;

use codevetra_db::rocks::{GraphRetention, PruneReport, RetentionPolicy, StorageRequest};
use codevetra_domain::ContentDigest;

use super::{RocksMaintenanceReport, RocksProjectRuntime, retention_outcome};

pub(super) struct Maintenance<'a> {
    runtime: &'a RocksProjectRuntime,
    request: &'a StorageRequest,
}

impl<'a> Maintenance<'a> {
    pub(super) const fn new(runtime: &'a RocksProjectRuntime, request: &'a StorageRequest) -> Self {
        Self { runtime, request }
    }

    pub(super) async fn complete(&self, contract: &ContentDigest) -> RocksMaintenanceReport {
        let (graph, graph_error) = self.graph().await;
        let (parse_cache, parse_cache_error, auxiliary, auxiliary_error) =
            self.auxiliary(contract).await;
        RocksMaintenanceReport {
            graph,
            graph_error,
            parse_cache,
            parse_cache_error,
            auxiliary,
            auxiliary_error,
        }
    }

    pub(super) async fn graph(&self) -> (Option<PruneReport>, Option<String>) {
        let result = async {
            self.runtime
                .store
                .with_request(self.request)
                .prune(
                    RetentionPolicy::AUTOMATIC,
                    RetentionPolicy::read_budget(Duration::from_secs(30))?,
                )
                .await
        }
        .await;
        retention_outcome(&match result {
            Ok(report) => GraphRetention::Completed(report),
            Err(error) => GraphRetention::Deferred(error),
        })
    }

    pub(super) async fn auxiliary(
        &self,
        contract: &ContentDigest,
    ) -> (
        Option<codevetra_db::NativeParseCacheRetentionReport>,
        Option<String>,
        Option<codevetra_db::NativeAuxiliaryPruneReport>,
        Option<String>,
    ) {
        let cache = self
            .runtime
            .database
            .prune_native_parse_cache_with_request(
                self.runtime.database.project_id(),
                codevetra_db::NativeParseCacheRetentionPolicy::automatic().protecting(contract),
                self.request,
            )
            .await;
        let auxiliary = self
            .runtime
            .database
            .prune_auxiliary_snapshots_with_request(self.request)
            .await;
        let (cache, cache_error) = match cache {
            Ok(report) => (Some(report), None),
            Err(error) => (None, Some(error.to_string())),
        };
        let (auxiliary, auxiliary_error) = match auxiliary {
            Ok(report) => (Some(report), None),
            Err(error) => (None, Some(error.to_string())),
        };
        (cache, cache_error, auxiliary, auxiliary_error)
    }
}
