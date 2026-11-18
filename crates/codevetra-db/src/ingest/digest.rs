use std::mem::size_of;

use blake3::Hasher;
use codevetra_domain::{ContentDigest, GenerationDigestVersion};

use super::model::{
    CanonicalSearchDocument, EdgeInput, FileInput, NumericalSiteInput, ReferenceInput, SymbolInput,
    ValidatedFactTables,
};

const DIGEST_V1_TO_V6_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v4";
const DIGEST_V7_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v5";
const DIGEST_V8_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v6";
const DIGEST_V9_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v7";
const DIGEST_V10_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v8";
const DIGEST_V11_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v9";
const DIGEST_V12_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v10";
const DIGEST_V13_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v11";
const DIGEST_V14_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v12";
const DIGEST_V15_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v13";
const DIGEST_V16_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v14";
const DIGEST_V17_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v15";
const DIGEST_V18_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v16";
const DIGEST_V19_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v17";
const DIGEST_V20_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v18";
const DIGEST_V21_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v19";
const DIGEST_V22_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v20";
const DIGEST_V23_DOMAIN: &[u8] = b"cartograph-v2-logical-generation-v21";
const DIGEST_V24_DOMAIN: &[u8] = b"cartograph-v3-logical-generation-v24";
const DIGEST_V25_DOMAIN: &[u8] = b"cartograph-v3-logical-generation-v25";
const DIGEST_V26_DOMAIN: &[u8] = b"cartograph-v3-logical-generation-v26";
const DIGEST_V27_DOMAIN: &[u8] = b"codevetra-v4-logical-generation-v27";
const DIGEST_V28_DOMAIN: &[u8] = b"codevetra-v4-logical-generation-v28";
const DIGEST_V29_DOMAIN: &[u8] = b"codevetra-v4-logical-generation-v29";
const DIGEST_V30_DOMAIN: &[u8] = b"codevetra-v4-logical-generation-v30";
const DIGEST_V31_DOMAIN: &[u8] = b"codevetra-v4-logical-generation-v31";
const DIGEST_V32_DOMAIN: &[u8] = b"codevetra-v4-logical-generation-v32";
const DIGEST_V33_DOMAIN: &[u8] = b"codevetra-v4-logical-generation-v33";
const DIGEST_V34_DOMAIN: &[u8] = b"codevetra-v4-logical-generation-v34";

/// Each digest contract's hash domain, in [`GenerationDigestVersion::ALL`]
/// order. The length follows `ALL`, so a new contract does not compile until
/// it names its own domain instead of silently reusing an older one.
const DIGEST_DOMAINS: [&[u8]; GenerationDigestVersion::ALL.len()] = [
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V1_TO_V6_DOMAIN,
    DIGEST_V7_DOMAIN,
    DIGEST_V8_DOMAIN,
    DIGEST_V9_DOMAIN,
    DIGEST_V10_DOMAIN,
    DIGEST_V11_DOMAIN,
    DIGEST_V12_DOMAIN,
    DIGEST_V13_DOMAIN,
    DIGEST_V14_DOMAIN,
    DIGEST_V15_DOMAIN,
    DIGEST_V16_DOMAIN,
    DIGEST_V17_DOMAIN,
    DIGEST_V18_DOMAIN,
    DIGEST_V19_DOMAIN,
    DIGEST_V20_DOMAIN,
    DIGEST_V21_DOMAIN,
    DIGEST_V22_DOMAIN,
    DIGEST_V23_DOMAIN,
    DIGEST_V24_DOMAIN,
    DIGEST_V25_DOMAIN,
    DIGEST_V26_DOMAIN,
    DIGEST_V27_DOMAIN,
    DIGEST_V28_DOMAIN,
    DIGEST_V29_DOMAIN,
    DIGEST_V30_DOMAIN,
    DIGEST_V31_DOMAIN,
    DIGEST_V32_DOMAIN,
    DIGEST_V33_DOMAIN,
    DIGEST_V34_DOMAIN,
];

pub(super) fn logical_digest<Cancel>(
    facts: &ValidatedFactTables,
    version: GenerationDigestVersion,
    mut cancelled: Cancel,
) -> Result<ContentDigest, ()>
where
    Cancel: FnMut() -> bool,
{
    let mut digest = CanonicalDigest::for_generation(version);
    digest.table(&facts.files, &mut cancelled)?;
    digest.table(&facts.symbols, &mut cancelled)?;
    digest.table(&facts.edges, &mut cancelled)?;
    digest.table(&facts.references, &mut cancelled)?;
    if digest_includes_numerical_sites(version) {
        digest.table(&facts.numerical_sites, &mut cancelled)?;
    }
    digest.table(&facts.documents, &mut cancelled)?;
    if cancelled() {
        return Err(());
    }
    Ok(digest.finish().0)
}

const fn digest_includes_numerical_sites(version: GenerationDigestVersion) -> bool {
    matches!(
        version,
        GenerationDigestVersion::V7
            | GenerationDigestVersion::V8
            | GenerationDigestVersion::V9
            | GenerationDigestVersion::V10
            | GenerationDigestVersion::V11
            | GenerationDigestVersion::V12
            | GenerationDigestVersion::V13
            | GenerationDigestVersion::V14
            | GenerationDigestVersion::V15
            | GenerationDigestVersion::V16
            | GenerationDigestVersion::V17
            | GenerationDigestVersion::V18
            | GenerationDigestVersion::V19
            | GenerationDigestVersion::V20
            | GenerationDigestVersion::V21
            | GenerationDigestVersion::V22
            | GenerationDigestVersion::V23
            | GenerationDigestVersion::V24
            | GenerationDigestVersion::V25
            | GenerationDigestVersion::V26
            | GenerationDigestVersion::V27
            | GenerationDigestVersion::V28
            | GenerationDigestVersion::V29
            | GenerationDigestVersion::V30
            | GenerationDigestVersion::V31
            | GenerationDigestVersion::V32
            | GenerationDigestVersion::V33
            | GenerationDigestVersion::V34
    )
}

/// A table's encoded name and row layout are selected together by its type.
trait CanonicalRow {
    const SECTION: &'static str;
    fn encode(&self, digest: &mut CanonicalDigest);
}

impl CanonicalRow for FileInput {
    const SECTION: &'static str = "files";
    fn encode(&self, digest: &mut CanonicalDigest) {
        digest.text(self.file_id.as_str());
        digest.text(&self.normalized_path);
        digest.text(&self.language);
        digest.text(self.content_hash.as_str());
        digest.u64(self.byte_size);
        digest.text(self.parse_status.as_str());
    }
}

impl CanonicalRow for SymbolInput {
    const SECTION: &'static str = "symbols";
    fn encode(&self, digest: &mut CanonicalDigest) {
        digest.text(self.symbol_id.as_str());
        digest.text(self.file_id.as_str());
        digest.text(&self.symbol_kind);
        digest.text(&self.qualified_name);
        digest.text(&self.signature);
        digest.u64(self.start_byte);
        digest.u64(self.end_byte);
        digest.u32(self.start_line);
        digest.u32(self.end_line);
        digest.text(self.structural_digest.as_str());
        digest.optional_text(self.visibility.map(codevetra_domain::Visibility::as_str));
        digest.boolean(self.export.exported);
        digest.boolean(self.export.default_export);
        digest.boolean(self.execution.async_symbol);
        digest.boolean(self.execution.static_member);
        digest.boolean(self.declaration_only);
    }
}

impl CanonicalRow for EdgeInput {
    const SECTION: &'static str = "edges";
    fn encode(&self, digest: &mut CanonicalDigest) {
        digest.text(self.source_symbol_id.as_str());
        digest.text(self.target_symbol_id.as_str());
        digest.text(self.kind.as_str());
        digest.u32(self.confidence.to_bits());
        digest.text(&self.provenance);
        digest.u32(self.site_count);
    }
}

impl CanonicalRow for ReferenceInput {
    const SECTION: &'static str = "references";
    fn encode(&self, digest: &mut CanonicalDigest) {
        digest.text(self.file_id.as_str());
        digest.optional_text(
            self.owner_symbol_id
                .as_ref()
                .map(codevetra_domain::SymbolId::as_str),
        );
        digest.optional_text(
            self.target_symbol_id
                .as_ref()
                .map(codevetra_domain::SymbolId::as_str),
        );
        digest.text(&self.reference_name);
        digest.text(&self.reference_kind);
        digest.u64(self.start_byte);
        digest.u64(self.end_byte);
        digest.u32(self.confidence.to_bits());
        digest.text(&self.resolution_provenance);
        digest.u32(self.site_count);
        digest.text(self.span_precision.as_str());
    }
}

impl CanonicalRow for NumericalSiteInput {
    const SECTION: &'static str = "numerical_sites";
    fn encode(&self, digest: &mut CanonicalDigest) {
        digest.text(self.site_id.as_str());
        digest.text(self.file_id.as_str());
        digest.optional_text(
            self.owner_symbol_id
                .as_ref()
                .map(codevetra_domain::SymbolId::as_str),
        );
        digest.u64(self.start_byte);
        digest.u64(self.end_byte);
        digest.u32(self.start_line);
        digest.u32(self.end_line);
        digest.text(&self.operation);
        digest.text(&self.hazard);
        digest.text(&self.precision);
        digest.text(self.expression_digest.as_str());
        digest.u32(self.confidence_ppm);
        digest.text(&self.provenance);
        digest.text(&self.evidence_level);
        digest.text(&self.unknowns);
    }
}

impl CanonicalRow for CanonicalSearchDocument {
    const SECTION: &'static str = "search_documents";
    fn encode(&self, digest: &mut CanonicalDigest) {
        digest.text(self.document_id.as_str());
        digest.optional_text(self.file_id.as_ref().map(codevetra_domain::FileId::as_str));
        digest.optional_text(
            self.symbol_id
                .as_ref()
                .map(codevetra_domain::SymbolId::as_str),
        );
        digest.text(&self.path);
        digest.text(&self.language);
        digest.text(self.kind.as_str());
        digest.text(&self.qualified_name);
        digest.text(&self.code);
        digest.text(&self.natural_text);
        digest.text(&self.metadata_json);
    }
}

fn usize_to_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

struct CanonicalDigest {
    hasher: Hasher,
    encoded_bytes: u64,
    // Bound the transient stream workspace while letting BLAKE3 process whole
    // chunks. The logical bytes, domains and row-level cancellation stay exact.
    buffer: [u8; 8192],
    buffered: usize,
}

impl CanonicalDigest {
    fn for_generation(version: GenerationDigestVersion) -> Self {
        // `ALL` lists every contract, so the fallback is never taken.
        let domain = GenerationDigestVersion::ALL
            .iter()
            .zip(DIGEST_DOMAINS)
            .find_map(|(candidate, domain)| (*candidate == version).then_some(domain))
            .unwrap_or(DIGEST_V1_TO_V6_DOMAIN);
        Self::new(domain)
    }

    fn table<Row: CanonicalRow>(
        &mut self,
        rows: &[Row],
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<(), ()> {
        self.section(Row::SECTION, usize_to_u64(rows.len()));
        for row in rows {
            if cancelled() {
                return Err(());
            }
            row.encode(self);
        }
        Ok(())
    }

    fn new(domain: &[u8]) -> Self {
        let mut hasher = Hasher::new();
        hasher.update(domain);
        Self {
            hasher,
            encoded_bytes: 0,
            buffer: [0; 8192],
            buffered: 0,
        }
    }

    fn section(&mut self, name: &str, count: u64) {
        self.text(name);
        self.text(itoa::Buffer::new().format(count));
    }

    fn text(&mut self, value: &str) {
        let mut decimal = itoa::Buffer::new();
        let length = decimal.format(value.len());
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(length.len()))
            .saturating_add(1)
            .saturating_add(usize_to_u64(value.len()));
        self.bytes(length.as_bytes());
        self.bytes(&[0]);
        self.bytes(value.as_bytes());
    }

    fn optional_text(&mut self, value: Option<&str>) {
        self.encoded_bytes = self.encoded_bytes.saturating_add(1);
        match value {
            Some(value) => {
                self.bytes(&[1]);
                self.text(value);
            }
            None => {
                self.bytes(&[0]);
            }
        }
    }

    fn u64(&mut self, value: u64) {
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(size_of::<u64>()));
        self.bytes(&value.to_be_bytes());
    }

    fn u32(&mut self, value: u32) {
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_add(usize_to_u64(size_of::<u32>()));
        self.bytes(&value.to_be_bytes());
    }

    fn boolean(&mut self, value: bool) {
        self.encoded_bytes = self.encoded_bytes.saturating_add(1);
        self.bytes(&[u8::from(value)]);
    }

    fn bytes(&mut self, mut bytes: &[u8]) {
        if bytes.len() > self.buffer.len() - self.buffered {
            let remaining = self.buffer.len() - self.buffered;
            self.buffer[self.buffered..].copy_from_slice(&bytes[..remaining]);
            self.buffered = self.buffer.len();
            self.flush();
            bytes = &bytes[remaining..];
            let whole_buffers = bytes.len() / self.buffer.len() * self.buffer.len();
            if whole_buffers != 0 {
                self.hasher.update(&bytes[..whole_buffers]);
                bytes = &bytes[whole_buffers..];
            }
        }
        let end = self.buffered + bytes.len();
        self.buffer[self.buffered..end].copy_from_slice(bytes);
        self.buffered = end;
    }

    fn flush(&mut self) {
        self.hasher.update(&self.buffer[..self.buffered]);
        self.buffered = 0;
    }

    fn finish(mut self) -> (ContentDigest, u64) {
        self.flush();
        (
            ContentDigest::from_bytes(*self.hasher.finalize().as_bytes()),
            self.encoded_bytes,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::CanonicalDigest;
    use codevetra_domain::ContentDigest;

    #[test]
    fn typed_edge_table_preserves_its_name_count_and_exact_row_bytes() {
        let source = codevetra_domain::SymbolId::parse("11111111-1111-4111-8111-111111111111")
            .unwrap_or_else(|error| panic!("source: {error}"));
        let target = codevetra_domain::SymbolId::parse("22222222-2222-4222-8222-222222222222")
            .unwrap_or_else(|error| panic!("target: {error}"));
        let rows = [super::EdgeInput {
            source_symbol_id: source,
            target_symbol_id: target,
            kind: codevetra_domain::EdgeKind::Calls,
            confidence: 0.75,
            provenance: "syntax".to_owned(),
            site_count: 7,
        }];
        let mut digest = CanonicalDigest::new(b"typed-table");
        assert_eq!(digest.table(&rows, &mut || false), Ok(()));
        let encoded = concat!(
            "5\x00edges1\x001",
            "36\x0011111111-1111-4111-8111-111111111111",
            "36\x0022222222-2222-4222-8222-222222222222",
            "5\x00calls\x3f\x40\x00\x00",
            "6\x00syntax\x00\x00\x00\x07",
        )
        .as_bytes();
        let mut oracle = blake3::Hasher::new();
        oracle.update(b"typed-table");
        oracle.update(encoded);
        assert_eq!(
            digest.finish(),
            (
                ContentDigest::from_bytes(*oracle.finalize().as_bytes()),
                encoded.len() as u64
            )
        );

        let mut cancelled = CanonicalDigest::new(b"typed-table");
        assert_eq!(cancelled.table(&rows, &mut || true), Err(()));
        // Cancellation is checked after the table header and before the first row.
        assert_eq!(cancelled.finish().1, 10);
    }

    #[test]
    fn empty_generation_digest_still_checks_final_cancellation() {
        let facts = super::ValidatedFactTables {
            files: Vec::new(),
            symbols: Vec::new(),
            edges: Vec::new(),
            references: Vec::new(),
            numerical_sites: Vec::new(),
            documents: Vec::new(),
        };
        for version in codevetra_domain::GenerationDigestVersion::ALL {
            assert!(super::logical_digest(&facts, version, || true).is_err());
            assert!(super::logical_digest(&facts, version, || false).is_ok());
        }
    }

    #[test]
    fn canonical_wire_format_preserves_lengths_markers_and_integer_widths() {
        let domain = b"canonical-wire-contract";
        let mut digest = CanonicalDigest::new(domain);
        digest.section("rows", u64::MAX);
        digest.text("");
        digest.text("é🦀");
        digest.optional_text(None);
        digest.optional_text(Some(""));
        digest.optional_text(Some("λ"));
        digest.u64(u64::MAX);
        digest.u32(u32::MAX);
        digest.boolean(true);
        digest.boolean(false);
        // This is the existing wire contract, specified independently of the
        // optimized encoder. Decimal lengths count UTF-8 bytes, not characters.
        let encoded = b"4\x00rows20\x00184467440737095516150\x006\x00\xc3\xa9\xf0\x9f\xa6\x80\x00\x010\x00\x012\x00\xce\xbb\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\xff\x01\x00";
        let mut oracle = blake3::Hasher::new();
        oracle.update(domain);
        oracle.update(encoded);
        let (actual, bytes) = digest.finish();
        assert_eq!(
            actual,
            ContentDigest::from_bytes(*oracle.finalize().as_bytes())
        );
        assert_eq!(bytes, u64::try_from(encoded.len()).unwrap_or(u64::MAX));
    }

    #[test]
    fn canonical_stream_matches_legacy_encoding_across_buffer_boundaries() {
        let domain = b"canonical-stream-contract";
        let mut digest = CanonicalDigest::new(domain);
        let mut oracle = blake3::Hasher::new();
        oracle.update(domain);
        let mut encoded_bytes = 0_u64;
        let values =
            [0, 1, 63, 1023, 8191, 8192, 8193, 32769, 7, 8190].map(|length| "x".repeat(length));
        for value in values.into_iter().chain(["é\0🦀".to_owned()]) {
            digest.text(&value);
            // Preserve the original unbuffered decimal framing as the oracle.
            let prefix = value.len().to_string();
            oracle.update(prefix.as_bytes());
            oracle.update(&[0]);
            oracle.update(value.as_bytes());
            encoded_bytes += u64::try_from(prefix.len() + 1 + value.len()).unwrap_or(u64::MAX);
        }
        let (actual, bytes) = digest.finish();
        assert_eq!(
            actual,
            ContentDigest::from_bytes(*oracle.finalize().as_bytes())
        );
        assert_eq!(bytes, encoded_bytes);
    }
}
