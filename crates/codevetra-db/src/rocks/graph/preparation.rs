//! Bounded graph payload preparation inside the caller's CPU reservation.

use std::sync::Arc;

use crate::{CanonicalGenerationFacts, GenerationMemoryModelError};

use super::{
    GraphBase, GraphPayload, GraphPolicy, GraphPublicationInput, GraphRevision, Result, RocksError,
    StorageRequest, ingest, limit, packed,
};

/// One graph revision's construction inputs, shared by full and sparse preparation.
///
/// Binding these inputs performs no work or admission. Each consuming operation
/// checks the live request and validates its policy inside the caller's CPU reservation.
/// Publication still verifies the current generation and writer fence separately.
pub struct GraphPreparation {
    identity: GraphRevision,
    policy: GraphPolicy,
    request: StorageRequest,
}

impl GraphPreparation {
    /// Bind immutable revision and limits to the preparation's cancellation/deadline.
    #[must_use]
    pub fn new(identity: GraphRevision, policy: GraphPolicy, request: &StorageRequest) -> Self {
        Self {
            identity,
            policy,
            request: request.clone(),
        }
    }

    /// Check whether this preparation can continue doing bounded CPU work.
    /// # Errors
    /// Returns cancellation or deadline expiry from the bound request.
    pub fn check(&self) -> Result<()> {
        self.request.check()
    }

    /// Cancellation and deadline used by the caller while preparing this revision.
    #[must_use]
    pub const fn request(&self) -> &StorageRequest {
        &self.request
    }

    /// Bind an admitted worker's control while retaining the same revision and limits.
    /// This performs no admission; the caller must derive the worker control first.
    #[must_use]
    pub fn with_request(mut self, request: &StorageRequest) -> Self {
        self.request = request.clone();
        self
    }

    /// Validate a file-local replacement against an exact canonical base.
    /// The topology is retained; changes to declarations or resolution must use
    /// the complete native build instead. Publication fences both base identity
    /// and digest before applying the sparse records.
    /// # Errors
    /// Rejects changed identities, invalid relations, oversized work, or cancellation.
    pub fn replace_files(
        self,
        facts: CanonicalGenerationFacts,
        replacements: crate::GenerationFacts,
    ) -> Result<GraphPublicationInput> {
        self.check()?;
        let policy = self.policy.validate()?;
        let request = &self.request;
        if !matches!(self.identity.base, GraphBase::Current(_)) {
            return Err(RocksError::GenerationChanged);
        }
        let affected = replacements
            .files
            .iter()
            .map(|file| file.file_id.clone())
            .collect();
        let mut changes = ingest::RowCollector::new(&facts, request, policy.maximum_working_bytes)
            .capture(affected)?;
        let limits = crate::GenerationValidationLimits::new(
            policy.maximum_input_bytes,
            policy
                .maximum_input_bytes
                .checked_mul(4)
                .ok_or_else(limit)?,
        )
        .map_err(|_| limit())?;
        let facts = facts
            .replace_files(replacements, limits, || request.check().is_err())
            .map_err(|error| match error {
                crate::GenerationValidationError::Cancelled => {
                    request.check().err().unwrap_or(RocksError::Cancelled)
                }
                crate::GenerationValidationError::RetainedLimit => limit(),
                _ => RocksError::InvalidInput {
                    field: "incremental_facts",
                },
            })?;
        changes.finish(&facts, policy, request)?;
        let mut input = self.resident(facts)?;
        input.retained_bytes = input
            .retained_bytes
            .checked_add(changes.retained_bytes()?)
            .ok_or_else(limit)?;
        if input.retained_bytes > policy.maximum_input_bytes {
            return Err(limit());
        }
        input.changes = Some(changes);
        Ok(input)
    }

    /// Prepare a validated generation inside the caller's existing CPU reservation.
    ///
    /// Large canonical tables are consumed into lossless bounded blocks before
    /// storage admission. Their original validation measurement and digest stay
    /// unchanged. Packed inputs do not retain a warm canonical allocation.
    /// # Errors
    /// Rejects preparation or retained-input bounds, invalid workspace,
    /// cancellation, and expired requests. No database I/O occurs here.
    pub fn prepare(
        self,
        facts: CanonicalGenerationFacts,
        maximum_preparation_bytes: u64,
    ) -> Result<GraphPublicationInput> {
        self.check()?;
        let policy = self.policy.validate()?;
        let request = &self.request;
        let measurement = facts
            .admission_measurement(maximum_preparation_bytes, || request.check().is_err())
            .map_err(|error| match error {
                GenerationMemoryModelError::Cancelled => {
                    request.check().err().unwrap_or(RocksError::Cancelled)
                }
                GenerationMemoryModelError::RetainedLimit => RocksError::Limit {
                    resource: "graph_preparation_bytes",
                },
                GenerationMemoryModelError::MetadataDepth => RocksError::Corrupt {
                    field: "graph_metadata",
                },
            })?;
        ingest::validate_workspace(&facts, policy, request)?;
        if measurement.retained_bytes() <= policy.maximum_input_bytes {
            request.check()?;
            return Ok(self.resident_input(facts, measurement.retained_bytes()));
        }
        let peak = measurement
            .retained_bytes()
            .checked_add(policy.maximum_input_bytes)
            .and_then(|bytes| bytes.checked_add(u64::try_from(policy.maximum_working_bytes).ok()?))
            .and_then(|bytes| bytes.checked_add(96 * 1_048_576))
            .ok_or_else(limit)?;
        if peak > maximum_preparation_bytes {
            return Err(RocksError::Limit {
                resource: "graph_preparation_bytes",
            });
        }
        let graph = packed::PackedGraph::build(facts, policy, request)?;
        let retained_bytes = graph.retained_bytes()?;
        if retained_bytes > policy.maximum_input_bytes {
            return Err(RocksError::Limit {
                resource: "graph_input_bytes",
            });
        }
        Ok(GraphPublicationInput {
            identity: self.identity,
            payload: GraphPayload::Packed(graph),
            changes: None,
            policy,
            retained_bytes,
            run_excludes: Vec::new(),
            maximum_file_bytes: None,
        })
    }

    /// Validate policy and use the immutable retained-size proof before async admission.
    ///
    /// # Errors
    /// Rejects excessive retained memory, cancellation, or expired requests.
    pub fn resident(self, facts: CanonicalGenerationFacts) -> Result<GraphPublicationInput> {
        self.check()?;
        let policy = self.policy.validate()?;
        let request = &self.request;
        let measurement = facts
            .admission_measurement(policy.maximum_input_bytes, || request.check().is_err())
            .map_err(|error| match error {
                GenerationMemoryModelError::Cancelled => {
                    request.check().err().unwrap_or(RocksError::Cancelled)
                }
                GenerationMemoryModelError::RetainedLimit => RocksError::Limit {
                    resource: "graph_input_bytes",
                },
                GenerationMemoryModelError::MetadataDepth => RocksError::Corrupt {
                    field: "graph_metadata",
                },
            })?;
        request.check()?;
        ingest::validate_workspace(&facts, policy, request)?;
        Ok(self.resident_input(facts, measurement.retained_bytes()))
    }

    fn resident_input(
        self,
        facts: CanonicalGenerationFacts,
        retained_bytes: u64,
    ) -> GraphPublicationInput {
        GraphPublicationInput {
            identity: self.identity,
            payload: GraphPayload::Resident(Arc::new(facts)),
            changes: None,
            policy: self.policy,
            retained_bytes,
            run_excludes: Vec::new(),
            maximum_file_bytes: None,
        }
    }
}
