//! Per-file JavaScript-family state shared between the walk and its passes.

use std::collections::BTreeSet;

use codevetra_domain::{ReferenceKind, SymbolId};

use super::{
    javascript_owners::OwnerIndex, javascript_scopes::LexicalScopes,
    type_contracts::ContractGenerics,
};

/// What the JavaScript-family passes of one file remember between nodes.
#[derive(Default)]
pub(super) struct JavaScriptState<'source> {
    /// Active data-binding initializers keep declaration scopes but execute
    /// calls in their enclosing callable. Suspended at nested callable nodes;
    /// bounded by AST depth and charged to the working budget.
    pub(super) initializer_reference_owners: Vec<(SymbolId, SymbolId)>,
    /// Contract generics this file declares (an object shape with a member
    /// typed by its string name parameter); their first string-literal
    /// argument names a contract property.
    pub(super) contract_generics: ContractGenerics,
    /// The scope index reads consult for their nearest enclosing binding.
    pub(super) scopes: LexicalScopes<'source>,
    /// The interval index shared by value-reference and binding-table reads.
    pub(super) owners: Option<OwnerIndex>,
    /// Owners whose declaration already recorded every type named anywhere
    /// inside it (a non-callable variable declarator, a type alias), so a type
    /// consumer in its value is not recorded a second time.
    pub(super) whole_type_owners: BTreeSet<SymbolId>,
}

impl JavaScriptState<'_> {
    pub(super) fn initializer_execution_owner(&self, owner: Option<&SymbolId>) -> Option<SymbolId> {
        let owner = owner?;
        Some(
            self.initializer_reference_owners
                .iter()
                .rev()
                .find_map(|(binding, execution)| (binding == owner).then_some(execution))
                .unwrap_or(owner)
                .clone(),
        )
    }

    pub(super) fn execution_reference_owner(
        &self,
        owner: Option<&SymbolId>,
        kind: ReferenceKind,
    ) -> Option<SymbolId> {
        if matches!(
            kind,
            ReferenceKind::Calls | ReferenceKind::Instantiates | ReferenceKind::Imports
        ) {
            self.initializer_execution_owner(owner)
        } else {
            owner.cloned()
        }
    }
}
