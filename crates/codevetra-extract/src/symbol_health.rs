//! Sparse ownership of immutable symbol metrics, preserving their wire shape.
use std::{ops::Deref, sync::LazyLock};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::SymbolHealthMetrics;

static EMPTY: LazyLock<SymbolHealthMetrics> = LazyLock::new(SymbolHealthMetrics::default);

/// Immutable metrics with no per-symbol allocation for an entirely empty record.
/// Full-width counters are retained whenever the producer emits any evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SymbolHealth(Option<Box<SymbolHealthMetrics>>);

impl SymbolHealth {
    /// Owned heap bytes for the metric record and its sparse operation sites.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.0.as_ref().map_or(0, |metrics| {
            size_of::<SymbolHealthMetrics>()
                .saturating_add(metrics.operation_sites.retained_bytes())
        })
    }
}

impl From<SymbolHealthMetrics> for SymbolHealth {
    fn from(metrics: SymbolHealthMetrics) -> Self {
        if metrics == *EMPTY {
            Self::default()
        } else {
            Self(Some(Box::new(metrics)))
        }
    }
}

impl Deref for SymbolHealth {
    type Target = SymbolHealthMetrics;

    fn deref(&self) -> &Self::Target {
        self.0.as_deref().unwrap_or(&EMPTY)
    }
}

impl Serialize for SymbolHealth {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.deref().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for SymbolHealth {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SymbolHealthMetrics::deserialize(deserializer).map(Self::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_metrics_share_storage_but_keep_the_full_serialized_record()
    -> Result<(), serde_json::Error> {
        let dense = SymbolHealthMetrics::default();
        let sparse = SymbolHealth::from(dense.clone());
        assert_eq!(sparse.retained_bytes(), 0);
        assert_eq!(size_of::<SymbolHealth>(), size_of::<usize>());
        assert_eq!(
            serde_json::to_value(&sparse)?,
            serde_json::to_value(&dense)?
        );
        let decoded: SymbolHealth = serde_json::from_value(serde_json::to_value(dense)?)?;
        assert_eq!(decoded, sparse);
        assert_eq!(decoded.retained_bytes(), 0);
        Ok(())
    }

    #[test]
    fn analyzed_empty_bodies_and_large_counts_keep_their_evidence_and_allocation_charge()
    -> Result<(), serde_json::Error> {
        for metrics in [
            SymbolHealthMetrics {
                analyzed_language: Some(codevetra_domain::SourceLanguage::Rust),
                ..Default::default()
            },
            SymbolHealthMetrics {
                cyclomatic: 65_537,
                magic_numbers: u32::MAX,
                ..Default::default()
            },
        ] {
            let dense = serde_json::to_value(&metrics)?;
            let sparse = SymbolHealth::from(metrics.clone());
            assert_eq!(sparse.retained_bytes(), size_of::<SymbolHealthMetrics>());
            assert_eq!(*sparse, metrics);
            assert_eq!(serde_json::to_value(&sparse)?, dense);
            let decoded: SymbolHealth = serde_json::from_value(dense)?;
            assert_eq!(decoded, sparse);
        }
        Ok(())
    }
}
