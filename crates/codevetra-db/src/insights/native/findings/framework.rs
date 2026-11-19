//! Fixed framework and fixture exclusions are compiled once and remain source-specific.

use crate::{SymbolInput, retrieval::native::simple_name, rocks::RocksError};
use std::sync::OnceLock;

struct Rules {
    fixture: regex::Regex,
    camel_test: regex::Regex,
    production: regex::Regex,
    routes: regex::Regex,
    app_routes: regex::Regex,
    root: regex::Regex,
    server: regex::Regex,
    next: regex::Regex,
    route: regex::Regex,
    metadata: regex::Regex,
    middleware: regex::Regex,
    instrumentation: regex::Regex,
    config: regex::Regex,
}
fn rules() -> Result<&'static Rules, RocksError> {
    static RULES: OnceLock<Result<Rules, RocksError>> = OnceLock::new();
    RULES.get_or_init(|| {
        let compile=|pattern| regex::RegexBuilder::new(pattern).size_limit(1_048_576).dfa_size_limit(1_048_576)
            .build().map_err(|_| RocksError::Corrupt {field:"finding_exclusions"});
        Ok(Rules {
            fixture:compile(r"(?i)(^|/)(fixtures?|test-beds?|__tests__|__mocks__|tests?|specs?|integration|testing|testlib|bench(es|marks?)?|scripts?|examples?|samples?|demos?)(/|$)|(\.test\.|\.spec\.|_test\.|_spec\.)[a-z0-9]+$")?,
            camel_test:compile(r"([A-Za-z](Test|Tests|TestCase|Spec))\.[a-z0-9]+$")?,
            production:compile(r"(?i)(^|/)(test[-_][^/]*|test|mocks?|fixtures?)\.[a-z0-9]+$|\.(gen|generated)\.[a-z0-9]+$|^(publish|release|build|deploy|bundle|prepublish|postinstall)\.[mc]?[jt]s$")?,
            routes:compile(r"(^|/)routes/.*\.[jt]sx?$")?,
            app_routes:compile(r"(^|/)app/routes/.*\.[jt]sx?$")?,
            root:compile(r"(^|/)app/root\.[jt]sx?$")?,
            server:compile(r"(^|/)app/entry\.server\.[jt]sx?$")?,
            next:compile(r"(^|/)app/(.*/)?(page|layout|template|route|error|global-error|loading|not-found|default|sitemap|robots|manifest|icon|apple-icon|opengraph-image|twitter-image)\.(ts|tsx|js|jsx|mjs)$")?,
            route:compile(r"(^|/)app/(.*/)?route\.(ts|tsx|js|jsx|mjs)$")?,
            metadata:compile(r"(^|/)app/(.*/)?(page|layout|template|error|global-error|loading|not-found|default|sitemap|robots|manifest|icon|apple-icon|opengraph-image|twitter-image)\.(ts|tsx|js|jsx|mjs)$")?,
            middleware:compile(r"^(src/)?middleware\.(ts|js|mjs)$")?,
            instrumentation:compile(r"^(src/)?instrumentation\.(ts|js|mjs)$")?,
            config:compile(r"(^|/)[^/]+\.config\.(ts|tsx|js|jsx|mjs|cjs)$")?,
        })
    }).as_ref().map_err(|error|*error)
}
pub(super) fn fixture(path: &str) -> Result<bool, RocksError> {
    let rules = rules()?;
    Ok(rules.fixture.is_match(path) || rules.camel_test.is_match(path))
}
pub(super) fn production(path: &str) -> Result<bool, RocksError> {
    Ok(!fixture(path)? && !rules()?.production.is_match(path))
}
pub(super) fn convention(
    symbol: &SymbolInput,
    path: &str,
    parent_default: bool,
) -> Result<bool, RocksError> {
    let rules = rules()?;
    let name = simple_name(&symbol.qualified_name);
    Ok(rules.react_router(symbol, path)
        || rules.next_js(symbol, path)
        || (rules.config.is_match(path) && symbol.symbol_kind == "method")
        || worker_method(symbol, name, parent_default))
}

impl Rules {
    fn react_router(&self, symbol: &SymbolInput, path: &str) -> bool {
        let name = simple_name(&symbol.qualified_name);
        let router = matches!(
            name,
            "loader"
                | "clientLoader"
                | "action"
                | "clientAction"
                | "headers"
                | "handle"
                | "links"
                | "meta"
                | "shouldRevalidate"
                | "middleware"
                | "clientMiddleware"
                | "unstable_middleware"
                | "unstable_clientMiddleware"
                | "ErrorBoundary"
                | "HydrateFallback"
        );
        (self.routes.is_match(path) && name == "Route")
            || (self.app_routes.is_match(path) && (symbol.export.default_export || router))
            || (self.root.is_match(path)
                && (symbol.export.default_export || router || name == "Layout"))
            || (self.server.is_match(path)
                && (symbol.export.default_export
                    || matches!(
                        name,
                        "handleRequest" | "handleDataRequest" | "handleError" | "streamTimeout"
                    )))
    }

    fn next_js(&self, symbol: &SymbolInput, path: &str) -> bool {
        let name = simple_name(&symbol.qualified_name);
        (self.next.is_match(path)
            && matches!(
                name,
                "dynamic"
                    | "dynamicParams"
                    | "revalidate"
                    | "fetchCache"
                    | "runtime"
                    | "preferredRegion"
                    | "maxDuration"
                    | "experimental_ppr"
                    | "generateStaticParams"
                    | "generateImageMetadata"
                    | "generateSitemaps"
            ))
            || (self.route.is_match(path)
                && matches!(
                    name,
                    "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
                ))
            || (self.metadata.is_match(path)
                && matches!(
                    name,
                    "metadata" | "generateMetadata" | "viewport" | "generateViewport"
                ))
            || (self.middleware.is_match(path) && matches!(name, "middleware" | "config"))
            || (self.instrumentation.is_match(path)
                && matches!(name, "register" | "onRequestError"))
    }
}

fn worker_method(symbol: &SymbolInput, name: &str, parent_default: bool) -> bool {
    symbol.symbol_kind == "method"
        && (symbol.export.default_export || parent_default)
        && matches!(
            name,
            "fetch"
                | "scheduled"
                | "queue"
                | "email"
                | "alarm"
                | "webSocketMessage"
                | "webSocketClose"
                | "webSocketError"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use codevetra_domain::{ContentDigest, FileId, SymbolId};

    fn symbol(name: &str, kind: &str, default_export: bool) -> SymbolInput {
        SymbolInput {
            symbol_id: SymbolId::from_uuid_v8([1; 16]),
            file_id: FileId::from_uuid_v8([2; 16]),
            symbol_kind: kind.into(),
            qualified_name: name.into(),
            signature: String::new(),
            start_byte: 0,
            end_byte: 1,
            start_line: 1,
            end_line: 1,
            structural_digest: ContentDigest::from_bytes([3; 32]),
            visibility: None,
            export: codevetra_domain::SymbolExportFlags {
                default_export,
                ..Default::default()
            },
            execution: codevetra_domain::SymbolExecutionFlags::default(),
            implementation: codevetra_domain::SymbolImplementationFlags::default(),
            betweenness_ppb: None,
            pagerank_ppb: None,
        }
    }

    #[test]
    fn convention_names_stay_bound_to_their_framework_paths() {
        for (path, name, expected) in [
            ("routes/account.ts", "Route", true),
            ("src/account.ts", "Route", false),
            ("app/routes/account.tsx", "loader", true),
            ("app/root.tsx", "Layout", true),
            ("app/routes/account.tsx", "Layout", false),
            ("app/entry.server.tsx", "handleRequest", true),
            ("app/root.tsx", "handleRequest", false),
            ("app/account/page.tsx", "generateMetadata", true),
            ("app/account/route.ts", "generateMetadata", false),
            ("app/account/route.ts", "GET", true),
            ("app/account/page.tsx", "GET", false),
            ("app/account/page.tsx", "dynamic", true),
            ("src/account.ts", "dynamic", false),
            ("src/middleware.ts", "middleware", true),
            ("src/middleware.ts", "onRequestError", false),
            ("src/instrumentation.ts", "onRequestError", true),
            ("src/instrumentation.ts", "middleware", false),
        ] {
            assert_eq!(
                convention(&symbol(name, "function", false), path, false),
                Ok(expected),
                "{path}: {name}"
            );
        }
    }

    #[test]
    fn default_exports_and_worker_methods_keep_their_distinct_ownership_rules() {
        assert_eq!(
            convention(&symbol("entry", "function", true), "app/root.tsx", false),
            Ok(true)
        );
        assert_eq!(
            convention(&symbol("entry", "function", false), "app/root.tsx", false),
            Ok(false)
        );
        assert_eq!(
            convention(&symbol("entry", "function", true), "src/entry.ts", false),
            Ok(false)
        );
        assert_eq!(
            convention(&symbol("fetch", "method", false), "src/worker.ts", true),
            Ok(true)
        );
        assert_eq!(
            convention(&symbol("fetch", "method", false), "src/worker.ts", false),
            Ok(false)
        );
        assert_eq!(
            convention(&symbol("fetch", "function", true), "src/worker.ts", false),
            Ok(false)
        );
        assert_eq!(
            convention(
                &symbol("configure", "method", false),
                "vite.config.ts",
                false
            ),
            Ok(true)
        );
        assert_eq!(
            convention(
                &symbol("configure", "function", false),
                "vite.config.ts",
                false
            ),
            Ok(false)
        );
    }
}
