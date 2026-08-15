//! Command-line entrypoint for the workspace quality tooling. See `lib.rs` for
//! why this crate exists and where it lives.

use anyhow::{Result, bail};
use xtask::graph::Graph;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("deps-graph") => print_graph(),
        Some(other) => bail!("unknown command `{other}`; known commands: deps-graph"),
        None => {
            eprintln!("usage: cargo run -p xtask -- <command>");
            eprintln!();
            eprintln!("commands:");
            eprintln!("  deps-graph   print the workspace dependency graph as it is asserted on");
            bail!("no command given")
        }
    }
}

/// Print the internal dependency graph. This is the debugging counterpart to
/// the assertions in `tests/dependency_direction.rs`: when one fails, this shows
/// the graph it was reading.
fn print_graph() -> Result<()> {
    let g = Graph::load()?;
    for member in &g.members {
        let internal: Vec<String> = g
            .reachable_from(member)
            .into_iter()
            .filter(|d| g.members.contains(d))
            .collect();
        println!("{member}");
        if internal.is_empty() {
            println!("    (no internal dependencies)");
        } else {
            println!("    {}", internal.join(", "));
        }
    }
    Ok(())
}
