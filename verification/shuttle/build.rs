use std::{env, fs, path::PathBuf};

fn main() {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("manifest directory"));
    let root = manifest
        .parent()
        .and_then(|p| p.parent())
        .expect("repository root");
    let request_path = root.join("crates/codevetra-db/src/rocks/runtime.rs");
    let group_path = root.join("crates/codevetra-db/src/rocks/runtime/group.rs");
    let source = fs::read_to_string(&request_path).expect("live storage request source");
    let start = "#[derive(Clone, Default)]\nstruct Cancellation";
    let end = "/// Async access to the shared, multithreaded owner of one project database.";
    assert_eq!(source.matches(start).count(), 1, "ambiguous request start");
    assert_eq!(source.matches(end).count(), 1, "ambiguous request end");
    let begin = source.find(start).expect("request start");
    let finish = source.find(end).expect("request end");
    assert!(begin < finish);
    let mut request = source[begin..finish].to_owned();
    let mut group = fs::read_to_string(&group_path).expect("live staging barrier source");

    // Only disposable negative-control builds alter executable source. A normal
    // build copies both production bodies verbatim, with imports supplied by the
    // instrumented parent module. Drift fails compilation or these exact checks.
    let mutation = env::var("CODEVETRA_SHUTTLE_MUTATION").unwrap_or_default();
    match mutation.as_str() {
        "" => {}
        "lost-notification" => {
            let correct = "let changed = self.inner.changed.notified();\n        tokio::pin!(changed);\n        changed.as_mut().enable();\n        if !self.is_cancelled() {\n            changed.await;\n        }";
            assert_eq!(
                request.matches(correct).count(),
                1,
                "cancellation binding drift"
            );
            request = request.replace(
                correct,
                "if !self.is_cancelled() { self.inner.changed.notified().await; }",
            );
        }
        "premature-drain" => {
            assert_eq!(
                group.matches("if state.pending == 0 {").count(),
                1,
                "barrier binding drift"
            );
            group = group.replace("if state.pending == 0 {", "if true {");
        }
        _ => panic!("unknown Shuttle negative control"),
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("output directory"));
    fs::write(out.join("request.rs"), request).expect("bound request source");
    fs::write(out.join("group.rs"), group).expect("bound group source");
    println!("cargo:rerun-if-changed={}", request_path.display());
    println!("cargo:rerun-if-changed={}", group_path.display());
    println!("cargo:rerun-if-env-changed=CODEVETRA_SHUTTLE_MUTATION");
}
