//! Sparse operation evidence does not reserve a vector header for every declaration.
use crate::HealthOperationSite;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Sites {
    values: Vec<HealthOperationSite>,
}

/// Source-local sites, allocating only when an operation actually needs evidence.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HealthOperationSites(Option<Box<Sites>>);

impl HealthOperationSites {
    /// The public serialized representation remains an ordinary site array.
    #[must_use]
    pub fn as_slice(&self) -> &[HealthOperationSite] {
        self.0
            .as_ref()
            .map_or(&[][..], |sites| sites.values.as_slice())
    }

    /// Whether this symbol has no retained operation evidence.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.as_ref().is_none_or(|sites| sites.values.is_empty())
    }

    pub(crate) fn push(&mut self, site: HealthOperationSite) {
        self.0
            .get_or_insert_with(Default::default)
            .values
            .push(site);
    }

    /// Retained heap bytes, including spare site capacity and its boxed header.
    #[must_use]
    pub fn retained_bytes(&self) -> usize {
        self.0.as_ref().map_or(0, |sites| {
            std::mem::size_of::<Sites>().saturating_add(
                sites
                    .values
                    .capacity()
                    .saturating_mul(std::mem::size_of::<HealthOperationSite>()),
            )
        })
    }
}

impl Serialize for HealthOperationSites {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.as_slice().serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for HealthOperationSites {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let values = Vec::<HealthOperationSite>::deserialize(deserializer)?;
        Ok(Self(
            (!values.is_empty()).then(|| Box::new(Sites { values })),
        ))
    }
}
