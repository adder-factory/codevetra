//! One catalog defines detector applicability independently from parser admission.

use serde::{Deserialize, Serialize};

use super::{SOURCE_RULES, SourceHealthMetric};

/// Language admission for one detector, separate from indexing support.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorLanguages {
    /// Requires an actual native AST metrics producer for the symbol.
    NativeAst,
    /// JavaScript, JSX, TypeScript and TSX only.
    JavascriptFamily,
    /// TypeScript and TSX only.
    TypescriptFamily,
    /// Consumes indexed graph facts across languages, with resolution limits retained.
    IndexedGraph,
}

impl DetectorLanguages {
    /// Language-only admission; native AST availability is checked separately.
    #[must_use]
    pub fn admits(self, language: &str) -> bool {
        match self {
            Self::NativeAst | Self::IndexedGraph => true,
            Self::JavascriptFamily => {
                matches!(language, "javascript" | "jsx" | "typescript" | "tsx")
            }
            Self::TypescriptFamily => matches!(language, "typescript" | "tsx"),
        }
    }

    /// Compact documentation spelling for the capability matrix.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::NativeAst => "native AST metric producers",
            Self::JavascriptFamily => "javascript, jsx, typescript, tsx",
            Self::TypescriptFamily => "typescript, tsx",
            Self::IndexedGraph => "indexed graph languages",
        }
    }
}

/// Symbol scopes whose evidence the detector knows how to evaluate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorScope {
    /// Independently owned function, method or component implementation.
    Callable,
    /// Class, struct or module containing behavior.
    Container,
    /// Instance or static methods with member-access evidence.
    Method,
    /// Constant declarations with documentation metrics.
    Constant,
    /// Production declarations; test ownership is always excluded.
    ProductionSymbol,
    /// Module relationships under explicit project architecture policy.
    ModuleDependency,
}

impl DetectorScope {
    /// Test and declaration-only exclusions are applied independently.
    #[must_use]
    pub fn admits(self, kind: &str) -> bool {
        match self {
            Self::Callable => matches!(kind, "function" | "method" | "component"),
            Self::Container => matches!(kind, "class" | "struct" | "module"),
            Self::Method => kind == "method",
            Self::Constant => kind == "constant",
            Self::ProductionSymbol => !matches!(kind, "file" | "import" | "parameter"),
            Self::ModuleDependency => kind == "module",
        }
    }

    /// Compact documentation spelling.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Callable => "callable implementations",
            Self::Container => "class, struct, module",
            Self::Method => "methods",
            Self::Constant => "constants",
            Self::ProductionSymbol => "production declarations",
            Self::ModuleDependency => "module dependencies",
        }
    }
}

/// Depth of evidence, never a claim of exploitability or formal proof.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorEvidence {
    /// Structural AST counters and formulas.
    StructuralMetric,
    /// Lexical cues within a bounded owned source scope.
    LexicalHeuristic,
    /// Operation-local AST evidence with documented resolution limits.
    AstLocal,
    /// Published graph or auxiliary facts.
    GraphSupported,
}

/// Input that must exist before absence of findings has meaning.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorInput {
    /// Native source metrics were actually collected.
    SourceMetrics,
    /// Current graph facts, including explicit unresolved references.
    Graph,
    /// Comparable recent retained generation.
    PreviousGeneration,
    /// Generation-fenced LCOV plus complexity rank.
    Coverage,
    /// Explicit configured project layers.
    LayerPolicy,
    /// Structural/shape hashes and bounded token profiles; embeddings are optional.
    CloneEvidence,
}

/// Registry entry shared by documentation, CLI/MCP and coverage counting.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectorCapability {
    /// Stable public detector identity.
    pub name: &'static str,
    /// Language evidence required beyond parser admission.
    pub languages: DetectorLanguages,
    /// Admitted declaration scopes.
    pub scope: DetectorScope,
    /// Analysis depth.
    pub evidence: DetectorEvidence,
    /// Required evidence availability.
    pub input: DetectorInput,
    /// Runtime assumptions and bounded unsupported shapes.
    pub assumptions: &'static str,
}

/// Return the complete bounded catalog in deterministic name order.
#[must_use]
pub fn detector_capabilities() -> Vec<DetectorCapability> {
    let mut rows = SOURCE_RULES
        .iter()
        .map(|rule| source_capability(rule.name, rule.metric))
        .collect::<Vec<_>>();
    rows.extend(PROJECT_RULES);
    rows.sort_unstable_by_key(|rule| rule.name);
    rows
}

/// Find a detector without treating an unknown name as a supported clean check.
#[must_use]
pub fn detector_capability(name: &str) -> Option<DetectorCapability> {
    SOURCE_RULES
        .iter()
        .find(|rule| rule.name == name)
        .map(|rule| source_capability(rule.name, rule.metric))
        .or_else(|| PROJECT_RULES.iter().find(|rule| rule.name == name).copied())
}

pub(super) fn source_capability(
    name: &'static str,
    metric: SourceHealthMetric,
) -> DetectorCapability {
    use SourceHealthMetric as M;
    let languages = match metric {
        M::TsAnyCasts | M::TsSuppressions => DetectorLanguages::TypescriptFamily,
        M::SequentialAwaitLoops
        | M::DynamicEval
        | M::InsecureHash
        | M::InsecureRandom
        | M::HttpWithoutTimeout
        | M::SqlStringConcatenation
        | M::UnsafeJsonParse
        | M::UnvalidatedEnv
        | M::DebugLogs
        | M::SyncIoInAsync => DetectorLanguages::JavascriptFamily,
        _ => DetectorLanguages::NativeAst,
    };
    let evidence = match metric {
        M::TsAnyCasts | M::TsSuppressions | M::IncompleteMarkers | M::SecretsScore => {
            DetectorEvidence::LexicalHeuristic
        }
        M::CodeLines
        | M::Cyclomatic
        | M::MaxNesting
        | M::MaxConditionalOperands
        | M::ParameterCount
        | M::BrainRisk
        | M::EmptyBody => DetectorEvidence::StructuralMetric,
        _ => DetectorEvidence::AstLocal,
    };
    DetectorCapability {
        name,
        languages,
        scope: DetectorScope::Callable,
        evidence,
        input: DetectorInput::SourceMetrics,
        assumptions: match languages {
            DetectorLanguages::JavascriptFamily => {
                "bounded conventional unshadowed names; direct imports only; dynamic aliases and client configurations unresolved; nested callables evaluated independently"
            }
            DetectorLanguages::TypescriptFamily => {
                "TypeScript owned-source lexical cues; no compiler type proof"
            }
            _ => {
                "requires produced native metrics; parser admission alone is insufficient; no top-level statement or initializer scan"
            }
        },
    }
}

const fn project(
    name: &'static str,
    scope: DetectorScope,
    input: DetectorInput,
    assumptions: &'static str,
) -> DetectorCapability {
    DetectorCapability {
        name,
        scope,
        input,
        languages: match input {
            DetectorInput::SourceMetrics => DetectorLanguages::NativeAst,
            _ => DetectorLanguages::IndexedGraph,
        },
        evidence: match input {
            DetectorInput::SourceMetrics => DetectorEvidence::AstLocal,
            _ => DetectorEvidence::GraphSupported,
        },
        assumptions,
    }
}

const PROJECT_RULES: &[DetectorCapability] = &[
    project(
        "god_class",
        DetectorScope::Container,
        DetectorInput::Graph,
        "behavioral and complex member population required",
    ),
    project(
        "feature_envy",
        DetectorScope::Method,
        DetectorInput::Graph,
        "resolved own and foreign member evidence; uncertainty retained",
    ),
    project(
        "recently_grew",
        DetectorScope::Callable,
        DetectorInput::PreviousGeneration,
        "comparable retained generation within thirty days; physical-line growth",
    ),
    project(
        "stale_doc",
        DetectorScope::Constant,
        DetectorInput::SourceMetrics,
        "native documented-number evidence; no natural-language correctness proof",
    ),
    project(
        "unused_export",
        DetectorScope::ProductionSymbol,
        DetectorInput::Graph,
        "framework exports and dynamic external references abstain; never authorizes deletion",
    ),
    project(
        "duplicate_code",
        DetectorScope::Callable,
        DetectorInput::CloneEvidence,
        "exact and structural hashes; bounded partial-clone profiles; embeddings optional",
    ),
    project(
        "low_coverage",
        DetectorScope::Callable,
        DetectorInput::Coverage,
        "generation-fenced LCOV and complexity percentile",
    ),
    project(
        "illegal_import",
        DetectorScope::ModuleDependency,
        DetectorInput::LayerPolicy,
        "evaluated by project layer analysis rather than the native symbol cascade",
    ),
    project(
        "high_fan_out",
        DetectorScope::ProductionSymbol,
        DetectorInput::Graph,
        "grouped graph dependencies and orchestration policy",
    ),
    project(
        "unresolved_reference_pressure",
        DetectorScope::ProductionSymbol,
        DetectorInput::Graph,
        "CodeVetra resolution coverage; not a compiler diagnostic",
    ),
];

/// Coverage state for a detector and language in one completed computation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DetectorCoverageState {
    /// Every eligible symbol had the required evidence.
    Evaluated,
    /// Some eligible symbols could not be evaluated.
    PartiallyEvaluated,
    /// No admitted language/scope/metrics producer was present.
    #[default]
    Unsupported,
    /// The population consists only of intentionally excluded source.
    Excluded,
    /// Required optional input was missing for every eligible symbol.
    Unavailable,
}

/// Exact source-population counts; zero findings alone never establishes coverage.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectorCoverage {
    /// Native source language.
    pub language: String,
    /// Detector identity from the authoritative registry.
    pub detector: String,
    /// Readiness within this declared population.
    pub state: DetectorCoverageState,
    /// All symbol documents considered for this language.
    pub considered_symbols: u64,
    /// Admitted symbols, including those missing optional evidence.
    pub eligible_symbols: u64,
    /// Admitted symbols evaluated with available evidence.
    pub evaluated_symbols: u64,
    /// Unsupported language, source scope, or metrics producer.
    pub unsupported_symbols: u64,
    /// Tests, declarations or explicit rule-policy exclusions.
    pub excluded_symbols: u64,
    /// Admitted symbols with missing optional evidence.
    pub unavailable_symbols: u64,
    /// Complete findings before output filters or limits.
    pub finding_count: u64,
}

impl DetectorCoverage {
    /// Derive readiness after counting the complete bounded native inventory.
    pub fn finish(&mut self) {
        self.state = if self.evaluated_symbols > 0 {
            if self.unavailable_symbols > 0 {
                DetectorCoverageState::PartiallyEvaluated
            } else {
                DetectorCoverageState::Evaluated
            }
        } else if self.unavailable_symbols > 0 {
            DetectorCoverageState::Unavailable
        } else if self.unsupported_symbols > 0 {
            DetectorCoverageState::Unsupported
        } else {
            DetectorCoverageState::Excluded
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_detector_has_one_documented_capability() {
        let rows = detector_capabilities();
        assert_eq!(rows.len(), 35);
        assert!(rows.windows(2).all(|pair| pair[0].name < pair[1].name));
        let documentation = include_str!("../../../../docs/BIOMARKER-DETECTORS.md");
        for row in rows {
            let evidence = serde_json::to_string(&row.evidence)
                .unwrap_or_else(|error| panic!("evidence: {error}"));
            let input =
                serde_json::to_string(&row.input).unwrap_or_else(|error| panic!("input: {error}"));
            let expected = format!(
                "| `{}` | {} | {} | `{}` | `{}` |",
                row.name,
                row.languages.label(),
                row.scope.label(),
                evidence.trim_matches('"'),
                input.trim_matches('"')
            );
            assert_eq!(
                documentation.matches(&expected).count(),
                1,
                "capability documentation drift: {expected}"
            );
        }
    }
}
