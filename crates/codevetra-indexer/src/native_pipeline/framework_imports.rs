//! Explicit Angular lazy-import evidence binds only a real module.
//! Missing or ambiguous modules abstain without blocking existing resolution.
use super::{
    IMPORT_BINDING_CONFIDENCE, ModuleResolutionRequest, ReferenceKind, ResolutionIndex,
    ResolutionRequest, ResolvedTarget, StageItemFailure, SymbolKind, javascript_modules,
    resolve_module_file,
};

struct Import<'name> {
    specifier: &'name str,
    provenance: &'static str,
}

impl<'name> Import<'name> {
    fn parse(request: &ResolutionRequest<'name>) -> Option<Self> {
        // Native export flags cannot prove whether an export list is runtime
        // or type-only. Lazy `.then(m => m.X)` evidence adds no target.
        if request.kind != ReferenceKind::Imports {
            return None;
        }
        request
            .name
            .strip_prefix("framework-angular-lazy::")
            .map(|specifier| Self {
                specifier,
                provenance: "native-angular-lazy-module",
            })
    }
}

pub(super) fn resolve<Cancel>(
    index: &ResolutionIndex,
    request: &ResolutionRequest<'_>,
    cancelled: &mut Cancel,
) -> Result<Option<ResolvedTarget>, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    if request.kind == ReferenceKind::Calls
        && let Some(specifier) = request.name.strip_prefix("framework-react-lazy::")
    {
        let module = ModuleResolutionRequest {
            importing_path: request.file_path,
            specifier,
            importing_language: request.language,
        };
        let Some(file) = resolve_module_file(&index.modules, module) else {
            return Ok(None);
        };
        let Some(candidates) = index.default_exports.get(file) else {
            return Ok(None);
        };
        let [candidate] = candidates.as_slice() else {
            return Ok(None);
        };
        // Require a real executable default declaration, never an export-list/type hypothesis.
        if !candidate.export.default_export
            || !matches!(
                candidate.kind,
                SymbolKind::Function | SymbolKind::Component | SymbolKind::Class
            )
        {
            return Ok(None);
        }
        let mut target = ResolvedTarget {
            symbol_id: candidate.symbol_id.clone(),
            kind: candidate.kind,
            confidence: IMPORT_BINDING_CONFIDENCE,
            provenance: "native-react-lazy-default",
        };
        javascript_modules::lower_fallback_target(&mut target, &index.modules, module);
        return Ok(Some(target));
    }
    let Some(import) = Import::parse(request) else {
        return Ok(None);
    };
    let module = ModuleResolutionRequest {
        importing_path: request.file_path,
        specifier: import.specifier,
        importing_language: request.language,
    };
    let Some(file) = resolve_module_file(&index.modules, module) else {
        return Ok(None);
    };
    let mut target = ResolvedTarget {
        symbol_id: index
            .file_symbols
            .get(file)
            .ok_or(StageItemFailure)?
            .clone(),
        kind: SymbolKind::File,
        confidence: IMPORT_BINDING_CONFIDENCE,
        provenance: import.provenance,
    };
    javascript_modules::lower_fallback_target(&mut target, &index.modules, module);
    Ok(Some(target))
}
