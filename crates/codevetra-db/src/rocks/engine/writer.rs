//! Writer-owned renewal, sealing, publication, and exact-identity failure cleanup.

use super::{
    ACTIVE_KEY, ActiveWriter, CURRENT_KEY, ContentDigest, Duration, GenerationDigestVersion,
    ProjectStore, Result, RevisionState, RevisionWriter, RocksError, StoredRevision, WriteBatch,
    lease_deadline, lifecycle, published_key, put_revision, staging, validate_fence,
    validate_lease_duration,
};

impl ActiveWriter {
    /// Retire this expired reservation in the next writer's atomic batch.
    pub(super) fn abandon(&self, store: &ProjectStore, batch: &mut WriteBatch) -> Result<()> {
        let mut revision = store.revision(self.identity.sequence)?;
        revision.state = RevisionState::Failed;
        put_revision(batch, &store.meta()?, &revision)?;
        batch.delete_cf(
            &store.meta()?,
            staging::byte_count_key(self.identity.sequence),
        );
        store.clear_claims(batch, self.identity.sequence)
    }
}

/// Complete content identity and its exact source admission travel together.
/// The typed graph boundary validates content; the kernel owns lifecycle checks.
pub(in crate::rocks) struct RevisionSeal {
    content_digest: ContentDigest,
    digest_version: GenerationDigestVersion,
    run_excludes: Vec<String>,
    maximum_file_bytes: Option<usize>,
}

impl RevisionSeal {
    pub(in crate::rocks) fn new(
        content_digest: ContentDigest,
        digest_version: GenerationDigestVersion,
    ) -> Self {
        Self {
            content_digest,
            digest_version,
            run_excludes: Vec::new(),
            maximum_file_bytes: None,
        }
    }

    pub(in crate::rocks) fn with_source_admission(
        mut self,
        run_excludes: Vec<String>,
        maximum_file_bytes: Option<usize>,
    ) -> Self {
        self.run_excludes = run_excludes;
        self.maximum_file_bytes = maximum_file_bytes;
        self
    }
}

impl RevisionWriter {
    pub(in crate::rocks) fn seal(
        &self,
        store: &ProjectStore,
        seal: RevisionSeal,
    ) -> Result<StoredRevision> {
        let _serial = store.lock_mutation()?;
        let active = store.lock_writer()?;
        validate_fence(active.as_ref(), self)?;
        let mut revision = store.revision(self.identity.sequence)?;
        if revision.state != RevisionState::Staging {
            return Err(RocksError::Transition);
        }
        revision.content_digest = Some(seal.content_digest);
        revision.digest_version = Some(seal.digest_version);
        revision.run_excludes = seal.run_excludes;
        revision.maximum_file_bytes = seal.maximum_file_bytes;
        revision.state = RevisionState::Ready;
        let mut batch = WriteBatch::default();
        put_revision(&mut batch, &store.meta()?, &revision)?;
        batch.delete_cf(
            &store.meta()?,
            staging::byte_count_key(self.identity.sequence),
        );
        store.clear_claims(&mut batch, self.identity.sequence)?;
        validate_fence(active.as_ref(), self)?;
        store.write(&batch)?;
        Ok(revision)
    }
}

impl RevisionWriter {
    pub(in crate::rocks) fn heartbeat(
        &self,
        store: &ProjectStore,
        duration: Duration,
    ) -> Result<()> {
        validate_lease_duration(duration)?;
        let mut active = store.lock_writer()?;
        store.check_control()?;
        validate_fence(active.as_ref(), self)?;
        // Calculate renewal after lock acquisition: waiting for another
        // lifecycle operation must not consume the newly granted lease.
        let deadline = lease_deadline(duration)?;
        if let Some(active) = active.as_mut() {
            active.expires_at = deadline;
        }
        Ok(())
    }

    pub(in crate::rocks) fn publish(&self, store: &ProjectStore) -> Result<StoredRevision> {
        let _serial = store.lock_mutation()?;
        let mut active = store.lock_writer()?;
        validate_fence(active.as_ref(), self)?;
        let mut revision = store.revision(self.identity.sequence)?;
        if revision.state != RevisionState::Ready {
            return Err(RocksError::Transition);
        }
        if !lifecycle::same_base(
            store.optional_sequence(CURRENT_KEY)?,
            revision.base_sequence,
        ) {
            return Err(RocksError::GenerationChanged);
        }
        let meta = store.meta()?;
        let mut batch = WriteBatch::default();
        if let Some(previous) = revision.base_sequence {
            let mut previous = store.revision(previous)?;
            if previous.state != RevisionState::Current {
                return Err(RocksError::Corrupt {
                    field: "current_revision",
                });
            }
            previous.state = RevisionState::Superseded;
            put_revision(&mut batch, &meta, &previous)?;
        }
        revision.state = RevisionState::Current;
        revision.published_at = Some(jiff::Timestamp::now().to_string());
        put_revision(&mut batch, &meta, &revision)?;
        batch.put_cf(&meta, published_key(self.identity.sequence), []);
        batch.put_cf(&meta, CURRENT_KEY, self.identity.sequence.to_be_bytes());
        batch.delete_cf(&meta, ACTIVE_KEY);
        validate_fence(active.as_ref(), self)?;
        store.write(&batch)?;
        *active = None;
        Ok(revision)
    }

    pub(in crate::rocks) fn fail(&self, store: &ProjectStore) -> Result<()> {
        let _serial = store.lock_mutation()?;
        let mut active = store.lock_writer()?;
        if !active
            .as_ref()
            .is_some_and(|active| lifecycle::owns_writer(&active.identity, &self.identity))
        {
            return Err(RocksError::FenceLost);
        }
        let mut revision = store.revision(self.identity.sequence)?;
        if !matches!(
            revision.state,
            RevisionState::Staging | RevisionState::Ready
        ) {
            return Err(RocksError::Transition);
        }
        revision.state = RevisionState::Failed;
        let meta = store.meta()?;
        let mut batch = WriteBatch::default();
        put_revision(&mut batch, &meta, &revision)?;
        batch.delete_cf(&meta, ACTIVE_KEY);
        batch.delete_cf(&meta, staging::byte_count_key(self.identity.sequence));
        store.clear_claims(&mut batch, self.identity.sequence)?;
        store.write(&batch)?;
        *active = None;
        Ok(())
    }
}
