//! The dependency graph: what gates what.

use std::collections::{BTreeMap, HashSet};

use anyhow::Result;
use bd_core::{Dependency, DependencyType, Issue, IssueFilter, Status};
use serde_json::json;

use crate::cli::{DepCmd, GraphCmd};
use crate::commands::stub;
use crate::context::Ctx;
use crate::exit::{self, SilentExit};

pub async fn dep(ctx: &Ctx, cmd: DepCmd) -> Result<()> {
    match cmd {
        DepCmd::Add(a) => {
            ctx.ensure_writable("add a dependency")?;
            let store = ctx.store().await?;
            let mut d = Dependency::new(&a.issue, &a.depends_on, a.dep_type.clone())?;
            d.created_by = ctx.identity.actor.clone();
            store.add_dependency(&d).await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&d)?;
            } else {
                ctx.out.line(format!(
                    "{} now depends on {} [{}]",
                    a.issue, a.depends_on, a.dep_type
                ));
            }
            Ok(())
        }
        DepCmd::Remove {
            issue,
            depends_on,
            dep_type,
        } => remove(ctx, &issue, &depends_on, &dep_type).await,
        DepCmd::List { id } => list(ctx, &id).await,
        DepCmd::Tree { id, depth } => tree(ctx, &id, depth).await,
        DepCmd::Cycles => cycles(ctx).await,
        // `related` is an association: it records that two beads have something
        // to do with each other and gates nothing. `DependencyType::Related`
        // is the authority on that, not this line.
        DepCmd::Relate { from, to } => {
            ctx.ensure_writable("relate two issues")?;
            let store = ctx.store().await?;
            let mut d = Dependency::new(&from, &to, DependencyType::Related)?;
            d.created_by = ctx.identity.actor.clone();
            store.add_dependency(&d).await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&d)?;
            } else {
                ctx.out.line(format!("{from} is related to {to}"));
            }
            Ok(())
        }
        // Writable only since `remove_dependency` learned to take an edge type.
        // Before that this could not have been written honestly: dropping "the
        // relation" between two beads would have dropped every other edge
        // between them too, including whatever was blocking one on the other.
        DepCmd::Unrelate { from, to } => {
            remove(ctx, &from, &to, &DependencyType::Related).await
        }
    }
}

async fn remove(ctx: &Ctx, issue: &str, depends_on: &str, dep_type: &DependencyType) -> Result<()> {
    ctx.ensure_writable("remove a dependency")?;
    let store = ctx.store().await?;
    store.remove_dependency(issue, depends_on, dep_type).await?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "issue_id": issue,
            "depends_on_id": depends_on,
            "type": dep_type.as_str(),
            "removed": true,
        }))?;
    } else {
        ctx.out.line(format!(
            "{issue} no longer depends on {depends_on} [{dep_type}]"
        ));
    }
    Ok(())
}

async fn list(ctx: &Ctx, id: &str) -> Result<()> {
    let store = ctx.store().await?;
    let out_edges = store.dependencies_of(id).await?;
    let in_edges = store.dependents_of(id).await?;

    if ctx.out.is_json() {
        return ctx.out.json_value(&json!({
            "id": id,
            "depends_on": out_edges,
            "dependents": in_edges,
        }));
    }
    if out_edges.is_empty() && in_edges.is_empty() {
        ctx.out.line(format!("{id} has no dependencies."));
        return Ok(());
    }
    if !out_edges.is_empty() {
        ctx.out.line(format!("{id} depends on:"));
        for d in &out_edges {
            ctx.out
                .line(format!("  {} [{}]", d.depends_on_id, d.dep_type));
        }
    }
    if !in_edges.is_empty() {
        ctx.out.line(format!("depends on {id}:"));
        for d in &in_edges {
            ctx.out.line(format!("  {} [{}]", d.issue_id, d.dep_type));
        }
    }
    Ok(())
}

struct Frame {
    id: String,
    edge: Option<String>,
    prefix: String,
    last: bool,
    depth: u32,
}

/// What an issue is waiting on, drawn as a tree.
///
/// Iterative rather than recursive because the graph is *not* guaranteed
/// acyclic — `bd dep cycles` exists precisely because cycles happen — and a
/// naive recursion would blow the stack on one. Nodes already drawn are marked
/// and not re-expanded, which also keeps a diamond from doubling the output.
async fn tree(ctx: &Ctx, root: &str, max_depth: u32) -> Result<()> {
    let store = ctx.store().await?;
    if store.get_issue(root).await?.is_none() {
        anyhow::bail!("issue not found: {root}");
    }

    if ctx.out.is_json() {
        // JSON gets the edges, not the drawing: a client can build its own tree.
        let mut edges: Vec<Dependency> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue = vec![(root.to_string(), 0u32)];
        while let Some((id, d)) = queue.pop() {
            if d >= max_depth || !seen.insert(id.clone()) {
                continue;
            }
            for e in store.dependencies_of(&id).await? {
                queue.push((e.depends_on_id.clone(), d + 1));
                edges.push(e);
            }
        }
        return ctx
            .out
            .json_value(&json!({ "root": root, "edges": edges }));
    }

    let mut seen: HashSet<String> = HashSet::new();
    let mut stack = vec![Frame {
        id: root.to_string(),
        edge: None,
        prefix: String::new(),
        last: true,
        depth: 0,
    }];

    while let Some(f) = stack.pop() {
        let title = store
            .get_issue(&f.id)
            .await?
            .map(|i| i.title)
            .unwrap_or_else(|| "(missing)".to_string());

        let repeat = !seen.insert(f.id.clone());
        let line = if f.depth == 0 {
            format!("{} {}", f.id, title)
        } else {
            let branch = if f.last { "└── " } else { "├── " };
            let edge = f.edge.as_deref().unwrap_or("");
            format!(
                "{}{}{} [{}] {}{}",
                f.prefix,
                branch,
                f.id,
                edge,
                title,
                if repeat { "  (already shown)" } else { "" }
            )
        };
        ctx.out.line(line);

        if repeat || f.depth >= max_depth {
            continue;
        }
        let deps = store.dependencies_of(&f.id).await?;
        let child_prefix = if f.depth == 0 {
            String::new()
        } else {
            format!("{}{}", f.prefix, if f.last { "    " } else { "│   " })
        };
        // Pushed in reverse so the stack pops them in declaration order.
        for (i, d) in deps.iter().enumerate().rev() {
            stack.push(Frame {
                id: d.depends_on_id.clone(),
                edge: Some(d.dep_type.to_string()),
                prefix: child_prefix.clone(),
                last: i + 1 == deps.len(),
                depth: f.depth + 1,
            });
        }
    }
    Ok(())
}

async fn cycles(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let cycles = store.find_cycles().await?;
    if ctx.out.is_json() {
        return ctx.out.json_value(&cycles);
    }
    if cycles.is_empty() {
        ctx.out.line("No cycles: the graph is a DAG.");
        return Ok(());
    }
    ctx.out
        .line(format!("{} cycle(s):", cycles.len()));
    for c in &cycles {
        ctx.out.line(format!("  {}", c.join(" -> ")));
    }
    Ok(())
}

pub async fn recompute_blocked(ctx: &Ctx) -> Result<()> {
    ctx.ensure_writable("recompute blocked state")?;
    let store = ctx.store().await?;
    let n = store.recompute_blocked().await?;
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({ "updated": n }))?;
    } else {
        ctx.out
            .line(format!("Recomputed blocked state; {n} issue(s) changed."));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// graph
// ---------------------------------------------------------------------------

/// Bare `bd graph` renders the graph; `bd graph check` audits it. Two different
/// commands wearing one name, which is why the subcommand is optional.
///
/// There is deliberately no Mermaid renderer. The root's `--format` flag is
/// global and clap parses it, but [`Ctx`] only carries the boolean it distils it
/// into (`is_json`), so a handler cannot see whether it was handed `dot` or
/// `mermaid` — and sniffing argv here to find out would be a second, disagreeing
/// parser. Give `Ctx` the raw `format` and a Mermaid arm is ~15 lines; until
/// then, one honest format beats two guessed ones.
pub async fn graph(ctx: &Ctx, cmd: Option<GraphCmd>) -> Result<()> {
    match cmd {
        Some(GraphCmd::Check) => check(ctx).await,
        None => render(ctx).await,
    }
}

/// The whole graph, read once.
struct Graph {
    issues: Vec<Issue>,
    edges: Vec<Dependency>,
    /// What the store *currently believes* is blocked.
    ///
    /// Read from the store, never recomputed here. `is_blocked` has exactly one
    /// authority — the backend's fixpoint — and a second opinion computed in the
    /// CLI would not be a check on it, it would be a fork of the definition of
    /// "ready". If this set is wrong, the bug is in the backend and the fix is
    /// `bd recompute-blocked`, not a renderer that quietly disagrees.
    blocked: HashSet<String>,
}

async fn load(ctx: &Ctx) -> Result<Graph> {
    let store = ctx.store().await?;

    // Closed issues included. A graph that hides them cannot show you *why* the
    // open ones are free, which is most of what you look at a graph for.
    let mut issues = store.list_issues(&IssueFilter::new()).await?;
    issues.sort_by(|a, b| a.id.cmp(&b.id));

    // The whole edge table, in one query — including any edge whose *source* has
    // gone missing. That last part is not incidental: a loader that discovers
    // edges by walking the issues that exist can only ever find the half of a
    // corrupt graph that is still attached to something, and finding the other
    // half is what `check` is for.
    let edges = store.list_dependencies(&IssueFilter::new()).await?;

    let blocked = store
        .blocked_work(&IssueFilter::blocked())
        .await?
        .into_iter()
        .map(|i| i.id)
        .collect();

    Ok(Graph {
        issues,
        edges,
        blocked,
    })
}

/// Graphviz DOT, or the same thing as JSON.
///
/// DOT rather than a hand-drawn tree because the graph is a *graph*: it has
/// diamonds and it is not always acyclic, and `dot -Tsvg` renders both without
/// this file having to know how to lay out a plane. `bd dep tree` already covers
/// the "just show me what this one bead waits on" case.
async fn render(ctx: &Ctx) -> Result<()> {
    let g = load(ctx).await?;

    if ctx.out.is_json() {
        let nodes: Vec<_> = g
            .issues
            .iter()
            .map(|i| {
                json!({
                    "id": i.id,
                    "title": i.title,
                    "status": i.status,
                    "priority": i.priority,
                    "issue_type": i.issue_type,
                    // Not on `Issue` (it is derived state); supplied beside it.
                    "is_blocked": g.blocked.contains(&i.id),
                })
            })
            .collect();
        return ctx.out.json_value(&json!({ "nodes": nodes, "edges": g.edges }));
    }

    // Written with `println!`, not `out.line`: this is the command's output, and
    // `bd graph --quiet | dot -Tsvg` must still produce a picture.
    for l in dot(&g).lines() {
        println!("{l}");
    }
    Ok(())
}

fn dot(g: &Graph) -> String {
    let mut s = String::new();
    s.push_str("digraph beads {\n");
    // Left-to-right: a dependency chain reads as a chain, and long titles have
    // somewhere to go.
    s.push_str("  rankdir=LR;\n");
    s.push_str("  node [shape=box, style=rounded, fontname=\"sans-serif\", fontsize=10];\n");
    s.push_str("  edge [fontname=\"sans-serif\", fontsize=9];\n");

    for i in &g.issues {
        let label = format!("{}\\n{}", dot_escape(&i.id), dot_escape(&clip(&i.title, 40)));
        let style = if i.status == Status::Closed {
            // Closed nodes are still drawn — greyed, because they are the reason
            // the live half of the graph looks the way it does.
            "style=\"rounded,filled\", fillcolor=\"#f0f0f0\", color=\"#b0b0b0\", fontcolor=\"#808080\""
        } else if g.blocked.contains(&i.id) {
            "color=\"#c0392b\", fontcolor=\"#c0392b\""
        } else {
            "color=\"#27823b\""
        };
        s.push_str(&format!(
            "  \"{}\" [label=\"{label}\", {style}];\n",
            dot_escape(&i.id)
        ));
    }

    if !g.edges.is_empty() {
        s.push('\n');
    }
    for d in &g.edges {
        // `issue --[type]--> depends_on`, i.e. the arrow points at what the
        // issue is waiting for. Solid edges gate `bd ready`; dashed ones are
        // associations that never do (`DependencyType::affects_ready_work` is
        // the authority, not a list repeated here).
        let style = if d.dep_type.affects_ready_work() {
            "color=\"#333333\""
        } else {
            "style=dashed, color=\"#aaaaaa\", fontcolor=\"#aaaaaa\""
        };
        s.push_str(&format!(
            "  \"{}\" -> \"{}\" [label=\"{}\", {style}];\n",
            dot_escape(&d.issue_id),
            dot_escape(&d.depends_on_id),
            dot_escape(d.dep_type.as_str()),
        ));
    }

    s.push_str("}\n");
    s
}

/// DOT string literals take C-style escapes. An unescaped quote in an issue
/// title would not merely look wrong, it would produce a file `dot` refuses to
/// parse — and titles are user text.
fn dot_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' => out.push_str("\\n"),
            _ => out.push(c),
        }
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{keep}…")
}

/// `bd graph check` — is the graph *sound*?
///
/// Three failures, each of which the write path already refuses to create, and
/// each of which an import, a merge, or another beads implementation can still
/// land in the table:
///
/// * a **cycle** — "A before B before A" is a contradiction, `bd dep tree` would
///   not terminate on it, and the `is_blocked` fixpoint has to spend its
///   iteration budget defending itself against it;
/// * a **self-edge** — an issue that blocks itself is blocked forever;
/// * a **dangling edge** — an edge to a bead that does not exist. SQLite's
///   foreign keys make this unreachable *there*; a backend without them, or a
///   half-applied import, is exactly what this check is for.
///
/// Exits 1 when it finds any of them. A corrupt graph is a real failure, and a
/// check command that always exits 0 cannot be used in a hook.
async fn check(ctx: &Ctx) -> Result<()> {
    let store = ctx.store().await?;
    let g = load(ctx).await?;
    // The store's own cycle detector, not a second one written here: it knows
    // which edge types define an ordering (`blocks`, `parent-child`), and a copy
    // of that list in the CLI would drift out of agreement with the one the write
    // path actually enforces.
    let f = analyze(&g, store.find_cycles().await?);

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "ok": f.ok(),
            "nodes": g.issues.len(),
            "edges": g.edges.len(),
            "blocked": g.blocked.len(),
            "edge_types": f.by_type,
            "cycles": f.cycles,
            "self_edges": f.self_edges,
            "dangling_edges": f.dangling,
        }))?;
    } else {
        ctx.out.line(format!(
            "{} node(s), {} edge(s), {} blocked.",
            g.issues.len(),
            g.edges.len(),
            g.blocked.len()
        ));
        if !f.by_type.is_empty() {
            let types: Vec<String> = f.by_type.iter().map(|(t, n)| format!("{t} {n}")).collect();
            ctx.out.line(format!("  edges: {}", types.join(", ")));
        }

        if f.ok() {
            ctx.out
                .line("The graph is sound: no cycles, no self-edges, no dangling edges.");
        } else {
            if !f.cycles.is_empty() {
                ctx.out.line(format!("\n{} cycle(s):", f.cycles.len()));
                for c in &f.cycles {
                    ctx.out.line(format!("  {}", c.join(" -> ")));
                }
            }
            if !f.self_edges.is_empty() {
                ctx.out.line(format!("\n{} self-edge(s):", f.self_edges.len()));
                for d in &f.self_edges {
                    ctx.out
                        .line(format!("  {} depends on itself [{}]", d.issue_id, d.dep_type));
                }
            }
            if !f.dangling.is_empty() {
                ctx.out.line(format!("\n{} dangling edge(s):", f.dangling.len()));
                for d in &f.dangling {
                    ctx.out.line(format!(
                        "  {} -> {} [{}] (no such issue)",
                        d.issue_id, d.depends_on_id, d.dep_type
                    ));
                }
            }
        }
    }

    if f.ok() {
        Ok(())
    } else {
        // The findings *are* the output; `report` must not print an error on top
        // of them, hence a silent exit rather than a bail.
        Err(SilentExit(exit::FAILURE).into())
    }
}

struct Findings<'a> {
    cycles: Vec<Vec<String>>,
    self_edges: Vec<&'a Dependency>,
    dangling: Vec<&'a Dependency>,
    by_type: BTreeMap<&'a str, usize>,
}

impl Findings<'_> {
    fn ok(&self) -> bool {
        self.cycles.is_empty() && self.self_edges.is_empty() && self.dangling.is_empty()
    }
}

/// Pure, so it can be tested against graphs the write path would never let you
/// build — which is the only kind worth checking for.
fn analyze<'a>(g: &'a Graph, cycles: Vec<Vec<String>>) -> Findings<'a> {
    let ids: HashSet<&str> = g.issues.iter().map(|i| i.id.as_str()).collect();

    let mut by_type: BTreeMap<&str, usize> = BTreeMap::new();
    for d in &g.edges {
        *by_type.entry(d.dep_type.as_str()).or_default() += 1;
    }

    Findings {
        cycles,
        self_edges: g
            .edges
            .iter()
            .filter(|d| d.issue_id == d.depends_on_id)
            .collect(),
        // Both ends. `load` now reads the edge table itself rather than walking
        // out of the issues that exist, so an edge whose *source* has vanished is
        // finally visible here — and it is exactly as broken as one whose target
        // has.
        dangling: g
            .edges
            .iter()
            .filter(|d| {
                !ids.contains(d.issue_id.as_str()) || !ids.contains(d.depends_on_id.as_str())
            })
            .collect(),
        by_type,
    }
}

// ---------------------------------------------------------------------------
// Registered, not ported
// ---------------------------------------------------------------------------

/// Upstream's `flatten` promotes the children of a hierarchy (`bd-abc.1.2`) into
/// top-level beads. Every id in the subtree therefore *changes*, and an id is a
/// primary key with edges, comments, labels and events hanging off it.
///
/// The seam has no re-key operation, and the ones it does have cannot be
/// composed into one. `create_issue` + `delete_issue` is not a rename: the
/// delete cascades away every edge that pointed at the old id (the very edges
/// this command exists to preserve), and there is no `add_event`, so the audit
/// trail is lost too. Worse, the two calls are separate transactions — a crash
/// between them leaves the workspace with the bead gone and its replacement not
/// yet written.
///
/// So this stays exit 64. A command that half-rewrites a graph is strictly worse
/// than one that admits it cannot. See the report for the seam method it needs.
pub async fn flatten(ctx: &Ctx, _id: &str) -> Result<()> {
    stub("flatten", ctx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bd_core::DependencyType;

    fn issue(id: &str, title: &str) -> Issue {
        Issue::new(id, title)
    }

    fn graph() -> Graph {
        let mut closed = issue("bd-c", "Closed one");
        closed.status = Status::Closed;
        Graph {
            issues: vec![issue("bd-a", "A \"quoted\" title"), issue("bd-b", "B"), closed],
            edges: vec![
                Dependency::new("bd-a", "bd-b", DependencyType::Blocks).unwrap(),
                Dependency::new("bd-a", "bd-c", DependencyType::Related).unwrap(),
            ],
            blocked: ["bd-a".to_string()].into_iter().collect(),
        }
    }

    #[test]
    fn dot_quotes_are_escaped_not_emitted() {
        let out = dot(&graph());
        // The title's quotes must arrive escaped, or `dot` rejects the file.
        assert!(out.contains(r#"A \"quoted\" title"#), "{out}");
        // ...and the label's own delimiters must still be there, unescaped.
        assert!(out.contains(r#"[label="bd-a\n"#), "{out}");
    }

    #[test]
    fn gating_edges_are_solid_and_associations_are_dashed() {
        let out = dot(&graph());
        let blocks = out
            .lines()
            .find(|l| l.contains("-> \"bd-b\""))
            .expect("blocks edge");
        assert!(!blocks.contains("dashed"), "a blocks edge gates ready: {blocks}");
        let related = out
            .lines()
            .find(|l| l.contains("-> \"bd-c\""))
            .expect("related edge");
        assert!(
            related.contains("dashed"),
            "an association must not look like a gate: {related}"
        );
    }

    #[test]
    fn a_closed_node_is_drawn_greyed_rather_than_dropped() {
        let out = dot(&graph());
        let node = out
            .lines()
            .find(|l| l.starts_with("  \"bd-c\" ["))
            .expect("closed node");
        assert!(node.contains("filled"), "{node}");
    }

    // -- graph check ------------------------------------------------------
    //
    // Every graph below is one `add_dependency` refuses to create. That is the
    // point: `graph check` is for the graphs that arrive some *other* way — an
    // import, a merge, another beads implementation — so the only way to test it
    // is to build them here, past the write path that would have said no.

    /// A raw edge, bypassing `Dependency::new`'s self-edge check.
    fn raw_edge(from: &str, to: &str) -> Dependency {
        Dependency {
            issue_id: from.to_string(),
            depends_on_id: to.to_string(),
            dep_type: DependencyType::Blocks,
            created_at: chrono::Utc::now(),
            created_by: String::new(),
            metadata: String::new(),
            thread_id: String::new(),
        }
    }

    #[test]
    fn a_sound_graph_is_reported_sound() {
        let g = graph();
        let f = analyze(&g, Vec::new());
        assert!(f.ok());
        assert_eq!(f.by_type.get("blocks"), Some(&1));
        assert_eq!(f.by_type.get("related"), Some(&1));
    }

    #[test]
    fn a_self_edge_is_found() {
        let g = Graph {
            issues: vec![issue("bd-a", "A")],
            edges: vec![raw_edge("bd-a", "bd-a")],
            blocked: HashSet::new(),
        };
        let f = analyze(&g, Vec::new());
        assert!(!f.ok(), "an issue that blocks itself is blocked forever");
        assert_eq!(f.self_edges.len(), 1);
        // It is not *also* dangling: the target exists, it is just the same bead.
        assert!(f.dangling.is_empty());
    }

    #[test]
    fn an_edge_to_a_bead_that_does_not_exist_is_found() {
        let g = Graph {
            issues: vec![issue("bd-a", "A")],
            edges: vec![raw_edge("bd-a", "bd-gone")],
            blocked: HashSet::new(),
        };
        let f = analyze(&g, Vec::new());
        assert!(!f.ok());
        assert_eq!(f.dangling.len(), 1);
        assert_eq!(f.dangling[0].depends_on_id, "bd-gone");
    }

    /// An edge whose *source* has vanished is exactly as broken, and used to be
    /// invisible: the loader found edges by walking out of the issues that
    /// existed, so an edge from a bead that no longer exists was never in the
    /// graph to be checked. `list_dependencies` reads the edge table itself, and
    /// this is the finding that only becomes possible because of it.
    #[test]
    fn an_edge_from_a_bead_that_does_not_exist_is_found_too() {
        let g = Graph {
            issues: vec![issue("bd-a", "A")],
            edges: vec![raw_edge("bd-gone", "bd-a")],
            blocked: HashSet::new(),
        };
        let f = analyze(&g, Vec::new());
        assert!(!f.ok(), "an edge out of a bead that does not exist is dangling");
        assert_eq!(f.dangling.len(), 1);
        assert_eq!(f.dangling[0].issue_id, "bd-gone");
    }

    /// A cycle is the store's finding, not ours — but it still has to sink the
    /// verdict, or `bd graph check` would exit 0 on a graph `bd dep tree` cannot
    /// even walk.
    #[test]
    fn a_cycle_reported_by_the_store_sinks_the_check() {
        let g = graph();
        let f = analyze(
            &g,
            vec![vec!["bd-a".into(), "bd-b".into(), "bd-a".into()]],
        );
        assert!(!f.ok());
    }
}
