//! Explicit package-dependency ownership for independently compiled tests and benchmarks.
//!
//! Cargo exposes the package-wide dependency set to every auxiliary target. Underscore imports
//! make that target-granularity ownership visible to the strict compiler dependency audit.

use blake3 as _;
use codevetra_config as _;
use codevetra_db as _;
use codevetra_domain as _;
use codevetra_extract as _;
use codevetra_scip as _;
use globset as _;
use serde as _;
use serde_json as _;
use tempfile as _;
use thiserror as _;
use tokio as _;
