use std::time::{Duration, Instant};

use super::{Result, RocksError};

const MIB: usize = 1_048_576;

/// Resource policy shared by every column family in one project database.
///
/// Cache and memtable budgets are soft engine limits. Iterators, request
/// buffers, native metadata, and the filesystem cache require additional RAM.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StorePolicy {
    /// Shared block-cache capacity, including charged memtable allocations.
    pub cache_bytes: usize,
    /// Shared write-buffer budget across all column families.
    pub memtable_bytes: usize,
    /// Maximum encoded logical bytes in one foreground atomic write.
    pub maximum_batch_bytes: usize,
    /// Maximum number of changes in one foreground batch.
    pub maximum_batch_rows: usize,
}

impl Default for StorePolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl StorePolicy {
    const DEFAULT: Self = Self {
        cache_bytes: 64 * MIB,
        memtable_bytes: 32 * MIB,
        maximum_batch_bytes: 64 * MIB,
        maximum_batch_rows: 100_000,
    };

    pub(super) fn validate(self) -> Result<Self> {
        if !super::arithmetic::valid_store_policy(&self) {
            return Err(RocksError::Limit { resource: "policy" });
        }
        Ok(self)
    }
}

/// Generation fence and write capacity admitted for one state transaction.
///
/// Read-only access never acquires the mutation lock. Writable access retains
/// both row and byte limits through worker reservation and WAL publication.
#[derive(Clone, Debug)]
pub struct StateAccess {
    pub(super) expected: Option<codevetra_domain::GenerationId>,
    pub(super) maximum_write_bytes: u32,
    pub(super) maximum_write_rows: u32,
}

impl StateAccess {
    /// Read project state, optionally fenced to the current graph generation.
    #[must_use]
    pub fn read_only(expected: Option<codevetra_domain::GenerationId>) -> Self {
        Self::write(expected, 0, 0)
    }

    /// Bound writes by encoded bytes and changed rows, with an optional graph fence.
    /// Store policy and worker admission validate these limits before execution.
    #[must_use]
    pub fn write(
        expected: Option<codevetra_domain::GenerationId>,
        maximum_write_bytes: u32,
        maximum_write_rows: u32,
    ) -> Self {
        Self {
            expected,
            maximum_write_bytes,
            maximum_write_rows,
        }
    }
}

/// A consumable read allowance covering physical records, bytes, and time.
///
/// Callers reuse the same allowance across every storage lookup in a request.
pub struct ReadBudget {
    remaining_rows: usize,
    remaining_bytes: usize,
    workspace_bytes: usize,
    deadline: Instant,
    control: Option<super::StorageRequest>,
}

impl ReadBudget {
    const MAXIMUM_TIMEOUT: Duration = Duration::from_secs(300);
    const MAXIMUM_STREAM_ROWS: usize = 2_000_000;
    const MAXIMUM_STREAM_BYTES: usize = 8 * 1024 * MIB;
    // Both audited streaming consumers use this fixed retained workspace.
    const STREAMING_WORKSPACE_BYTES: usize = 64 * MIB;

    /// Admit a finite request. No zero/unbounded deadline is accepted.
    ///
    /// # Errors
    /// Returns a limit error for invalid rows, bytes, or deadlines.
    pub fn new(rows: usize, bytes: usize, timeout: Duration) -> Result<Self> {
        if rows == 0 || bytes == 0 || timeout.is_zero() || timeout > Self::MAXIMUM_TIMEOUT {
            return Err(RocksError::Limit {
                resource: "read_policy",
            });
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or(RocksError::Limit {
                resource: "deadline",
            })?;
        Ok(Self {
            remaining_rows: rows,
            remaining_bytes: bytes,
            workspace_bytes: bytes,
            deadline,
            control: None,
        })
    }

    /// Streaming operators retain a separately bounded workspace while charging
    /// every physical read, including repeated reads, against a finite total.
    /// Only audited native operators may use this admission contract.
    pub(crate) fn streaming(rows: usize, bytes: usize, timeout: Duration) -> Result<Self> {
        if rows > Self::MAXIMUM_STREAM_ROWS || bytes > Self::MAXIMUM_STREAM_BYTES {
            return Err(RocksError::Limit {
                resource: "streaming_read_policy",
            });
        }
        let mut budget = Self::new(rows, bytes, timeout)?;
        budget.workspace_bytes = Self::STREAMING_WORKSPACE_BYTES;
        Ok(budget)
    }

    pub(crate) fn with_control(mut self, control: super::StorageRequest) -> Self {
        self.control = Some(control);
        self
    }

    pub(super) fn reservation_bytes(&self) -> Result<u32> {
        super::arithmetic::worker_reservation(
            self.workspace_bytes,
            super::engine::MAXIMUM_RECORD_BYTES,
        )
        .ok_or(RocksError::Limit {
            resource: "worker_bytes",
        })
    }

    /// Check cancellation and time during CPU work without consuming a row.
    /// # Errors
    /// Returns cancellation, closure, or deadline errors.
    pub fn checkpoint(&self) -> Result<()> {
        if let Some(control) = &self.control {
            control.check()?;
        }
        if Instant::now() >= self.deadline {
            return Err(RocksError::Limit {
                resource: "read_deadline",
            });
        }
        Ok(())
    }

    pub(super) fn charge(&mut self, bytes: usize) -> Result<()> {
        self.checkpoint()?;
        self.remaining_rows = self
            .remaining_rows
            .checked_sub(1)
            .ok_or(RocksError::Limit {
                resource: "read_rows",
            })?;
        self.remaining_bytes =
            self.remaining_bytes
                .checked_sub(bytes)
                .ok_or(RocksError::Limit {
                    resource: "read_bytes",
                })?;
        Ok(())
    }
}
