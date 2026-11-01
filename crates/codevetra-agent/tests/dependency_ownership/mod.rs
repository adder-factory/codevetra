//! Explicit package-dependency ownership for independently compiled agent integration targets.
//!
//! Cargo exposes the package-wide dependency set to each test crate. Underscore imports make that
//! target-granularity ownership visible without weakening the strict dependency audit.

use blake3 as _;
use codevetra_config as _;
use codevetra_db as _;
use codevetra_domain as _;
use codevetra_extract as _;
use codevetra_indexer as _;
use codevetra_llm as _;
use codevetra_scip as _;
use codevetra_search as _;
use futures_util as _;
use globset as _;
use ignore as _;
#[cfg(unix)]
use libc as _;
use memchr as _;
use num_traits as _;
use regex as _;
use regex_syntax as _;
use serde as _;
use serde_json as _;
use sha2 as _;
use tempfile as _;
use thiserror as _;
use tokio as _;
