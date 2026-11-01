//! Typed metadata for framework boundary landmarks in the immutable symbol graph.
use serde::Serialize;

/// A syntax or convention-backed boundary site; connecting edges carry confidence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundarySite {
    /// HTTP, queue, event, observer, or UI navigation channel.
    pub channel: String,
    /// Source/registration/unknown/external disposition. Source alone does not imply a match.
    pub role: String,
    /// Digest of the evidenced application/router/instance identity, never a global string join.
    pub scope: String,
    /// Request method or channel operation.
    pub operation: String,
    /// Explicit adapter contract.
    pub adapter: String,
    /// Bounded literal destination, or an explicit dynamic/unknown marker.
    pub destination: String,
    /// Generation-derived resolution state; ordinary syntax extraction is uncomputed.
    pub resolution: String,
}

impl BoundarySite {
    /// Decode only the canonical framework landmark namespace.
    #[must_use]
    pub fn from_qualified_name(name: &str) -> Option<Self> {
        let (_, encoded) = name.split_once("::boundary-v1::")?;
        let encoded = encoded
            .rsplit_once('#')
            .filter(|(_, ordinal)| {
                !ordinal.is_empty() && ordinal.bytes().all(|byte| byte.is_ascii_digit())
            })
            .map_or(encoded, |(base, _)| base);
        let mut parts = encoded.splitn(6, "::");
        let channel = parts.next()?;
        let role = parts.next()?;
        let scope = parts.next()?;
        let operation = parts.next()?;
        let adapter = parts.next()?;
        let destination = parts.next()?;
        if !matches!(
            channel,
            "http" | "queue" | "event" | "observer" | "navigation"
        ) || !matches!(role, "source" | "registration" | "unknown" | "external")
            || scope.len() != 32
            || !scope.bytes().all(|byte| byte.is_ascii_hexdigit())
            || operation.len() > 32
            || adapter.len() > 64
            || destination.len() > 512
        {
            return None;
        }
        Some(Self {
            channel: channel.to_owned(),
            role: role.to_owned(),
            scope: scope.to_owned(),
            operation: operation.to_owned(),
            adapter: adapter.to_owned(),
            destination: destination.to_owned(),
            resolution: "uncomputed".to_owned(),
        })
    }

    /// Read a bounded generation-owned state from this framework resource's signature.
    #[must_use]
    pub fn with_resolution_signature(mut self, signature: &str) -> Self {
        if let Some(state) = signature.strip_prefix("boundary-state:").filter(|state| {
            state.len() <= 64
                && state
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }) {
            state.clone_into(&mut self.resolution);
        }
        self
    }

    /// Exact relation kind for this boundary, distinct from a syntax-proven call.
    #[must_use]
    pub fn edge_kind(&self) -> Option<crate::EdgeKind> {
        match self.channel.as_str() {
            "http" => Some(crate::EdgeKind::HttpRequest),
            "queue" => Some(crate::EdgeKind::QueueDispatch),
            "event" | "observer" => Some(crate::EdgeKind::EventDispatch),
            "navigation" => Some(crate::EdgeKind::Navigates),
            _ => None,
        }
    }
}

/// Explicit coverage, independent of whether a query happens to return boundary edges.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BoundaryCoverage {
    /// Stable adapter-family contract.
    pub contract: &'static str,
    /// Adapters with bounded supported forms.
    pub adapters: &'static [&'static str],
    /// Unsupported shapes are not evidence of absent runtime relationships.
    pub limitations: &'static str,
    /// Per-file optional landmark bound; overflow uses `OptionalFactsOmitted`.
    pub maximum_sites_per_file: usize,
    /// Exact channel subscriber limit; excess fanout abstains explicitly.
    pub maximum_subscribers: usize,
    /// Parameterized route-pattern bound per application/method scope.
    pub maximum_parameter_patterns: usize,
}

/// Coverage accompanies graph results, including empty or ordinary-call-only results.
pub const BOUNDARY_COVERAGE: BoundaryCoverage = BoundaryCoverage {
    contract: "application-boundaries-v1",
    adapters: &[
        "node-events-v1",
        "bull-instance-v1",
        "set-observer-v1",
        "next-app-v1",
        "react-router-jsx-v1",
    ],
    limitations: "Supported literal, uniquely bound forms only. Next routing configuration, server-relative fetch, arbitrary bases, aliases, reassignment and dynamic destinations are unproved; source/registration nodes expose resolution state. No runtime execution or coverage is established. Additional router and messaging frameworks are unsupported unless listed.",
    maximum_sites_per_file: 512,
    maximum_subscribers: 8,
    maximum_parameter_patterns: 64,
};
