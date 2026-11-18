//! Exact external module files and their source-directory loading context.
use std::collections::HashSet;

use super::{
    ExtractedImportBinding, FileId, ImportBindingKind, NativeFileFacts,
    RESOLUTION_MAP_NODE_ALLOWANCE, ResolutionIndex, ResolutionIndexTarget, ResolutionRequest,
    StageItemFailure, rust_inline_modules::ModuleScope, rust_path_resolution::ModuleSource,
    size_of, usize_to_u64,
};

pub(super) fn binding_file<'index>(
    index: &'index ResolutionIndex,
    file: &NativeFileFacts,
    binding: &ExtractedImportBinding,
) -> Option<&'index FileId> {
    if binding.kind == ImportBindingKind::RustFileModule {
        let path = super::normalize_joined_project_path(
            &file.file.normalized_path,
            &binding.module_specifier,
        )?;
        let id = super::unique_file_for_path(index, &path)?;
        return index
            .modules
            .files
            .get(id)
            .filter(|file| file.language == "rust")
            .map(|_| id);
    }
    let path = super::normalize_relative_module_path(
        &file.file.normalized_path,
        &binding.module_specifier,
    )?;
    super::resolve_normalized_module_file(&index.modules, &path, "rust")
}

/// A path-loaded file uses its physical directory for conventional children.
/// A file also loaded conventionally can have two different child directories;
/// retain that uncertainty instead of selecting a global, order-dependent owner.
pub(super) fn directories<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    roots: &HashSet<FileId>,
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let mut contexts = LoadingContexts::collect(target, cancelled)?;
    for file in &contexts.explicit {
        rebase(target, (file, &mut contexts.conventional), cancelled)?;
    }
    for file in contexts.explicit.intersection(&contexts.conventional) {
        reject_conflicting_directory(target, (file, roots), cancelled)?;
    }
    target.index.languages.rust_paths.explicit_files = contexts.explicit;
    Ok(())
}

#[derive(Default)]
struct LoadingContexts {
    explicit: HashSet<FileId>,
    conventional: HashSet<FileId>,
}

impl LoadingContexts {
    fn collect<Cancel>(
        target: &mut ResolutionIndexTarget<'_>,
        cancelled: &mut Cancel,
    ) -> Result<Self, StageItemFailure>
    where
        Cancel: FnMut() -> bool,
    {
        let mut contexts = Self::default();
        for edges in target.index.languages.rust_paths.modules.values() {
            for edge in edges.values().flatten() {
                if cancelled() {
                    return Err(StageItemFailure);
                }
                if let Some(file) = &edge.file {
                    let files = match edge.source {
                        ModuleSource::ExplicitFile => &mut contexts.explicit,
                        ModuleSource::Conventional => &mut contexts.conventional,
                    };
                    retain(target.budget, files, file)?;
                }
            }
        }
        Ok(contexts)
    }
}

fn reject_conflicting_directory<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    (file, roots): (&FileId, &HashSet<FileId>),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if cancelled() {
        return Err(StageItemFailure);
    }
    let context = target
        .index
        .modules
        .files
        .get(file)
        .ok_or(StageItemFailure)?;
    let name = context.path.rsplit('/').next().unwrap_or(&context.path);
    if roots.contains(file) || matches!(name, "lib.rs" | "main.rs" | "mod.rs") {
        return Ok(());
    }
    if let Some(edges) = target.index.languages.rust_paths.modules.get_mut(file) {
        for edge in edges.values_mut().flatten() {
            if cancelled() {
                return Err(StageItemFailure);
            }
            if edge.source == ModuleSource::Conventional {
                edge.file = None;
            }
        }
    }
    Ok(())
}

fn rebase<Cancel>(
    target: &mut ResolutionIndexTarget<'_>,
    (file, conventional): (&FileId, &mut HashSet<FileId>),
    cancelled: &mut Cancel,
) -> Result<(), StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let context = target
        .index
        .modules
        .files
        .get(file)
        .ok_or(StageItemFailure)?;
    let Some(edges) = target.index.languages.rust_paths.modules.get_mut(file) else {
        return Ok(());
    };
    for (name, edge) in edges {
        if cancelled() {
            return Err(StageItemFailure);
        }
        let Some(edge) = edge else { continue };
        if edge.source == ModuleSource::ExplicitFile {
            continue;
        }
        let suffix = name.replace("::", "/");
        let path = super::joined_path(&context.directory, &suffix)?;
        target
            .budget
            .charge(usize_to_u64(suffix.len() + path.len()))?;
        let child = super::resolve_normalized_module_file(&target.index.modules, &path, "rust");
        target
            .budget
            .charge(child.map_or(0, |id| usize_to_u64(id.as_str().len())))?;
        if let Some(child) = child {
            retain(target.budget, conventional, child)?;
        }
        edge.file = child.cloned();
    }
    Ok(())
}

fn retain(
    budget: &mut super::ResolveBudget,
    files: &mut HashSet<FileId>,
    file: &FileId,
) -> Result<(), StageItemFailure> {
    if !files.contains(file) {
        budget.charge(
            RESOLUTION_MAP_NODE_ALLOWANCE + usize_to_u64(size_of::<FileId>() + file.as_str().len()),
        )?;
        files.try_reserve(1).map_err(|_| StageItemFailure)?;
        files.insert(file.clone());
    }
    Ok(())
}

/// The declaration proves a parent relation even for a standalone proof input.
/// It does not manufacture a Cargo root for `pub(crate)` visibility.
pub(super) fn declared_parent<Cancel>(
    index: &ResolutionIndex,
    (request, scope): (&ResolutionRequest<'_>, ModuleScope<'_>),
    cancelled: &mut Cancel,
) -> Result<bool, StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    if scope.module.is_some() || !scope.inline.is_empty() {
        return Ok(false);
    }
    let Some(edges) = index.languages.rust_paths.modules.get(request.file_id) else {
        return Ok(false);
    };
    for (name, edge) in edges {
        if cancelled() {
            return Err(StageItemFailure);
        }
        if edge.as_ref().is_some_and(|edge| {
            edge.source == ModuleSource::ExplicitFile
                && edge.parent_module.is_none()
                && edge.file.as_ref() == Some(scope.file)
        }) && super::rust_path_guards::module_declared(index, (request.file_id, name))
            == Some(true)
        {
            return Ok(true);
        }
    }
    Ok(false)
}
