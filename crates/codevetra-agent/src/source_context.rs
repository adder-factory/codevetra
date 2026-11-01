use codevetra_db::{
    CurrentFileLookup, CurrentFileRecord, CurrentSymbolRecord, CurrentSymbolSetLookup,
    ProjectSnapshot, StorageError,
};
use codevetra_domain::{
    ContentDigest, GenerationDigestVersion, GenerationId, NormalizedPath, SymbolId,
};
use serde::Serialize;

use crate::{ProjectCancellation, ProjectError, ProjectRuntime};

mod allocation;
pub use allocation::{
    SourceAllocation, SourceAllocationOptions, SourceSelection, SourceSelectionPolicy,
};

const DEFAULT_CONTEXT_LINES: u16 = 3;
const MAXIMUM_CONTEXT_LINES: u16 = 200;
const DEFAULT_EXCERPT_BYTES: usize = 64 * 1_024;
const MINIMUM_EXCERPT_BYTES: usize = 1_024;
const MAXIMUM_EXCERPT_BYTES: usize = 256 * 1_024;
const SOURCE_CONTEXT_ATTEMPTS: usize = 2;
const DEFAULT_FILE_LINE_LIMIT: u16 = 200;
const MAXIMUM_FILE_LINE_LIMIT: u16 = 500;
const MAXIMUM_FILE_EXCERPT_BYTES: usize = 256 * 1_024;
pub(crate) const MAXIMUM_CONTEXT_BATCH: usize = 500;
pub(crate) const MAXIMUM_CAPTURE_BATCH_BYTES: usize = 64 * 1_024 * 1_024;

/// Explicit body selection; outlines contain indexed declaration metadata, not source text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceDetail {
    /// Preserve the body within the declared per-symbol byte bound.
    #[default]
    Full,
    /// Return a bounded prefix with precise omitted-byte accounting.
    Focused,
    /// Return identity/signature metadata without claiming the body was inspected.
    Outline,
}

/// Actual representation, independent of what was requested.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRepresentation {
    /// The entire indexed body is present in verbatim source.
    Full,
    /// Explicitly focused source with omitted body ranges.
    Focused,
    /// Indexed signature/identity only; the body is omitted.
    Outline,
    /// A requested complete body exceeded the admitted source bound.
    Truncated,
    /// Fresh source could not be paired with indexed coordinates.
    Unavailable,
}

/// Half-open file-byte range absent from the delivered source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OmittedSourceRange {
    /// Inclusive UTF-8 byte offset.
    pub start_byte: u64,
    /// Exclusive UTF-8 byte offset.
    pub end_byte: u64,
}

/// Validated line and byte bounds for one source-context request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceContextOptions {
    context_lines: u16,
    maximum_bytes: usize,
    allow_stale_live_source: bool,
    detail: SourceDetail,
}

/// Exact symbol identity and bounded excerpt policy for one source read.
pub struct SourceContextRequest {
    symbol_id: SymbolId,
    options: SourceContextOptions,
}

/// Validated zero-based line window for one exact indexed file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FileSourceOptions {
    line_offset: u32,
    line_limit: u16,
    byte_window: Option<(u64, u32)>,
}

impl FileSourceOptions {
    /// Creates validated source-excerpt limits.
    ///
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `line_limit` is zero or
    /// exceeds the maximum returned source lines.
    pub const fn new(line_offset: u32, line_limit: u16) -> Result<Self, ProjectError> {
        if line_limit == 0 || line_limit > MAXIMUM_FILE_LINE_LIMIT {
            return Err(ProjectError::InvalidOptions);
        }
        Ok(Self {
            line_offset,
            line_limit,
            byte_window: None,
        })
    }

    /// Read a bounded byte window, including continuations inside very long lines.
    /// The starting offset must be a UTF-8 boundary; the end rounds down to one.
    /// # Errors
    /// Rejects zero or more than 256 KiB of source per window.
    pub const fn bytes(offset: u64, limit: u32) -> Result<Self, ProjectError> {
        if limit == 0 || limit > 256 * 1024 {
            return Err(ProjectError::InvalidOptions);
        }
        Ok(Self {
            line_offset: 0,
            line_limit: 1,
            byte_window: Some((offset, limit)),
        })
    }
}

impl Default for FileSourceOptions {
    fn default() -> Self {
        Self {
            line_offset: 0,
            line_limit: DEFAULT_FILE_LINE_LIMIT,
            byte_window: None,
        }
    }
}

/// Exact indexed path and bounded source-window policy.
pub struct FileSourceRequest {
    path: NormalizedPath,
    options: FileSourceOptions,
}

impl FileSourceRequest {
    #[must_use]
    /// Creates a validated file source request.
    pub const fn new(path: NormalizedPath, options: FileSourceOptions) -> Self {
        Self { path, options }
    }
}

/// Bounded UTF-8 source window with exact line and continuation evidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSourceExcerpt {
    start_line: u32,
    end_line: u32,
    total_lines: u32,
    start_byte: u64,
    end_byte: u64,
    text: String,
    truncated: bool,
}

/// Indexed file identity plus live source only when its generation is fresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSourceContext {
    file: CurrentFileRecord,
    live_source_revision: ContentDigest,
    fresh: bool,
    file_fresh: bool,
    excerpt: Option<FileSourceExcerpt>,
}

impl FileSourceContext {
    #[must_use]
    /// Returns whether the live source still matches the indexed digest.
    pub const fn fresh(&self) -> bool {
        self.fresh
    }

    /// Whether this exact file still matches its indexed content hash.
    #[must_use]
    pub const fn file_fresh(&self) -> bool {
        self.file_fresh
    }
}

impl SourceContextRequest {
    /// Bind a current-generation symbol to validated output bounds.
    #[must_use]
    pub const fn new(symbol_id: SymbolId, options: SourceContextOptions) -> Self {
        Self { symbol_id, options }
    }
}

impl SourceContextOptions {
    /// Build symmetric surrounding-line and exact output-byte bounds.
    /// # Errors
    ///
    /// Returns [`ProjectError::InvalidOptions`] when `context_lines` is
    /// excessive or `maximum_bytes` is outside the accepted excerpt range.
    pub const fn new(context_lines: u16, maximum_bytes: usize) -> Result<Self, ProjectError> {
        if context_lines > MAXIMUM_CONTEXT_LINES
            || maximum_bytes < MINIMUM_EXCERPT_BYTES
            || maximum_bytes > MAXIMUM_EXCERPT_BYTES
        {
            return Err(ProjectError::InvalidOptions);
        }
        Ok(Self {
            context_lines,
            maximum_bytes,
            allow_stale_live_source: false,
            detail: SourceDetail::Full,
        })
    }

    /// Explicitly allow a bounded live-disk slice when the indexed line range
    /// belongs to a file whose content no longer matches the generation.
    #[must_use]
    pub const fn with_stale_live_source(mut self, allow: bool) -> Self {
        self.allow_stale_live_source = allow;
        self
    }

    /// Select an explicitly labeled representation without changing freshness policy.
    #[must_use]
    pub const fn with_detail(mut self, detail: SourceDetail) -> Self {
        self.detail = detail;
        self
    }
}

impl Default for SourceContextOptions {
    fn default() -> Self {
        Self {
            context_lines: DEFAULT_CONTEXT_LINES,
            maximum_bytes: DEFAULT_EXCERPT_BYTES,
            allow_stale_live_source: false,
            detail: SourceDetail::Full,
        }
    }
}

/// Bounded, line-numbered UTF-8 source text around one indexed declaration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SourceExcerpt {
    start_line: u32,
    end_line: u32,
    text: String,
    truncated: bool,
    start_byte: u64,
    end_byte: u64,
}

impl SourceExcerpt {
    /// Original inclusive UTF-8 byte offset of the verbatim excerpt.
    #[must_use]
    pub const fn start_byte(&self) -> u64 {
        self.start_byte
    }

    /// First one-based source line included in `text`.
    #[must_use]
    pub const fn start_line(&self) -> u32 {
        self.start_line
    }

    /// Last one-based source line represented in `text`.
    #[must_use]
    pub const fn end_line(&self) -> u32 {
        self.end_line
    }

    /// Exact bounded source text. A final newline is retained when present.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// True when the byte ceiling cut off the requested line range.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }
}

/// Agent-facing declaration metadata plus source only when the live checkout is fresh.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SymbolSourceContext {
    symbol: CurrentSymbolRecord,
    live_source_revision: ContentDigest,
    fresh: bool,
    excerpt: Option<SourceExcerpt>,
    live_source: bool,
    file_content_hash: ContentDigest,
    representation: SourceRepresentation,
    omitted_ranges: Vec<OmittedSourceRange>,
}

impl SymbolSourceContext {
    /// Indexed declaration selected by exact identity.
    #[must_use]
    pub const fn symbol(&self) -> &CurrentSymbolRecord {
        &self.symbol
    }

    /// Whether the complete supported-source manifest matched the published generation.
    #[must_use]
    pub const fn fresh(&self) -> bool {
        self.fresh
    }

    /// Source excerpt, omitted rather than paired with stale indexed line numbers.
    #[must_use]
    pub const fn excerpt(&self) -> Option<&SourceExcerpt> {
        self.excerpt.as_ref()
    }

    /// Whether the excerpt is an explicitly requested approximate live-disk
    /// slice rather than an exact match for the indexed file content.
    #[must_use]
    pub const fn live_source(&self) -> bool {
        self.live_source
    }

    /// Actual full/focused/outline/truncated/unavailable evidence representation.
    #[must_use]
    pub const fn representation(&self) -> SourceRepresentation {
        self.representation
    }

    /// Exact indexed body byte ranges absent from this source window.
    #[must_use]
    pub fn omitted_ranges(&self) -> &[OmittedSourceRange] {
        &self.omitted_ranges
    }

    /// Convert a captured source to a labeled outline without altering identity or freshness.
    pub fn omit_body(&mut self) {
        self.excerpt = None;
        if self.representation != SourceRepresentation::Unavailable {
            self.representation = SourceRepresentation::Outline;
        }
        self.omitted_ranges = vec![OmittedSourceRange {
            start_byte: self.symbol.start_byte(),
            end_byte: self.symbol.end_byte(),
        }];
    }
}

impl ProjectRuntime {
    /// Read up to 500 symbol windows from one bounded source-manifest observation.
    ///
    /// All returned windows use the exact bytes hashed by that observation and
    /// belong to `expected_generation`. Missing symbols are omitted. Captured
    /// source is limited to 64 MiB across distinct files and is released on return.
    /// # Errors
    ///
    /// Returns an error for excessive requests, cancellation, source admission
    /// or capture failure, or a generation change. No partial batch is returned.
    pub async fn source_context_batch_with_cancellation(
        &self,
        expected_generation: &GenerationId,
        requests: Vec<SourceContextRequest>,
        cancellation: ProjectCancellation,
    ) -> Result<Vec<SymbolSourceContext>, ProjectError> {
        self.source_context_batch_if_current_with_cancellation(
            expected_generation,
            requests,
            cancellation,
        )
        .await?
        .ok_or(ProjectError::SourceContextUnavailable)
    }

    /// Capture optional source windows only while `expected_generation` is current.
    ///
    /// `None` means a published generation changed before or during the batch.
    /// No partial batch is returned. Other failures remain errors so callers can
    /// distinguish publication from unavailable or invalid source evidence.
    /// # Errors
    ///
    /// Returns an error for excessive requests, cancellation, a missing index,
    /// storage failure, or source admission/capture failure.
    pub async fn source_context_batch_if_current_with_cancellation(
        &self,
        expected_generation: &GenerationId,
        requests: Vec<SourceContextRequest>,
        cancellation: ProjectCancellation,
    ) -> Result<Option<Vec<SymbolSourceContext>>, ProjectError> {
        if requests.len() > MAXIMUM_CONTEXT_BATCH {
            return Err(ProjectError::InvalidOptions);
        }
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        if requests.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let before =
            crate::cancellable_project_read(&cancellation, self.source_context_snapshot()).await?;
        let current = before
            .current
            .as_ref()
            .ok_or(ProjectError::SourceContextUnavailable)?;
        if &current.generation_id != expected_generation {
            return Ok(None);
        }
        let ids = requests
            .iter()
            .map(|request| request.symbol_id.clone())
            .collect::<Vec<_>>();
        let symbols = crate::cancellable_project_read(&cancellation, async {
            match self
                .database()
                .current_symbols_by_ids(CurrentSymbolSetLookup::new(
                    &before.project_id,
                    expected_generation,
                    &ids,
                ))
                .await
            {
                Ok(symbols) => Ok(Some(symbols)),
                Err(StorageError::CurrentGenerationChanged) => Ok(None),
                Err(_) => Err(ProjectError::SourceContextUnavailable),
            }
        })
        .await?;
        let Some(symbols) = symbols else {
            return Ok(None);
        };
        let paths = symbols.iter().map(|symbol| symbol.path().clone()).collect();
        let source = self
            .scan_source_batch(
                paths,
                current.source_admission.run_excludes(),
                current.source_admission.maximum_file_bytes(),
                cancellation.clone(),
            )
            .await?;
        let fresh = current.source_revision == source.digest.as_str()
            && current.digest_version == GenerationDigestVersion::CURRENT;
        let mut contexts = Vec::new();
        contexts
            .try_reserve_exact(requests.len())
            .map_err(|_| ProjectError::SourceContextUnavailable)?;
        for request in requests {
            if cancellation.is_cancelled() {
                return Err(ProjectError::RequestCancelled);
            }
            let Some(symbol) = symbols
                .iter()
                .find(|symbol| symbol.symbol_id() == &request.symbol_id)
            else {
                continue;
            };
            let file = self
                .source_context_batch_file_if_current(
                    CurrentFileLookup::new(&before.project_id, expected_generation, symbol.path()),
                    &cancellation,
                )
                .await?;
            let Some(file) = file else {
                return Ok(None);
            };
            contexts.push(build_symbol_source_context(SymbolSourceContextInput {
                symbol: symbol.clone(),
                indexed_file: &file,
                source: &source,
                fresh,
                options: request.options,
            })?);
        }
        if !crate::cancellable_project_read(&cancellation, self.generation_is_current(&before))
            .await?
        {
            return Ok(None);
        }
        Ok(Some(contexts))
    }

    async fn source_context_batch_file_if_current(
        &self,
        lookup: CurrentFileLookup<'_>,
        cancellation: &ProjectCancellation,
    ) -> Result<Option<CurrentFileRecord>, ProjectError> {
        crate::cancellable_project_read(cancellation, async {
            match self.database().exact_current_file_by_path(lookup).await {
                Ok(Some(file)) => Ok(Some(file)),
                Err(StorageError::CurrentGenerationChanged) => Ok(None),
                Ok(None) | Err(_) => Err(ProjectError::SourceContextUnavailable),
            }
        })
        .await
    }

    /// Resolve one exact current-generation symbol and return bounded source only
    /// when the complete live checkout still matches that generation.
    /// # Errors
    ///
    /// Returns an error when no stable current generation or exact symbol can
    /// be read, the bounded live file cannot be captured safely, or generation
    /// churn prevents a consistent result after the bounded retry.
    pub async fn source_context(
        &self,
        request: SourceContextRequest,
    ) -> Result<SymbolSourceContext, ProjectError> {
        self.source_context_with_cancellation(request, ProjectCancellation::new())
            .await
    }

    /// Resolve context while polling one caller-owned cancellation signal.
    /// # Errors
    ///
    /// Returns an error when no stable current generation or exact symbol can
    /// be read, the bounded live file cannot be captured safely, generation
    /// churn exhausts the retry, or `cancellation` wins.
    pub async fn source_context_with_cancellation(
        &self,
        request: SourceContextRequest,
        cancellation: ProjectCancellation,
    ) -> Result<SymbolSourceContext, ProjectError> {
        let SourceContextRequest { symbol_id, options } = request;
        for attempt in 0..SOURCE_CONTEXT_ATTEMPTS {
            let can_retry = attempt + 1 < SOURCE_CONTEXT_ATTEMPTS;
            if let Some(context) = self
                .source_context_attempt(SourceContextAttempt {
                    symbol_id: &symbol_id,
                    options,
                    cancellation: cancellation.clone(),
                    can_retry,
                })
                .await?
            {
                return Ok(context);
            }
        }
        Err(ProjectError::SourceContextUnavailable)
    }

    async fn source_context_attempt(
        &self,
        request: SourceContextAttempt<'_>,
    ) -> Result<Option<SymbolSourceContext>, ProjectError> {
        let SourceContextAttempt {
            symbol_id,
            options,
            cancellation,
            can_retry,
        } = request;
        if cancellation.is_cancelled() {
            return Err(ProjectError::RequestCancelled);
        }
        let before = self.source_context_snapshot().await?;
        let current = before
            .current
            .as_ref()
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let Some(symbol) = self
            .source_context_symbol(&before, symbol_id, can_retry)
            .await?
        else {
            return Ok(None);
        };
        if symbol.generation_id() != &current.generation_id {
            return Ok(None);
        }
        let source = self
            .scan_source(Some(symbol.path().clone()), cancellation)
            .await?;
        if !self.generation_is_current(&before).await? {
            return if can_retry {
                Ok(None)
            } else {
                Err(ProjectError::SourceContextUnavailable)
            };
        }
        let indexed_file = self
            .database()
            .exact_current_file_by_path(CurrentFileLookup::new(
                &before.project_id,
                &current.generation_id,
                symbol.path(),
            ))
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let fresh = current.source_revision == source.digest.as_str()
            && current.digest_version == GenerationDigestVersion::CURRENT;
        Ok(Some(build_symbol_source_context(
            SymbolSourceContextInput {
                symbol,
                indexed_file: &indexed_file,
                source: &source,
                fresh,
                options,
            },
        )?))
    }

    async fn source_context_snapshot(&self) -> Result<ProjectSnapshot, ProjectError> {
        self.database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?
            .ok_or(ProjectError::SourceContextUnavailable)
    }

    async fn source_context_symbol(
        &self,
        before: &ProjectSnapshot,
        symbol_id: &SymbolId,
        can_retry: bool,
    ) -> Result<Option<CurrentSymbolRecord>, ProjectError> {
        let current = before
            .current
            .as_ref()
            .ok_or(ProjectError::SourceContextUnavailable)?;
        let symbols = self
            .database()
            .current_symbols_by_ids(CurrentSymbolSetLookup::new(
                &before.project_id,
                &current.generation_id,
                std::slice::from_ref(symbol_id),
            ))
            .await;
        let mut symbols = match symbols {
            Ok(symbols) => symbols,
            Err(StorageError::CurrentGenerationChanged) if can_retry => return Ok(None),
            Err(_) => return Err(ProjectError::SourceContextUnavailable),
        };
        let Some(symbol) = symbols.pop() else {
            if self.generation_is_current(before).await? {
                return Err(ProjectError::SymbolNotFound);
            }
            return Ok(None);
        };
        Ok(Some(symbol))
    }

    /// Read a bounded line window from one exact indexed file when source is current.
    /// # Errors
    ///
    /// Returns an error when no stable current generation or exact file can be
    /// read, the bounded live file cannot be captured safely, generation churn
    /// exhausts the retry, or `cancellation` wins.
    pub async fn file_source_with_cancellation(
        &self,
        request: FileSourceRequest,
        cancellation: ProjectCancellation,
    ) -> Result<FileSourceContext, ProjectError> {
        let FileSourceRequest { path, options } = request;
        for attempt in 0..SOURCE_CONTEXT_ATTEMPTS {
            if cancellation.is_cancelled() {
                return Err(ProjectError::RequestCancelled);
            }
            let Some(before) = self
                .database()
                .project_snapshot_by_root(self.root_identity())
                .await
                .map_err(|_| ProjectError::SourceContextUnavailable)?
            else {
                return Err(ProjectError::SourceContextUnavailable);
            };
            let Some(current) = before.current.as_ref() else {
                return Err(ProjectError::SourceContextUnavailable);
            };
            let file = self
                .database()
                .exact_current_file_by_path(CurrentFileLookup::new(
                    &before.project_id,
                    &current.generation_id,
                    &path,
                ))
                .await;
            let file = match file {
                Ok(Some(file)) => file,
                Ok(None) if self.generation_is_current(&before).await? => {
                    return Err(ProjectError::FileNotFound);
                }
                Ok(None) | Err(StorageError::CurrentGenerationChanged)
                    if attempt + 1 < SOURCE_CONTEXT_ATTEMPTS =>
                {
                    continue;
                }
                Ok(None) | Err(_) => return Err(ProjectError::SourceContextUnavailable),
            };
            let source = self
                .scan_source(Some(path.clone()), cancellation.clone())
                .await?;
            if !self.generation_is_current(&before).await? {
                if attempt + 1 < SOURCE_CONTEXT_ATTEMPTS {
                    continue;
                }
                return Err(ProjectError::SourceContextUnavailable);
            }
            let fresh = current.source_revision == source.digest.as_str()
                && current.digest_version == GenerationDigestVersion::CURRENT;
            let capture = source.captures.get(&path);
            let file_fresh =
                capture.is_some_and(|capture| &capture.content_hash == file.content_hash());
            let excerpt = if file_fresh {
                let captured = capture
                    .map(|capture| capture.source.as_ref())
                    .ok_or(ProjectError::SourceContextUnavailable)?;
                Some(extract_file_excerpt(captured, options)?)
            } else {
                None
            };
            return Ok(FileSourceContext {
                file,
                live_source_revision: source.digest,
                fresh,
                file_fresh,
                excerpt,
            });
        }
        Err(ProjectError::SourceContextUnavailable)
    }

    async fn generation_is_current(
        &self,
        expected: &codevetra_db::ProjectSnapshot,
    ) -> Result<bool, ProjectError> {
        let observed = self
            .database()
            .project_snapshot_by_root(self.root_identity())
            .await
            .map_err(|_| ProjectError::SourceContextUnavailable)?;
        Ok(observed.as_ref().is_some_and(|snapshot| {
            snapshot.project_id == expected.project_id
                && snapshot
                    .current
                    .as_ref()
                    .map(|current| (&current.generation_id, current.source_revision.as_str()))
                    == expected
                        .current
                        .as_ref()
                        .map(|current| (&current.generation_id, current.source_revision.as_str()))
        }))
    }
}

struct SourceContextAttempt<'a> {
    symbol_id: &'a SymbolId,
    options: SourceContextOptions,
    cancellation: ProjectCancellation,
    can_retry: bool,
}

struct SymbolSourceContextInput<'a> {
    symbol: CurrentSymbolRecord,
    indexed_file: &'a CurrentFileRecord,
    source: &'a crate::SourceRevision,
    fresh: bool,
    options: SourceContextOptions,
}

fn build_symbol_source_context(
    input: SymbolSourceContextInput<'_>,
) -> Result<SymbolSourceContext, ProjectError> {
    let SymbolSourceContextInput {
        symbol,
        indexed_file,
        source,
        fresh,
        options,
    } = input;
    let capture = source.captures.get(symbol.path());
    let file_fresh =
        capture.is_some_and(|capture| &capture.content_hash == indexed_file.content_hash());
    let live_source = !file_fresh && options.allow_stale_live_source;
    let excerpt = if (file_fresh || live_source) && options.detail != SourceDetail::Outline {
        let captured = capture
            .map(|capture| capture.source.as_ref())
            .ok_or(ProjectError::SourceContextUnavailable)?;
        Some(extract_excerpt(ExcerptRequest {
            source: captured,
            symbol_start: symbol.start_line(),
            symbol_end: symbol.end_line(),
            options,
        })?)
    } else {
        None
    };
    let (representation, omitted_ranges) =
        source_representation(&symbol, excerpt.as_ref(), options.detail, file_fresh);
    Ok(SymbolSourceContext {
        symbol,
        live_source_revision: source.digest.clone(),
        fresh,
        excerpt,
        live_source,
        file_content_hash: indexed_file.content_hash().clone(),
        representation,
        omitted_ranges,
    })
}

fn source_representation(
    symbol: &CurrentSymbolRecord,
    excerpt: Option<&SourceExcerpt>,
    detail: SourceDetail,
    file_fresh: bool,
) -> (SourceRepresentation, Vec<OmittedSourceRange>) {
    let start = symbol.start_byte();
    let end = symbol.end_byte();
    let mut omitted = Vec::new();
    if let Some(excerpt) = excerpt.filter(|_| file_fresh) {
        let included_start = excerpt.start_byte.clamp(start, end);
        let included_end = excerpt.end_byte.clamp(start, end);
        if start < included_start {
            omitted.push(OmittedSourceRange {
                start_byte: start,
                end_byte: included_start,
            });
        }
        if included_end < end {
            omitted.push(OmittedSourceRange {
                start_byte: included_end,
                end_byte: end,
            });
        }
    } else {
        omitted.push(OmittedSourceRange {
            start_byte: start,
            end_byte: end,
        });
    }
    let representation = if !file_fresh {
        SourceRepresentation::Unavailable
    } else if detail == SourceDetail::Outline {
        SourceRepresentation::Outline
    } else if omitted.is_empty() {
        SourceRepresentation::Full
    } else if detail == SourceDetail::Focused {
        SourceRepresentation::Focused
    } else {
        SourceRepresentation::Truncated
    };
    (representation, omitted)
}

fn extract_file_excerpt(
    source: &str,
    options: FileSourceOptions,
) -> Result<FileSourceExcerpt, ProjectError> {
    let total_lines = u32::try_from(source.split_inclusive('\n').count())
        .map_err(|_| ProjectError::SourceContextUnavailable)?;
    let (start, end) = if let Some((offset, limit)) = options.byte_window {
        let start = usize::try_from(offset).map_err(|_| ProjectError::InvalidOptions)?;
        if start > source.len() || !source.is_char_boundary(start) {
            return Err(ProjectError::InvalidOptions);
        }
        let end =
            source.floor_char_boundary(start.saturating_add(limit as usize).min(source.len()));
        if start < source.len() && start == end {
            return Err(ProjectError::InvalidOptions);
        }
        (start, end)
    } else {
        let start = source
            .split_inclusive('\n')
            .take(options.line_offset as usize)
            .map(str::len)
            .sum::<usize>();
        let length = source[start..]
            .split_inclusive('\n')
            .take(usize::from(options.line_limit))
            .map(str::len)
            .sum::<usize>();
        (
            start,
            source
                .floor_char_boundary(start.saturating_add(length.min(MAXIMUM_FILE_EXCERPT_BYTES))),
        )
    };
    let start_line = u32::try_from(
        source[..start]
            .bytes()
            .filter(|byte| *byte == b'\n')
            .count(),
    )
    .map_err(|_| ProjectError::SourceContextUnavailable)?
    .saturating_add(1);
    let text = source[start..end].to_owned();
    let end_line = start_line.saturating_add(
        u32::try_from(text.split_inclusive('\n').count())
            .map_err(|_| ProjectError::SourceContextUnavailable)?
            .saturating_sub(1),
    );
    Ok(FileSourceExcerpt {
        start_line,
        end_line,
        total_lines,
        start_byte: start as u64,
        end_byte: end as u64,
        text,
        truncated: end < source.len(),
    })
}

#[derive(Clone, Copy)]
struct ExcerptRequest<'source> {
    source: &'source str,
    symbol_start: u32,
    symbol_end: u32,
    options: SourceContextOptions,
}

fn extract_excerpt(input: ExcerptRequest<'_>) -> Result<SourceExcerpt, ProjectError> {
    let ExcerptRequest {
        source,
        symbol_start,
        symbol_end,
        options,
    } = input;
    let context = u32::from(options.context_lines);
    let requested_start = symbol_start.saturating_sub(context).max(1);
    let requested_end = symbol_end.saturating_add(context);
    let mut text = String::new();
    text.try_reserve(options.maximum_bytes)
        .map_err(|_| ProjectError::SourceContextUnavailable)?;
    let mut end_line = requested_start;
    let mut truncated = false;
    let mut start_byte = 0_u64;
    for (index, line) in source.split_inclusive('\n').enumerate() {
        let line_number = u32::try_from(index)
            .ok()
            .and_then(|value| value.checked_add(1))
            .ok_or(ProjectError::SourceContextUnavailable)?;
        if line_number < requested_start {
            start_byte = start_byte
                .checked_add(
                    u64::try_from(line.len())
                        .map_err(|_| ProjectError::SourceContextUnavailable)?,
                )
                .ok_or(ProjectError::SourceContextUnavailable)?;
            continue;
        }
        if line_number > requested_end {
            break;
        }
        let remaining = options.maximum_bytes.saturating_sub(text.len());
        if line.len() > remaining {
            let boundary = line.floor_char_boundary(remaining);
            text.push_str(&line[..boundary]);
            end_line = line_number;
            truncated = true;
            break;
        }
        text.push_str(line);
        end_line = line_number;
    }
    if text.is_empty() {
        return Err(ProjectError::SourceContextUnavailable);
    }
    Ok(SourceExcerpt {
        start_byte,
        end_byte: start_byte
            .checked_add(
                u64::try_from(text.len()).map_err(|_| ProjectError::SourceContextUnavailable)?,
            )
            .ok_or(ProjectError::SourceContextUnavailable)?,
        start_line: requested_start,
        end_line,
        text,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE_MAXIMUM_BYTES: usize = MINIMUM_EXCERPT_BYTES;
    const FIXTURE_SYMBOL_LINE: u32 = 3;
    const FIXTURE_CONTEXT_LINES: u16 = 1;

    #[test]
    fn excerpt_preserves_lines_and_reports_utf8_safe_truncation() {
        let source = "zero\none\n🦀two\nthree\nfour\n";
        let full = extract_excerpt(ExcerptRequest {
            source,
            symbol_start: FIXTURE_SYMBOL_LINE,
            symbol_end: FIXTURE_SYMBOL_LINE,
            options: SourceContextOptions::new(FIXTURE_CONTEXT_LINES, FIXTURE_MAXIMUM_BYTES)
                .unwrap_or_else(|error| panic!("source options failed: {error}")),
        })
        .unwrap_or_else(|error| panic!("source excerpt failed: {error}"));
        assert_eq!(full.start_line(), 2);
        assert_eq!(full.end_line(), 4);
        assert_eq!(full.text(), "one\n🦀two\nthree\n");
        assert!(!full.truncated());

        let long = format!("{}🦀", "x".repeat(FIXTURE_MAXIMUM_BYTES));
        let truncated = extract_excerpt(ExcerptRequest {
            source: &long,
            symbol_start: 1,
            symbol_end: 1,
            options: SourceContextOptions::new(0, FIXTURE_MAXIMUM_BYTES)
                .unwrap_or_else(|error| panic!("source options failed: {error}")),
        })
        .unwrap_or_else(|error| panic!("truncated excerpt failed: {error}"));
        assert_eq!(truncated.text().len(), FIXTURE_MAXIMUM_BYTES);
        assert_eq!(truncated.text(), "x".repeat(FIXTURE_MAXIMUM_BYTES));
        assert!(truncated.truncated());
    }

    #[test]
    fn source_context_options_reject_unbounded_output() {
        assert_eq!(
            SourceContextOptions::new(
                MAXIMUM_CONTEXT_LINES.saturating_add(1),
                DEFAULT_EXCERPT_BYTES
            ),
            Err(ProjectError::InvalidOptions)
        );
        assert_eq!(
            SourceContextOptions::new(
                DEFAULT_CONTEXT_LINES,
                MAXIMUM_EXCERPT_BYTES.saturating_add(1)
            ),
            Err(ProjectError::InvalidOptions)
        );
    }

    #[test]
    fn captured_excerpt_bytes_are_the_exact_bytes_used_by_the_manifest() {
        let directory =
            tempfile::tempdir().unwrap_or_else(|error| panic!("tempdir failed: {error}"));
        let file = directory.path().join("service.rs");
        std::fs::write(&file, "pub fn before() {}\n")
            .unwrap_or_else(|error| panic!("fixture write failed: {error}"));
        let path = codevetra_domain::NormalizedPath::parse("service.rs")
            .unwrap_or_else(|error| panic!("fixture path failed: {error}"));
        let revision = crate::source_revision_with_capture(directory.path(), Some(&path), || false)
            .unwrap_or_else(|error| panic!("source revision failed: {error}"));

        std::fs::write(&file, "pub fn after() {}\n")
            .unwrap_or_else(|error| panic!("fixture mutation failed: {error}"));
        let captured = revision.captures.get(&path).map_or_else(
            || panic!("target source was not captured"),
            |capture| capture.source.as_ref(),
        );
        let excerpt = extract_excerpt(ExcerptRequest {
            source: captured,
            symbol_start: 1,
            symbol_end: 1,
            options: SourceContextOptions::new(0, FIXTURE_MAXIMUM_BYTES)
                .unwrap_or_else(|error| panic!("source options failed: {error}")),
        })
        .unwrap_or_else(|error| panic!("source excerpt failed: {error}"));
        assert_eq!(excerpt.text(), "pub fn before() {}\n");
        assert!(!excerpt.text().contains("after"));
    }

    #[test]
    fn file_excerpt_uses_zero_based_offsets_and_reports_continuation() {
        let excerpt = extract_file_excerpt(
            "zero\none\ntwo\nthree\n",
            FileSourceOptions::new(1, 2)
                .unwrap_or_else(|error| panic!("file options failed: {error}")),
        )
        .unwrap_or_else(|error| panic!("file excerpt failed: {error}"));
        assert_eq!(excerpt.start_line, 2);
        assert_eq!(excerpt.end_line, 3);
        assert_eq!(excerpt.total_lines, 4);
        assert_eq!(excerpt.text, "one\ntwo\n");
        assert!(excerpt.truncated);
    }
}
