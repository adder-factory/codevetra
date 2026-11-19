//! Coverage is counted from the evaluated population, not inferred from findings.

use std::collections::BTreeMap;

use codevetra_domain::biomarkers::{
    DetectorCapability, DetectorCoverage, DetectorInput, SOURCE_RULES, detector_capabilities,
};

use super::inventory::{Base, Document};
use crate::{FileInput, SymbolInput};

pub(super) struct CoverageAccumulator {
    registry: Vec<DetectorCapability>,
    languages: BTreeMap<String, Vec<DetectorCoverage>>,
}

enum Disposition {
    Evaluated,
    Unsupported,
    Excluded,
    Unavailable,
}

impl CoverageAccumulator {
    pub(super) fn new() -> Self {
        Self {
            registry: detector_capabilities(),
            languages: BTreeMap::new(),
        }
    }

    pub(super) fn observe(&mut self, subject: Subject<'_>) {
        let file = subject.file;
        let rows = self
            .languages
            .entry(file.language.clone())
            .or_insert_with(|| {
                self.registry
                    .iter()
                    .map(|rule| DetectorCoverage {
                        language: file.language.clone(),
                        detector: rule.name.to_owned(),
                        ..DetectorCoverage::default()
                    })
                    .collect()
            });
        for (rule, row) in self.registry.iter().zip(rows) {
            row.considered_symbols += 1;
            match subject.disposition(rule) {
                Disposition::Evaluated => {
                    row.eligible_symbols += 1;
                    row.evaluated_symbols += 1;
                }
                Disposition::Unavailable => {
                    row.eligible_symbols += 1;
                    row.unavailable_symbols += 1;
                }
                Disposition::Unsupported => row.unsupported_symbols += 1,
                Disposition::Excluded => row.excluded_symbols += 1,
            }
        }
    }

    pub(super) fn finding(&mut self, language: &str, detector: &str) {
        if let Some(row) = self
            .languages
            .get_mut(language)
            .and_then(|rows| rows.iter_mut().find(|row| row.detector == detector))
        {
            row.finding_count += 1;
        }
    }

    pub(super) fn finish(self) -> Vec<DetectorCoverage> {
        self.languages
            .into_values()
            .flatten()
            .map(|mut row| {
                row.finish();
                row
            })
            .collect()
    }
}

/// One symbol and its matching evidence; scored observations derive every
/// identity from the same Base rather than accepting independently paired inputs.
#[derive(Clone, Copy)]
pub(super) struct Subject<'a> {
    file: &'a FileInput,
    symbol: &'a SymbolInput,
    document: &'a Document,
    base: Option<&'a Base<'a>>,
}

impl<'a> Subject<'a> {
    pub(super) fn unscored(
        file: &'a FileInput,
        symbol: &'a SymbolInput,
        document: &'a Document,
    ) -> Self {
        Self {
            file,
            symbol,
            document,
            base: None,
        }
    }

    pub(super) fn scored(base: &'a Base<'a>) -> Self {
        Self {
            file: base.file,
            symbol: base.symbol,
            document: base.document,
            base: Some(base),
        }
    }

    fn language(&self, input: DetectorInput) -> &str {
        if input == DetectorInput::SourceMetrics {
            self.document
                .analyzed_language
                .map_or(self.file.language.as_str(), |language| language.as_str())
        } else {
            self.file.language.as_str()
        }
    }

    fn source_rule_excluded(&self, rule: &DetectorCapability) -> bool {
        SOURCE_RULES
            .iter()
            .find(|source| source.name == rule.name)
            .is_some_and(|source| {
                self.document.health("code_lines") < f64::from(source.minimum_code_lines)
                    || source.excludes_value_mapping(
                        self.document.analyzed_language,
                        self.document.syntax.flat_value_mapping,
                    )
                    || (rule.name == "long_parameter_list"
                        && self.base.is_some_and(|base| base.topology.framework))
            })
    }

    fn disposition(&self, rule: &DetectorCapability) -> Disposition {
        if self.document.test || self.symbol.implementation.declaration_only {
            return Disposition::Excluded;
        }
        if !rule.languages.admits(self.language(rule.input))
            || !rule.scope.admits(&self.symbol.symbol_kind)
        {
            return Disposition::Unsupported;
        }
        if rule.input == DetectorInput::SourceMetrics && self.document.analyzed_language.is_none() {
            return Disposition::Unsupported;
        }
        if self.source_rule_excluded(rule) {
            return Disposition::Excluded;
        }
        match rule.input {
            DetectorInput::PreviousGeneration
                if self
                    .base
                    .is_none_or(|base| base.prior.is_none() || !base.recent_prior) =>
            {
                Disposition::Unavailable
            }
            DetectorInput::Coverage if self.base.is_none_or(|base| base.coverage.is_none()) => {
                Disposition::Unavailable
            }
            // Layer policy is evaluated by the project service, independently of native findings.
            DetectorInput::LayerPolicy => Disposition::Unavailable,
            _ => Disposition::Evaluated,
        }
    }
}
