use super::*;
use codevetra_domain::BoundarySite;
use std::fmt::Write as _;

fn reachable(facts: &CanonicalGenerationFacts, source: &SymbolInput, target: &SymbolInput) -> bool {
    let mut pending = vec![source.symbol_id.clone()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(id) = pending.pop() {
        if id == target.symbol_id {
            return true;
        }
        if !seen.insert(id.clone()) {
            continue;
        }
        for edge in facts.edges().iter().filter(|edge| {
            edge.source_symbol_id == id
                && matches!(
                    edge.kind,
                    EdgeKind::Calls
                        | EdgeKind::HttpRequest
                        | EdgeKind::QueueDispatch
                        | EdgeKind::EventDispatch
                        | EdgeKind::Navigates
                )
        }) {
            pending.push(edge.target_symbol_id.clone());
        }
    }
    false
}

#[test]
fn application_boundaries_connect_scoped_events_jobs_and_observer_callbacks() {
    let source = r"import { EventEmitter } from 'node:events';
import Queue from 'bull';
const stream = new EventEmitter(); const other = new EventEmitter();
const jobs = new Queue('receipts'); const listeners = new Set();
function onChange() {} function wrong() {} function processReceipt() {} function observer() {}
stream.on('changed', onChange); other.on('changed', wrong);
jobs.process('receipt', processReceipt); listeners.add(observer);
export function emitChange() { stream.emit('changed', 1); }
export function enqueue() { jobs.add('receipt', {}); }
export function dispatch() { listeners.forEach(listener => listener()); }
const values = new Set(); values.add(wrong); values.forEach(value => consume(value));
";
    let facts = bridge_details::generation(&[("src/channels.ts", source)]);
    for (source, target) in [
        ("emitChange", "onChange"),
        ("enqueue", "processReceipt"),
        ("dispatch", "observer"),
    ] {
        assert!(
            reachable(
                &facts,
                capability_symbol(&facts, "src/channels.ts", source),
                capability_symbol(&facts, "src/channels.ts", target)
            ),
            "missing {source} -> {target}: {:?}",
            facts
                .symbols()
                .iter()
                .filter(|symbol| symbol.symbol_kind == "resource")
                .collect::<Vec<_>>()
        );
    }
    assert!(!reachable(
        &facts,
        capability_symbol(&facts, "src/channels.ts", "emitChange"),
        capability_symbol(&facts, "src/channels.ts", "wrong")
    ));
    let sites = facts
        .symbols()
        .iter()
        .filter_map(|symbol| {
            BoundarySite::from_qualified_name(&symbol.qualified_name).map(|site| (symbol, site))
        })
        .collect::<Vec<_>>();
    assert_eq!(sites.len(), 7);
    for (symbol, _) in sites {
        assert!(symbol.start_byte < symbol.end_byte);
        assert!(symbol.signature.starts_with("boundary-state:"));
    }
}

#[test]
fn application_boundaries_next_same_origin_clients_navigation_and_parameters_are_scoped() {
    let fixtures = [
        (
            "apps/shop/app/checkout/page.tsx",
            r"'use client';
import Link from 'next/link'; import { useRouter, redirect } from 'next/navigation'; import axios from 'axios';
const client = axios.create({baseURL:'/'});
export default function Checkout() { const router = useRouter(); fetch('/api/receipt', {method:'POST'}); client.post('/api/receipt'); router.push('/done/42'); return <Link href='/done/42'>Go</Link>; }
export function jump() { redirect('/done/42'); }
",
        ),
        (
            "apps/shop/app/api/receipt/route.ts",
            "export async function POST() { return 1; } export function GET() { return 2; }",
        ),
        (
            "apps/shop/app/done/[id]/page.tsx",
            "export default function Receipt() { return <div/>; }",
        ),
        (
            "apps/admin/app/api/receipt/route.ts",
            "export function POST() { return 3; }",
        ),
    ];
    let facts = bridge_details::generation(&fixtures);
    let root = capability_symbol(&facts, fixtures[0].0, "Checkout");
    assert!(
        reachable(
            &facts,
            root,
            capability_symbol(&facts, fixtures[1].0, "POST")
        ),
        "{:?}",
        facts
            .symbols()
            .iter()
            .filter(|symbol| symbol.symbol_kind == "resource")
            .collect::<Vec<_>>()
    );
    assert!(reachable(
        &facts,
        root,
        capability_symbol(&facts, fixtures[2].0, "Receipt")
    ));
    assert!(!reachable(
        &facts,
        root,
        capability_symbol(&facts, fixtures[1].0, "GET")
    ));
    assert!(!reachable(
        &facts,
        root,
        capability_symbol(&facts, fixtures[3].0, "POST")
    ));
    assert!(reachable(
        &facts,
        capability_symbol(&facts, fixtures[0].0, "jump"),
        capability_symbol(&facts, fixtures[2].0, "Receipt")
    ));
}

#[test]
fn application_boundaries_unknown_external_ambiguous_and_configured_routes_do_not_guess() {
    let source = "'use client'; export function send(destination: string) { fetch(destination); fetch('https://elsewhere.invalid/api'); fetch('/api'); }";
    let fixtures = [
        ("app/client.ts", source),
        ("app/(one)/api/route.ts", "export function GET() {}"),
        ("app/(two)/api/route.ts", "export function GET() {}"),
    ];
    let facts = bridge_details::generation(&fixtures);
    for state in [
        "dynamic_or_unproven_identity",
        "external_destination",
        "ambiguous_registrations",
    ] {
        assert!(
            facts
                .symbols()
                .iter()
                .any(|symbol| symbol.signature == format!("boundary-state:{state}")),
            "missing {state}"
        );
    }
    assert!(
        !facts
            .edges()
            .iter()
            .any(|edge| edge.provenance == "framework-boundary-scoped-match-v1")
    );
    let configured = bridge_details::generation(&[
        ("app/client.ts", source),
        ("app/api/route.ts", "export function GET() {}"),
        (
            "next.config.mjs",
            "export default { basePath: '/different' }; ",
        ),
    ]);
    assert!(
        configured
            .symbols()
            .iter()
            .any(|symbol| symbol.signature == "boundary-state:routing_configuration_unverified")
    );
    assert!(
        !configured
            .edges()
            .iter()
            .any(|edge| edge.provenance == "framework-boundary-scoped-match-v1")
    );
}

#[test]
fn application_boundaries_shadowed_receivers_and_subscriber_fanout_remain_unknown() {
    let shadowed = bridge_details::generation(&[(
        "events.ts",
        "import { EventEmitter } from 'node:events'; const bus = new EventEmitter(); function listener() {} bus.on('changed',listener); function emit(bus: unknown) { bus.emit('changed'); }",
    )]);
    assert!(
        !shadowed
            .edges()
            .iter()
            .any(|edge| edge.provenance == "framework-boundary-scoped-match-v1")
    );
    assert!(
        shadowed
            .symbols()
            .iter()
            .any(|symbol| symbol.signature == "boundary-state:dynamic_or_unproven_identity")
    );
    let mut source = "import { EventEmitter } from 'node:events'; const bus = new EventEmitter(); export function send() { bus.emit('changed'); }".to_owned();
    for index in 0..9 {
        write!(
            source,
            "function listener_{index}() {{}} bus.on('changed',listener_{index});"
        )
        .unwrap_or_else(|error| panic!("fixture: {error}"));
    }
    let fanout = bridge_details::generation(&[("events.ts", &source)]);
    assert!(
        fanout
            .symbols()
            .iter()
            .any(|symbol| symbol.signature == "boundary-state:subscriber_fanout_limit")
    );
    assert!(
        !fanout
            .edges()
            .iter()
            .any(|edge| edge.provenance == "framework-boundary-scoped-match-v1")
    );
}

#[test]
fn application_boundaries_react_router_nested_lazy_and_instance_identity() {
    let source = r"import { BrowserRouter, Routes, Route, Link, Navigate, useNavigate } from 'react-router-dom';
import { lazy } from 'react';
const Receipt = lazy(() => import('./Receipt'));
function Checkout() { const navigate = useNavigate(); navigate('/orders/42'); return <Link to='/orders/42'>Next</Link>; }
function Unrelated() { return <Link to='/orders/42'>Unscoped</Link>; }
function Other() { return <div/>; }
export function Main() { return <BrowserRouter basename='/shop'><Routes><Route path='/' element={<Checkout/>}/><Route path='orders'><Route path=':id' element={<Receipt/>}/></Route></Routes><Navigate to='/orders/42'/></BrowserRouter>; }
export function Admin() { return <BrowserRouter basename='/admin'><Routes><Route path='/orders/:id' element={<Other/>}/></Routes><Link to='/orders/42'>Admin</Link></BrowserRouter>; }
";
    let facts = bridge_details::generation(&[
        ("src/routes.tsx", source),
        (
            "src/Receipt.tsx",
            "export default function ReceiptScreen() { return <div/>; }",
        ),
    ]);
    let checkout = capability_symbol(&facts, "src/routes.tsx", "Checkout");
    let screen = capability_symbol(&facts, "src/Receipt.tsx", "ReceiptScreen");
    assert!(
        reachable(&facts, checkout, screen),
        "sites: {:?}; edges: {:?}",
        facts
            .symbols()
            .iter()
            .filter(|symbol| symbol.symbol_kind == "resource")
            .collect::<Vec<_>>(),
        facts.edges()
    );
    assert!(!reachable(
        &facts,
        checkout,
        capability_symbol(&facts, "src/routes.tsx", "Other")
    ));
    assert!(!reachable(
        &facts,
        capability_symbol(&facts, "src/routes.tsx", "Unrelated"),
        screen
    ));
    assert!(
        facts
            .edges()
            .iter()
            .any(|edge| edge.provenance == "native-react-lazy-default")
    );
    assert!(
        facts
            .symbols()
            .iter()
            .any(|symbol| symbol.signature == "boundary-state:dynamic_or_unproven_identity")
    );
}

#[test]
fn application_boundaries_receivers_do_not_escape_lexical_scope() {
    let facts = bridge_details::generation(&[(
        "events.ts",
        "import { EventEmitter } from 'node:events'; function listener() {} function setup() { const bus = new EventEmitter(); bus.on('changed', listener); } function send() { bus.emit('changed'); }",
    )]);
    assert!(
        !facts
            .edges()
            .iter()
            .any(|edge| edge.provenance == "framework-boundary-scoped-match-v1")
    );
}
