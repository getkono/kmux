//! The workspace dependency graph, read from `cargo metadata`.
//!
//! This is the data the architecture assertions in
//! `xtask/tests/dependency_direction.rs` run against — the executable form of
//! rule R5 in [docs/crate-usage.md](../../docs/crate-usage.md), which declares
//! that "layering is a dependency rule, not a convention".

use std::collections::{BTreeSet, HashMap, VecDeque};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    resolve: Resolve,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<NodeDep>,
}

#[derive(Deserialize)]
struct NodeDep {
    pkg: String,
    dep_kinds: Vec<DepKind>,
}

#[derive(Deserialize)]
struct DepKind {
    /// `None` for a normal dependency; `Some("dev")` or `Some("build")`
    /// otherwise. Serde maps JSON `null` to `None`.
    kind: Option<String>,
}

/// The resolved package graph, keyed by package name.
pub struct Graph {
    /// Names of the crates that make up this workspace.
    pub members: BTreeSet<String>,
    /// Package name → the names it depends on. Which edge kinds are included
    /// depends on how the graph was built; see [`Graph::load`].
    edges: HashMap<String, BTreeSet<String>>,
}

impl Graph {
    /// Resolve the graph over **normal and build** dependencies, dropping
    /// `dev` edges.
    ///
    /// Dev-dependency cycles are legal in cargo and this workspace already has
    /// one shape of it (`kmuxd` depends on `kmux-vt-core` both normally and as
    /// a dev-dependency, for its `test-util` feature), so including dev edges
    /// would make the acyclicity assertion meaningless.
    ///
    /// `--filter-platform` is deliberately **not** passed. Without it cargo
    /// resolves every target-gated dependency regardless of host, so the GTK
    /// stack — which `kmux-gtk` gates to Linux and macOS — appears in the graph
    /// on every platform. That is exactly what R5 wants: a toolkit must not
    /// appear at or below `kmux-app` *on any platform*, and the platform-union
    /// graph is both the strictest reading and the only one that gives the same
    /// verdict on the Linux and macOS CI runners. Do not "fix" this by adding
    /// `--filter-platform`; it would narrow the check to whatever host happens
    /// to run it.
    pub fn load() -> Result<Self> {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let out = std::process::Command::new(cargo)
            .args(["metadata", "--format-version", "1", "--locked"])
            .output()
            .context("running `cargo metadata`")?;
        anyhow::ensure!(
            out.status.success(),
            "`cargo metadata` failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        Self::from_json(&out.stdout)
    }

    fn from_json(json: &[u8]) -> Result<Self> {
        let md: Metadata =
            serde_json::from_slice(json).context("parsing `cargo metadata` output")?;

        let name_of: HashMap<&str, &str> = md
            .packages
            .iter()
            .map(|p| (p.id.as_str(), p.name.as_str()))
            .collect();

        let lookup = |id: &str| -> Result<String> {
            name_of
                .get(id)
                .map(|s| (*s).to_owned())
                .with_context(|| format!("package id absent from metadata: {id}"))
        };

        let members = md
            .workspace_members
            .iter()
            .map(|id| lookup(id))
            .collect::<Result<BTreeSet<_>>>()?;

        let mut edges: HashMap<String, BTreeSet<String>> = HashMap::new();
        for node in &md.resolve.nodes {
            let from = lookup(&node.id)?;
            let to = node
                .deps
                .iter()
                .filter(|d| {
                    // Keep normal (`None`) and `build` edges; drop `dev`.
                    d.dep_kinds
                        .iter()
                        .any(|k| k.kind.as_deref().is_none_or(|k| k == "build"))
                })
                .map(|d| lookup(&d.pkg))
                .collect::<Result<BTreeSet<_>>>()?;
            edges.entry(from).or_default().extend(to);
        }

        Ok(Self { members, edges })
    }

    /// Every package reachable from `root`, excluding `root` itself.
    pub fn reachable_from(&self, root: &str) -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([root.to_owned()]);
        while let Some(pkg) = queue.pop_front() {
            for dep in self.edges.get(&pkg).into_iter().flatten() {
                if seen.insert(dep.clone()) {
                    queue.push_back(dep.clone());
                }
            }
        }
        seen.remove(root);
        seen
    }

    /// The shortest dependency path from `root` to any package in `targets`,
    /// as a chain of names, or `None` if none is reachable.
    ///
    /// Reporting the path is the whole point: "gtk4 is reachable from kmux-app"
    /// is not actionable, but `kmux-app -> kmux-client -> gtk4` names the edge
    /// somebody added.
    pub fn shortest_path_to_any(
        &self,
        root: &str,
        targets: &BTreeSet<String>,
    ) -> Option<Vec<String>> {
        let mut prev: HashMap<String, String> = HashMap::new();
        let mut seen = BTreeSet::from([root.to_owned()]);
        let mut queue = VecDeque::from([root.to_owned()]);

        while let Some(pkg) = queue.pop_front() {
            if targets.contains(&pkg) {
                let mut path = vec![pkg.clone()];
                let mut cur = pkg;
                while let Some(p) = prev.get(&cur) {
                    path.push(p.clone());
                    cur = p.clone();
                }
                path.reverse();
                return Some(path);
            }
            for dep in self.edges.get(&pkg).into_iter().flatten() {
                if seen.insert(dep.clone()) {
                    prev.insert(dep.clone(), pkg.clone());
                    queue.push_back(dep.clone());
                }
            }
        }
        None
    }

    /// A cycle among workspace members, as a chain `a -> b -> … -> a`, if one
    /// exists. Edges to non-members are ignored: a cycle through a third-party
    /// crate is impossible, and including them would make the search needlessly
    /// large.
    pub fn member_cycle(&self) -> Option<Vec<String>> {
        let mut marks: HashMap<String, Mark> = HashMap::new();
        let mut stack: Vec<String> = Vec::new();

        for start in &self.members {
            if let Some(cycle) = self.visit(start, &mut marks, &mut stack) {
                return Some(cycle);
            }
        }
        None
    }

    fn visit(
        &self,
        pkg: &str,
        marks: &mut HashMap<String, Mark>,
        stack: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        match marks.get(pkg) {
            Some(Mark::Done) => return None,
            Some(Mark::InProgress) => {
                // Back-edge. Report the cycle from where this package first
                // entered the stack, so the message reads `a -> b -> … -> a`.
                let at = stack
                    .iter()
                    .position(|p| p == pkg)
                    .expect("an in-progress package is always on the stack");
                let mut cycle = stack[at..].to_vec();
                cycle.push(pkg.to_owned());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(pkg.to_owned(), Mark::InProgress);
        stack.push(pkg.to_owned());
        for dep in self.edges.get(pkg).into_iter().flatten() {
            // Edges to non-members are ignored: a cycle cannot run through a
            // third-party crate, and following them would blow up the search.
            if self.members.contains(dep)
                && let Some(cycle) = self.visit(dep, marks, stack)
            {
                return Some(cycle);
            }
        }
        stack.pop();
        marks.insert(pkg.to_owned(), Mark::Done);
        None
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    InProgress,
    Done,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hand-written metadata document, so the graph logic is tested without
    /// shelling out to cargo (docs/testing.md R7: prefer the pure tier).
    fn sample_metadata() -> Vec<u8> {
        serde_json::json!({
            "packages": [
                { "id": "a 1.0", "name": "a" },
                { "id": "b 1.0", "name": "b" },
                { "id": "c 1.0", "name": "c" },
                { "id": "toolkit 1.0", "name": "toolkit" },
                { "id": "devonly 1.0", "name": "devonly" }
            ],
            "workspace_members": ["a 1.0", "b 1.0", "c 1.0"],
            "resolve": {
                "nodes": [
                    { "id": "a 1.0", "deps": [
                        { "pkg": "b 1.0", "dep_kinds": [{ "kind": null }] },
                        { "pkg": "devonly 1.0", "dep_kinds": [{ "kind": "dev" }] }
                    ]},
                    { "id": "b 1.0", "deps": [
                        { "pkg": "c 1.0", "dep_kinds": [{ "kind": "build" }] }
                    ]},
                    { "id": "c 1.0", "deps": [
                        { "pkg": "toolkit 1.0", "dep_kinds": [{ "kind": null }] }
                    ]},
                    { "id": "toolkit 1.0", "deps": [] },
                    { "id": "devonly 1.0", "deps": [] }
                ]
            }
        })
        .to_string()
        .into_bytes()
    }

    fn sample_graph() -> Graph {
        Graph::from_json(&sample_metadata()).expect("sample metadata parses")
    }

    #[test]
    fn members_are_the_workspace_packages_only() {
        let g = sample_graph();
        assert_eq!(
            g.members,
            BTreeSet::from(["a".to_owned(), "b".to_owned(), "c".to_owned()]),
            "third-party packages must not be reported as members"
        );
    }

    #[test]
    fn reachability_follows_normal_and_build_edges() {
        let g = sample_graph();
        assert_eq!(
            g.reachable_from("a"),
            BTreeSet::from(["b".to_owned(), "c".to_owned(), "toolkit".to_owned()]),
            "a -> b (normal) -> c (build) -> toolkit must all be reachable"
        );
    }

    #[test]
    fn reachability_drops_dev_edges() {
        let g = sample_graph();
        assert!(
            !g.reachable_from("a").contains("devonly"),
            "a dev-dependency must not appear in the graph, or acyclicity is meaningless"
        );
    }

    #[test]
    fn shortest_path_names_the_offending_chain() {
        let g = sample_graph();
        let path = g
            .shortest_path_to_any("a", &BTreeSet::from(["toolkit".to_owned()]))
            .expect("toolkit is reachable from a");
        assert_eq!(path, vec!["a", "b", "c", "toolkit"]);
    }

    #[test]
    fn shortest_path_is_none_when_unreachable() {
        let g = sample_graph();
        assert_eq!(
            g.shortest_path_to_any("c", &BTreeSet::from(["a".to_owned()])),
            None,
            "the graph is directed; a must not be reachable from c"
        );
    }

    #[test]
    fn acyclic_graph_reports_no_cycle() {
        assert_eq!(sample_graph().member_cycle(), None);
    }

    #[test]
    fn cycle_among_members_is_reported_as_a_chain() {
        let json = serde_json::json!({
            "packages": [
                { "id": "a 1.0", "name": "a" },
                { "id": "b 1.0", "name": "b" }
            ],
            "workspace_members": ["a 1.0", "b 1.0"],
            "resolve": { "nodes": [
                { "id": "a 1.0", "deps": [{ "pkg": "b 1.0", "dep_kinds": [{ "kind": null }] }] },
                { "id": "b 1.0", "deps": [{ "pkg": "a 1.0", "dep_kinds": [{ "kind": null }] }] }
            ]}
        })
        .to_string()
        .into_bytes();
        let cycle = Graph::from_json(&json)
            .expect("metadata parses")
            .member_cycle()
            .expect("a <-> b is a cycle");
        assert_eq!(
            cycle.first(),
            cycle.last(),
            "a reported cycle must start and end at the same package: {cycle:?}"
        );
        assert!(cycle.len() >= 3, "expected a -> b -> a, got {cycle:?}");
    }
}
