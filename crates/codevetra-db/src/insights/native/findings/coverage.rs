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

    pub(super) fn observe(
        &mut self,
        file: &FileInput,
        symbol: &SymbolInput,
        document: &Document,
        base: Option<&Base<'_>>,
    ) {
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
            match disposition(rule, file, symbol, document, base) {
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

fn disposition(
    rule: &DetectorCapability,
    file: &FileInput,
    symbol: &SymbolInput,
    document: &Document,
    base: Option<&Base<'_>>,
) -> Disposition {
    if document.test || symbol.declaration_only {
        return Disposition::Excluded;
    }
    let language = if rule.input == DetectorInput::SourceMetrics {
        document
            .analyzed_language
            .map_or(file.language.as_str(), |language| language.as_str())
    } else {
        file.language.as_str()
    };
    if !rule.languages.admits(language) || !rule.scope.admits(&symbol.symbol_kind) {
        return Disposition::Unsupported;
    }
    if rule.input == DetectorInput::SourceMetrics && document.analyzed_language.is_none() {
        return Disposition::Unsupported;
    }
    if let Some(source) = SOURCE_RULES.iter().find(|source| source.name == rule.name)
        && (document.health("code_lines") < f64::from(source.minimum_code_lines)
            || (rule.name == "long_parameter_list"
                && base.is_some_and(|base| base.topology.framework)))
    {
        return Disposition::Excluded;
    }
    match rule.input {
        DetectorInput::PreviousGeneration
            if base.is_none_or(|base| base.prior.is_none() || !base.recent_prior) =>
        {
            Disposition::Unavailable
        }
        DetectorInput::Coverage if base.is_none_or(|base| base.coverage.is_none()) => {
            Disposition::Unavailable
        }
        // Layer policy is evaluated by the project service, independently of native findings.
        DetectorInput::LayerPolicy => Disposition::Unavailable,
        _ => Disposition::Evaluated,
    }
}
