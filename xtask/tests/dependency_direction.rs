//! Executable enforcement of the crate layering.
//!
//! [docs/crate-usage.md](../../../docs/crate-usage.md) R5 says "layering is a
//! dependency rule, not a convention", and `crates/kmux-app/src/lib.rs` says
//! "Hard rule: nothing in this crate may depend on a UI toolkit". Until this
//! file existed, nothing checked either — the rules held only for as long as
//! everyone remembered them.
//!
//! These assertions read the platform-union graph (see `Graph::load` for why no
//! `--filter-platform`), so they give the same verdict on every host and it is
//! enough to run them on one. They live in a test target rather than a CI step
//! so they ride `mise run test` and the pre-push hook automatically.

use std::collections::BTreeSet;

use xtask::graph::Graph;

/// Every workspace member that must never reach a UI toolkit.
///
/// R5 phrases the rule as "at or below `kmux-app`", but this is written as an
/// explicit set rather than a graph position on purpose: `kmux-render` sits
/// *above* `kmux-app` in the layering yet is equally toolkit-free by its own
/// rule, and `kmuxd` is off to the side entirely. An explicit list also makes
/// `every_workspace_member_is_classified` possible, which is what stops a new
/// crate from silently escaping every other assertion here.
const TOOLKIT_FREE: &[&str] = &[
    "kmux",
    "kmux-app",
    "kmux-client",
    "kmux-connect",
    "kmux-ffi",
    "kmux-ghostty",
    "kmux-ghostty-sys",
    "kmux-protocol",
    "kmux-pty",
    "kmux-render",
    "kmux-vt-core",
    "kmux-vt-worker",
    "kmux-worker-protocol",
    "kmuxd",
];

/// The one member allowed to depend on a UI toolkit.
const TOOLKIT_FRONTENDS: &[&str] = &["kmux-gtk"];

/// Workspace tooling, exempt from the product layering rules.
const TOOLING: &[&str] = &["xtask"];

/// GUI toolkit crates and their transitive `-sys` companions. `kmux-gtk` gates
/// these to Linux and macOS, but the graph is resolved platform-union, so they
/// are visible here on every host — which is the point.
const TOOLKIT_CRATES: &[&str] = &[
    "gtk4",
    "gtk4-sys",
    "gdk4",
    "gdk4-sys",
    "gdk-pixbuf",
    "gdk-pixbuf-sys",
    "libadwaita",
    "libadwaita-sys",
    "glib",
    "glib-sys",
    "gio",
    "gio-sys",
    "gobject-sys",
    "pango",
    "pango-sys",
    "pangocairo",
    "pangocairo-sys",
    "cairo-rs",
    "cairo-sys-rs",
    "yeslogic-fontconfig-sys",
    // Other Rust GUI stacks, listed so adopting one is a deliberate act that
    // updates this file rather than an accident that slips through.
    "iced",
    "egui",
    "winit",
    "tao",
    "wry",
    "slint",
    "druid",
    "dioxus",
];

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn no_ui_toolkit_reaches_the_toolkit_free_crates() {
    let g = Graph::load().expect("cargo metadata");
    let toolkit = names(TOOLKIT_CRATES);

    for member in TOOLKIT_FREE {
        assert!(
            g.members.contains(*member),
            "{member} is listed in TOOLKIT_FREE but is not a workspace member; \
             update this test when a crate is renamed or removed"
        );
        if let Some(path) = g.shortest_path_to_any(member, &toolkit) {
            panic!(
                "crate-usage.md R5 violated: `{member}` can reach a UI toolkit.\n  \
                 {}\n\
                 Nothing at or below kmux-app may depend on a toolkit. If this edge is \
                 intended, change the rule in docs/crate-usage.md in the same commit.",
                path.join(" -> ")
            );
        }
    }
}

#[test]
fn kmux_protocol_depends_on_no_internal_crate() {
    let g = Graph::load().expect("cargo metadata");
    let internal: Vec<String> = g
        .reachable_from("kmux-protocol")
        .into_iter()
        .filter(|d| g.members.contains(d))
        .collect();
    assert!(
        internal.is_empty(),
        "kmux-protocol is the shared vocabulary every other crate builds on, so it \
         must depend on no internal crate. It now reaches: {internal:?}"
    );
}

#[test]
fn the_internal_dependency_graph_is_acyclic() {
    let g = Graph::load().expect("cargo metadata");
    if let Some(cycle) = g.member_cycle() {
        panic!(
            "the internal dependency graph has a cycle: {}\n\
             kmuxd depends on kmux-client and kmux-connect deliberately; this assertion \
             is what keeps those edges from ever growing a return path.",
            cycle.join(" -> ")
        );
    }
}

#[test]
fn worker_protocol_never_reaches_a_gui_frontend() {
    let g = Graph::load().expect("cargo metadata");
    // The daemon<->worker contract is server-side only. A GUI frontend that
    // reached it would be able to speak the worker protocol, which is a
    // process-isolation boundary, not a client-facing one.
    let client_side = [
        "kmux",
        "kmux-app",
        "kmux-client",
        "kmux-connect",
        "kmux-ffi",
        "kmux-gtk",
        "kmux-render",
    ];
    for crate_name in client_side {
        assert!(
            !g.reachable_from(crate_name)
                .contains("kmux-worker-protocol"),
            "`{crate_name}` reaches kmux-worker-protocol, which is a server-side \
             contract (see docs/architecture-process-isolation.md)"
        );
    }
}

#[test]
fn every_workspace_member_is_classified() {
    let g = Graph::load().expect("cargo metadata");
    let classified: BTreeSet<String> = names(TOOLKIT_FREE)
        .union(&names(TOOLKIT_FRONTENDS))
        .cloned()
        .collect::<BTreeSet<_>>()
        .union(&names(TOOLING))
        .cloned()
        .collect();

    let unclassified: Vec<&String> = g.members.difference(&classified).collect();
    assert!(
        unclassified.is_empty(),
        "new workspace member(s) {unclassified:?} are not classified in this test, so \
         they silently escape every layering assertion above. Add each to TOOLKIT_FREE, \
         TOOLKIT_FRONTENDS, or TOOLING — and to the crate groups in mise-tasks/mutants."
    );

    let stale: Vec<&String> = classified.difference(&g.members).collect();
    assert!(
        stale.is_empty(),
        "{stale:?} are classified here but are no longer workspace members"
    );
}
