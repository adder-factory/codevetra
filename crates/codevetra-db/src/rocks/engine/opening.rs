//! One requested native opening carries its path, identity and resource policy.

use super::{
    ACTIVE_KEY, ContentDigest, DB, FORMAT_KEY, FORMAT_VERSION, Inner, IteratorMode, PROJECT_KEY,
    ProjectStore, Result, RevisionState, RocksError, StorePolicy, WriteBatch, database_options,
    family_names, operation, ownership, private_directory, put_revision, staging,
};
use crate::rocks::StorageRequest;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
};

/// Requested storage configuration, held intact through asynchronous admission.
/// Construction does not validate policy, inspect the filesystem, or acquire ownership.
#[derive(Clone)]
pub struct ProjectOpening {
    pub(in crate::rocks) root: PathBuf,
    pub(in crate::rocks) identity: ContentDigest,
    pub(in crate::rocks) policy: StorePolicy,
}

impl ProjectOpening {
    /// Bind requested path, expected project identity and resource limits without I/O.
    #[must_use]
    pub const fn new(root: PathBuf, identity: ContentDigest, policy: StorePolicy) -> Self {
        Self {
            root,
            identity,
            policy,
        }
    }

    pub(in crate::rocks) fn open(&self, control: Option<StorageRequest>) -> Result<ProjectStore> {
        if let Some(control) = &control {
            control.check()?;
        }
        let policy = self.policy.validate()?;
        let root = self
            .root
            .canonicalize()
            .map_err(|_| RocksError::Directory)?;
        if !root.is_dir() {
            return Err(RocksError::Directory);
        }
        #[cfg(windows)]
        let admission = {
            let wait = std::time::Duration::from_secs(2);
            let wait = control.as_ref().map_or(Ok(wait), |request| {
                request
                    .remaining_timeout()
                    .map(|remaining| remaining.min(wait))
            })?;
            codevetra_config::StateAdmission::acquire_until(
                &root,
                std::time::Instant::now() + wait,
            )?
        };
        if let Some(control) = &control {
            control.check()?;
        }
        let state = codevetra_config::prepare_project_state(&root)?;
        private_directory(&state)?;
        let primary = ownership::lock_primary(&state)?;
        #[cfg(windows)]
        drop(admission);
        let directory = state.join("data");
        // RocksDB appends slash-separated filenames. Win32 verbatim paths do
        // not normalize those slashes, so use the equivalent ordinary path
        // only when it preserves the exact Windows filename semantics.
        #[cfg(windows)]
        let directory = dunce::simplified(&directory).to_path_buf();
        private_directory(&directory)?;
        // Refuse foreign storage before RocksDB can recover a WAL, create a
        // column family, or write its options/log files.
        ownership::admit(&directory, &self.identity)?;
        ownership::validate_native(&directory, &self.identity)?;
        if let Some(control) = &control {
            control.check()?;
        }
        let (options, families) = database_options(policy);
        let database = DB::open_cf_descriptors(&options, &directory, families)
            .map_err(|_| operation("open"))?;
        let store = ProjectStore {
            inner: Arc::new(Inner {
                database,
                _primary: primary,
                identity: self.identity.clone(),
                mutation: RwLock::new(()),
                stripes: std::array::from_fn(|_| Mutex::new(())),
                writer: Mutex::new(None),
                policy,
            }),
            control,
        };
        self.initialize(&store)?;
        Ok(ProjectStore {
            control: None,
            ..store
        })
    }
}

impl ProjectOpening {
    fn initialize(&self, store: &ProjectStore) -> Result<()> {
        let identity = &self.identity;
        let format = store.get_meta(FORMAT_KEY)?;
        let mut batch = WriteBatch::default();
        let meta = store.meta()?;
        if let Some(format) = format {
            if format != FORMAT_VERSION {
                return Err(RocksError::Format);
            }
            if store.get_meta(PROJECT_KEY)?.as_deref() != Some(identity.as_str().as_bytes()) {
                return Err(RocksError::ProjectMismatch);
            }
        } else {
            // A marker alone never permits adoption of unknown native rows.
            for name in family_names() {
                let family = store
                    .inner
                    .database
                    .cf_handle(name)
                    .ok_or(RocksError::Format)?;
                if store
                    .inner
                    .database
                    .iterator_cf(&family, IteratorMode::Start)
                    .next()
                    .is_some()
                {
                    return Err(RocksError::Format);
                }
            }
            batch.put_cf(&meta, FORMAT_KEY, FORMAT_VERSION);
            batch.put_cf(&meta, PROJECT_KEY, identity.as_str());
        }
        if let Some(sequence) = store.optional_sequence(ACTIVE_KEY)? {
            let mut revision = store.revision(sequence)?;
            if !matches!(
                revision.state,
                RevisionState::Staging | RevisionState::Ready
            ) {
                return Err(RocksError::Corrupt {
                    field: "active_revision",
                });
            }
            revision.state = RevisionState::Failed;
            put_revision(&mut batch, &meta, &revision)?;
            batch.delete_cf(&meta, ACTIVE_KEY);
            batch.delete_cf(&meta, staging::byte_count_key(sequence));
            store.clear_claims(&mut batch, sequence)?;
        }
        store.write(&batch)
    }
}
