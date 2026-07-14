//! The dependency graph: what gates what.

use std::collections::HashSet;

use anyhow::Result;
use bd_core::Dependency;
use serde_json::json;

use crate::cli::DepCmd;
use crate::context::Ctx;

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
        DepCmd::Remove { issue, depends_on } => {
            ctx.ensure_writable("remove a dependency")?;
            let store = ctx.store().await?;
            store.remove_dependency(&issue, &depends_on).await?;
            if ctx.out.is_json() {
                ctx.out
                    .json_value(&json!({ "issue_id": issue, "depends_on_id": depends_on, "removed": true }))?;
            } else {
                ctx.out
                    .line(format!("{issue} no longer depends on {depends_on}"));
            }
            Ok(())
        }
        DepCmd::List { id } => list(ctx, &id).await,
        DepCmd::Tree { id, depth } => tree(ctx, &id, depth).await,
        DepCmd::Cycles => cycles(ctx).await,
        DepCmd::Relate { .. } => crate::commands::stub("dep relate", ctx),
        DepCmd::Unrelate { .. } => crate::commands::stub("dep unrelate", ctx),
    }
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
