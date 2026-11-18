use super::capability_symbol;
use super::unqual_resolution::{call, generation};

const CARGO: &str = "[package]\nname = \"file-paths\"\nversion = \"0.1.0\"\n";

#[test]
fn rust_path_attribute_connects_the_actual_independent_path_proof() {
    let facts = generation(&[
        (
            "verification/kani/structures.rs",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../verification/kani/structures.rs"
            )),
        ),
        (
            "crates/codevetra-domain/src/source/path.rs",
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../codevetra-domain/src/source/path.rs"
            )),
        ),
    ]);
    let owner = capability_symbol(
        &facts,
        "verification/kani/structures.rs",
        "windows_drive_prefix",
    );
    let reference = super::CapabilityReferenceQuery::new(&facts, owner)
        .named("paths::drive_prefix", super::ReferenceKind::Calls);
    let target = capability_symbol(
        &facts,
        "crates/codevetra-domain/src/source/path.rs",
        "drive_prefix",
    );
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{reference:?}"
    );
}

#[test]
fn rust_path_attribute_connects_a_cargo_benchmark_with_conventional_target_path() {
    let facts = generation(&[
        (
            "Cargo.toml",
            "[package]\nname = \"file-paths\"\nversion = \"0.1.0\"\n[[bench]]\nname = \"scaling\"\nharness = false\n",
        ),
        (
            "benches/scaling.rs",
            "#[path = \"scaling/fixture.rs\"] mod fixture; pub fn use_it() { fixture::helper(); }",
        ),
        ("benches/scaling/fixture.rs", "pub(crate) fn helper() {}"),
    ]);
    let reference = call(&facts, "benches/scaling.rs", "fixture::helper");
    let target = capability_symbol(&facts, "benches/scaling/fixture.rs", "helper");
    assert_eq!(
        reference.target_symbol_id.as_ref(),
        Some(&target.symbol_id),
        "{reference:?}"
    );
}

#[test]
fn standard_macro_arguments_resolve_qualified_paths_only_with_proven_scope() {
    for (prefix, body, resolved) in [
        ("", "assert_eq!(api::helper(), true);", true),
        ("", "std::assert_eq!(api::helper(), true);", true),
        (
            "",
            "assert_eq!((|api: bool| api)(true), api::helper());",
            true,
        ),
        (
            "",
            "assert_eq!({ type api = Unknown; api::helper() }, true);",
            false,
        ),
        ("", "assert_eq!(custom!(api::helper()), true);", false),
        ("", "custom!(assert_eq!(api::helper(), true));", false),
        ("", "compile_error!(api::helper());", false),
        ("", "std::compile_error!(api::helper());", false),
        (
            "",
            "assert_eq!(type api = Unknown; api::helper(), true);",
            false,
        ),
        (
            "macro_rules! assert_eq { ($($tokens:tt)*) => {} }",
            "assert_eq!(api::helper(), true);",
            false,
        ),
        (
            "use unknown::assert_eq;",
            "assert_eq!(api::helper(), true);",
            false,
        ),
        ("use unknown::*;", "assert_eq!(api::helper(), true);", false),
        (
            "#[macro_use] mod macros;",
            "assert_eq!(api::helper(), true);",
            false,
        ),
        ("mod std {}", "std::assert_eq!(api::helper(), true);", false),
    ] {
        let source =
            format!("#[path = \"helper.rs\"] mod api; {prefix} pub fn use_it() {{ {body} }}");
        let facts = generation(&[
            ("lib.rs", &source),
            ("helper.rs", "pub fn helper() -> bool { true }"),
        ]);
        let reference = call(&facts, "lib.rs", "api::helper");
        let target = capability_symbol(&facts, "helper.rs", "helper");
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            resolved.then_some(&target.symbol_id),
            "{prefix} {body}: {reference:?}"
        );
    }
}

#[test]
fn root_diagnostic_macros_never_turn_their_tokens_into_qualified_calls() {
    let facts = generation(&[
        (
            "lib.rs",
            "#[path = \"helper.rs\"] mod api; compile_error!(api::helper()); pub fn use_it() { api::helper(); }",
        ),
        ("helper.rs", "pub fn helper() {}"),
    ]);
    let target = capability_symbol(&facts, "helper.rs", "helper");
    let direct = call(&facts, "lib.rs", "api::helper");
    assert_eq!(direct.target_symbol_id.as_ref(), Some(&target.symbol_id));
    let calls = facts
        .references()
        .iter()
        .filter(|reference| {
            reference.reference_name == "api::helper" && reference.reference_kind == "calls"
        })
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls
            .iter()
            .filter(|reference| reference.target_symbol_id.is_some())
            .count(),
        1
    );
}

#[test]
fn external_macro_prelude_imports_fence_standard_spellings_before_and_after_the_item() {
    let binding = "#[path = \"helper.rs\"] mod api;";
    let caller = "pub fn use_it() { assert_eq!(api::helper(), true); }";
    let import = "#[macro_use] extern crate unknown;";
    for source in [
        format!("{binding} {import} {caller}"),
        format!("{binding} {caller} {import}"),
    ] {
        let facts = generation(&[
            ("lib.rs", &source),
            ("helper.rs", "pub fn helper() -> bool { true }"),
        ]);
        assert!(
            call(&facts, "lib.rs", "api::helper")
                .target_symbol_id
                .is_none(),
            "{source}"
        );
    }
}

#[test]
fn cargo_named_targets_keep_exact_paths_and_ambiguous_conventions_separate() {
    for (kind, directory) in [
        ("bench", "benches"),
        ("test", "tests"),
        ("example", "examples"),
    ] {
        for (settings, source, competing, resolved) in [
            (
                "name = \"runner\"",
                format!("{directory}/runner.rs"),
                "unrelated.rs".to_owned(),
                true,
            ),
            (
                "name = \"runner\"",
                format!("{directory}/runner/main.rs"),
                "unrelated.rs".to_owned(),
                true,
            ),
            (
                "name = \"runner\"",
                format!("{directory}/runner.rs"),
                format!("{directory}/runner/main.rs"),
                false,
            ),
            (
                "name = \"runner\"\npath = \"custom/check.rs\"",
                "custom/check.rs".to_owned(),
                format!("{directory}/runner.rs"),
                true,
            ),
            (
                "path = \"custom/check.rs\"",
                "custom/check.rs".to_owned(),
                "unrelated.rs".to_owned(),
                false,
            ),
        ] {
            let manifest = format!("{CARGO}\n[[{kind}]]\n{settings}\n");
            let facts = generation(&[
                ("Cargo.toml", &manifest),
                (
                    &source,
                    "#[path = \"helper.rs\"] mod api; pub fn use_it() { api::helper(); }",
                ),
                (&competing, "fn decoy() {}"),
                (
                    &format!(
                        "{}/helper.rs",
                        source
                            .rsplit_once('/')
                            .map_or("", |(directory, _)| directory)
                    ),
                    "pub(crate) fn helper() {}",
                ),
            ]);
            assert_eq!(
                call(&facts, &source, "api::helper")
                    .target_symbol_id
                    .is_some(),
                resolved,
                "{kind}: {settings}: {source}"
            );
        }
    }
}

#[test]
fn rust_path_attribute_resolves_exact_file_and_retains_alias_identity() {
    for alias in ["helpers", "renamed"] {
        let source = format!(
            "#[path = \"../shared/odd.rs\"]\n// keep attribute adjacency\nmod {alias}; pub fn use_it() {{ {alias}::helper(); }}"
        );
        let facts = generation(&[
            ("Cargo.toml", CARGO),
            ("src/lib.rs", &source),
            ("shared/odd.rs", "pub fn helper() {}"),
            ("src/helpers.rs", "pub fn helper() {}"),
            ("shared/odd/mod.rs", "pub fn helper() {}"),
        ]);
        let reference = call(&facts, "src/lib.rs", &format!("{alias}::helper"));
        let target = capability_symbol(&facts, "shared/odd.rs", "helper");
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            Some(&target.symbol_id),
            "{alias}: {reference:?}"
        );
        assert_eq!(reference.confidence, 1.0);
        assert_eq!(reference.resolution_provenance, "native-import-binding");
    }
}

#[test]
fn rust_path_attribute_keeps_unsupported_raw_module_heads_unresolved() {
    let facts = generation(&[
        (
            "lib.rs",
            "#[path = \"helper.rs\"] mod r#type; pub fn use_it() { r#type::helper(); }",
        ),
        ("helper.rs", "pub fn helper() {}"),
    ]);
    assert!(
        call(&facts, "lib.rs", "r#type::helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_path_attribute_resolves_associated_members_only_through_visible_types() {
    for visibility in ["pub", ""] {
        let helper = format!("{visibility} struct Maker; impl Maker {{ pub fn new() {{}} }}");
        let facts = generation(&[
            (
                "lib.rs",
                "#[path = \"shared/api.rs\"] mod api; pub fn use_it() { api::Maker::new(); api::child::Maker::new(); }",
            ),
            ("shared/api.rs", &format!("pub mod child; {helper}")),
            ("shared/child.rs", &helper),
        ]);
        for (name, file) in [
            ("api::Maker::new", "shared/api.rs"),
            ("api::child::Maker::new", "shared/child.rs"),
        ] {
            let reference = call(&facts, "lib.rs", name);
            let target = capability_symbol(&facts, file, "Maker::new");
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                (!visibility.is_empty()).then_some(&target.symbol_id),
                "{name}: {visibility}"
            );
        }
    }
}

#[test]
fn rust_path_attribute_proves_parent_visibility_without_inventing_a_crate() {
    for visibility in ["pub(super)", "pub(crate)", ""] {
        let target = format!("{visibility} fn helper() {{}}");
        let facts = generation(&[
            (
                "proof/contracts.rs",
                "#[path = \"../source/math.rs\"] mod math; pub fn use_it() { math::helper(); }",
            ),
            ("source/math.rs", &target),
        ]);
        let reference = call(&facts, "proof/contracts.rs", "math::helper");
        let target = capability_symbol(&facts, "source/math.rs", "helper");
        assert_eq!(
            reference.target_symbol_id.as_ref(),
            (visibility == "pub(super)").then_some(&target.symbol_id),
            "{visibility}: {reference:?}"
        );
    }
}

#[test]
fn rust_path_attribute_loads_conventional_children_from_the_explicit_directory() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        (
            "src/lib.rs",
            "#[path = \"../shared/odd.rs\"] pub mod api; pub fn use_it() { api::child::helper(); }",
        ),
        ("shared/odd.rs", "pub mod child;"),
        ("shared/child.rs", "pub fn helper() {}"),
        ("shared/odd/child.rs", "pub fn helper() {}"),
    ]);
    let reference = call(&facts, "src/lib.rs", "api::child::helper");
    let target = capability_symbol(&facts, "shared/child.rs", "helper");
    assert_eq!(reference.target_symbol_id.as_ref(), Some(&target.symbol_id));
}

#[test]
fn rust_path_attribute_never_falls_back_after_a_missing_or_unsupported_path() {
    for attribute in [
        "#[path = \"missing.rs\"]",
        "#[path = \"../escape.rs\"]",
        "#[path = \"/absolute.rs\"]",
        "#[path = r\"helper.rs\"]",
        "#[path = \"helper.rs\"] #[unknown]",
        "#[unknown] #[path = \"helper.rs\"]",
        "#[path = \"helper.rs\"] #[path = \"other.rs\"]",
        "#[cfg_attr(unix, path = \"helper.rs\")]",
    ] {
        let source = format!("{attribute} mod api; pub fn use_it() {{ api::helper(); }}");
        let facts = generation(&[
            ("lib.rs", &source),
            ("api.rs", "pub fn helper() {}"),
            ("helper.rs", "pub fn helper() {}"),
            ("missing.rs/mod.rs", "pub fn helper() {}"),
        ]);
        let reference = call(&facts, "lib.rs", "api::helper");
        assert!(
            reference.target_symbol_id.is_none(),
            "{attribute}: {reference:?}"
        );
    }
}

#[test]
fn rust_path_attribute_shared_file_keeps_each_direct_crate_context() {
    let facts = generation(&[
        (
            "Cargo.toml",
            "[package]\nname = \"file-paths\"\nversion = \"0.1.0\"\n[[bin]]\nname = \"proof\"\npath = \"proof/check.rs\"\n",
        ),
        (
            "src/lib.rs",
            "#[path = \"../shared/math.rs\"] mod math; pub fn use_it() { math::helper(); }",
        ),
        (
            "proof/check.rs",
            "#[path = \"../shared/math.rs\"] mod math; pub fn use_it() { math::helper(); }",
        ),
        ("shared/math.rs", "pub(crate) fn helper() {}"),
        (
            "orphan/check.rs",
            "#[path = \"../shared/math.rs\"] mod math; pub fn use_it() { math::helper(); }",
        ),
    ]);
    let target = capability_symbol(&facts, "shared/math.rs", "helper");
    for source in ["src/lib.rs", "proof/check.rs"] {
        assert_eq!(
            call(&facts, source, "math::helper")
                .target_symbol_id
                .as_ref(),
            Some(&target.symbol_id)
        );
    }
    assert!(
        call(&facts, "orphan/check.rs", "math::helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_path_attribute_conflicting_child_directories_abstain_in_every_context() {
    let facts = generation(&[
        ("Cargo.toml", CARGO),
        (
            "src/lib.rs",
            "pub mod odd; #[path = \"odd.rs\"] pub mod other; pub fn use_it() { other::child::helper(); }",
        ),
        (
            "src/odd.rs",
            "pub mod child; pub fn use_it() { child::helper(); }",
        ),
        ("src/child.rs", "pub fn helper() {}"),
        ("src/odd/child.rs", "pub fn helper() {}"),
    ]);
    assert!(
        call(&facts, "src/lib.rs", "other::child::helper")
            .target_symbol_id
            .is_none()
    );
    assert!(
        call(&facts, "src/odd.rs", "child::helper")
            .target_symbol_id
            .is_none()
    );
}

#[test]
fn rust_path_attribute_visibility_follows_the_declared_alias_not_the_target_file() {
    for visibility in ["pub", "pub(crate)", "pub(super)", ""] {
        let source = format!(
            "#[path = \"../shared/helper.rs\"] {visibility} mod facade; pub fn use_it() {{ facade::helper(); }}"
        );
        let facts = generation(&[
            ("Cargo.toml", CARGO),
            (
                "src/lib.rs",
                "pub mod outer; pub fn use_it() { outer::facade::helper(); }",
            ),
            ("src/outer.rs", &source),
            ("shared/helper.rs", "pub fn helper() {}"),
        ]);
        let target = capability_symbol(&facts, "shared/helper.rs", "helper");
        assert_eq!(
            call(&facts, "src/outer.rs", "facade::helper")
                .target_symbol_id
                .as_ref(),
            Some(&target.symbol_id)
        );
        assert_eq!(
            call(&facts, "src/lib.rs", "outer::facade::helper")
                .target_symbol_id
                .as_ref(),
            (!visibility.is_empty()).then_some(&target.symbol_id),
            "{visibility}"
        );
    }
}

#[test]
fn rust_path_attribute_visibility_ignores_nested_namesake_declarations() {
    for visibility in ["pub", ""] {
        let declaration = format!("#[path = \"../shared/helper.rs\"] {visibility} mod api;");
        for source in [
            format!("{declaration} mod noise {{ mod api; }}"),
            format!("mod noise {{ pub mod api; }} {declaration}"),
        ] {
            let facts = generation(&[
                ("Cargo.toml", CARGO),
                (
                    "src/lib.rs",
                    "pub mod outer; pub fn use_it() { outer::api::helper(); }",
                ),
                ("src/outer.rs", &source),
                ("shared/helper.rs", "pub fn helper() {}"),
            ]);
            let reference = call(&facts, "src/lib.rs", "outer::api::helper");
            let target = capability_symbol(&facts, "shared/helper.rs", "helper");
            assert_eq!(
                reference.target_symbol_id.as_ref(),
                (!visibility.is_empty()).then_some(&target.symbol_id),
                "{source}"
            );
        }
    }
}

#[test]
fn rust_path_attribute_rejects_shadowed_and_duplicate_module_names() {
    for declaration in [
        "mod api;",
        "#[path = \"other.rs\"] mod api;",
        "#[unknown] mod api;",
        "mod api { pub fn helper() {} }",
    ] {
        for source in [
            format!(
                "#[path = \"helper.rs\"] mod api; {declaration} pub fn use_it() {{ api::helper(); }}"
            ),
            format!(
                "{declaration} #[path = \"helper.rs\"] mod api; pub fn use_it() {{ api::helper(); }}"
            ),
        ] {
            let facts = generation(&[
                ("lib.rs", &source),
                ("helper.rs", "pub fn helper() {}"),
                ("other.rs", "pub fn helper() {}"),
                ("api.rs", "pub fn helper() {}"),
            ]);
            assert!(
                call(&facts, "lib.rs", "api::helper")
                    .target_symbol_id
                    .is_none(),
                "{source}"
            );
        }
    }
    for shadow in [
        "type api = Unproven;",
        "struct api;",
        "inject_unknown_items!();",
    ] {
        let source = format!(
            "#[path = \"helper.rs\"] mod api; pub fn use_it() {{ {shadow} api::helper(); }}"
        );
        let facts = generation(&[("lib.rs", &source), ("helper.rs", "pub fn helper() {}")]);
        assert!(
            call(&facts, "lib.rs", "api::helper")
                .target_symbol_id
                .is_none(),
            "{shadow}"
        );
    }
}

#[test]
fn rust_path_attribute_index_and_resolution_propagate_every_cancellation_poll() {
    use super::rust_module_paths::{visibility_request, visibility_resolution_index};
    let fixtures = [
        ("Cargo.toml", CARGO),
        (
            "src/lib.rs",
            "#[path = \"../shared/odd.rs\"] mod api; pub fn run() { api::helper(); }",
        ),
        ("shared/odd.rs", "pub mod child; pub(super) fn helper() {}"),
        ("shared/child.rs", "pub fn child() {}"),
    ];
    let (index, span) = visibility_resolution_index(&fixtures, ("src/lib.rs", "api::helper"));
    let request = visibility_request(&index, ("src/lib.rs", span, "api::helper"));
    let scope = super::super::rust_inline_modules::ModuleScope {
        file: request.file_id,
        inline: "",
        module: None,
    };
    let mut polls = 0;
    let result = super::super::rust_path_resolution::resolve_in_scope(
        &index,
        (&request, scope, request.name),
        &mut || {
            polls += 1;
            false
        },
    )
    .unwrap_or_else(|_| panic!("resolution failed"));
    assert!(result.is_some());
    for cancel_at in 1..=polls {
        let mut observed = 0;
        let result = super::super::rust_path_resolution::resolve_in_scope(
            &index,
            (&request, scope, request.name),
            &mut || {
                observed += 1;
                observed == cancel_at
            },
        );
        assert!(result.is_err(), "resolution poll {cancel_at}");
    }
    let mut polls = 0;
    reindex(&fixtures, &mut || {
        polls += 1;
        false
    })
    .unwrap_or_else(|_| panic!("reindex failed"));
    for cancel_at in 1..=polls {
        let mut observed = 0;
        assert!(
            reindex(&fixtures, &mut || {
                observed += 1;
                observed == cancel_at
            })
            .is_err(),
            "index poll {cancel_at}"
        );
    }
}

fn reindex<Cancel>(
    fixtures: &[(&str, &str)],
    cancelled: &mut Cancel,
) -> Result<(), super::StageItemFailure>
where
    Cancel: FnMut() -> bool,
{
    let (mut index, _) = super::rust_module_paths::visibility_resolution_index(
        fixtures,
        ("src/lib.rs", "api::helper"),
    );
    let mut budget = super::ResolveBudget::new(0, super::TEST_GENERATION_BYTES)?;
    super::super::rust_root_ownership::index(
        &mut super::ResolutionIndexTarget {
            index: &mut index,
            budget: &mut budget,
        },
        cancelled,
    )
}
