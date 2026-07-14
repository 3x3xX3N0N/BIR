//! Creating a workspace, and the commands that run before one exists.

use anyhow::{Result, bail};
use bd_storage::{Backend, Identity, Locator};
use clap_complete::Shell;
use serde_json::json;

use crate::cli::{ConfigCmd, InitArgs};
use crate::context::{Config, Ctx};

/// The other place a concrete backend may be named (see [`crate::context`]).
///
/// It is legitimate *here* and nowhere else: at `init` there is nothing on disk
/// to contradict, so the flag decides. Afterwards the locator does, forever.
pub async fn init(ctx: &Ctx, a: InitArgs) -> Result<()> {
    ctx.ensure_writable("initialize a workspace")?;

    let root = match &a.path {
        Some(p) => {
            std::fs::create_dir_all(p)?;
            std::fs::canonicalize(p)?
        }
        None => ctx.cwd.clone(),
    };
    let beads_dir = root.join(bd_storage::locator::BEADS_DIR);

    let existing = beads_dir.join(bd_storage::locator::LOCATOR_FILE).exists();
    if existing && !a.force {
        bail!(
            "a beads workspace already exists at {} (use --force to re-initialize)",
            beads_dir.display()
        );
    }
    if existing {
        // --force rewrites the locator over a database that may hold work. Say
        // so out loud; the only thing worse than losing a workspace is losing it
        // quietly.
        ctx.out.warn(format!(
            "re-initializing over the existing workspace at {}",
            beads_dir.display()
        ));
    }

    if a.backend != Backend::Sqlite {
        // Not a capability gap — a backend this port has not built. Exit 64, so
        // a script can tell "come back later" from "never".
        return crate::commands::stub(&format!("init --backend={}", a.backend), ctx);
    }

    let prefix = a.prefix.clone().unwrap_or_else(|| derive_prefix(&root));
    let identity = Identity {
        actor: ctx.identity.actor.clone(),
        session: ctx.identity.session.clone(),
    };

    // `init` takes the project root and creates `.beads/` under it — including
    // the locator, whose workspace_id it preserves across a re-init. Writing our
    // own locator afterwards would rotate that id and fork the workspace from
    // itself, so we read the one it wrote instead.
    let store = bd_sqlite::init(&root, &prefix, identity).await?;
    store.close().await?;
    let locator = Locator::load(&beads_dir)?;

    let config = Config {
        prefix: Some(prefix.clone()),
        ..Default::default()
    };
    config.save(&beads_dir)?;

    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "workspace": beads_dir,
            "backend": a.backend.as_str(),
            "prefix": prefix,
            "workspace_id": locator.workspace_id,
        }))?;
    } else {
        ctx.out.line(format!(
            "Initialized a {} workspace at {}",
            a.backend,
            beads_dir.display()
        ));
        ctx.out.line(format!("Issue ids will look like {prefix}-a3f2"));
    }
    Ok(())
}

/// `my-project` -> `my-project`; `My Project!` -> `myproject`. Falls back to
/// `bd` rather than to an empty prefix, which would mint ids like `-a3f2`.
fn derive_prefix(root: &std::path::Path) -> String {
    let name: String = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("bd")
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(12)
        .collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() { "bd".to_string() } else { name }
}

pub fn version(ctx: &Ctx) -> Result<()> {
    let v = env!("CARGO_PKG_VERSION");
    if ctx.out.is_json() {
        ctx.out.json_value(&json!({
            "version": v,
            "implementation": "rust",
            "backend": ctx.backend().map(|b| b.as_str()),
        }))?;
    } else {
        println!("bd {v} (rust)");
    }
    Ok(())
}

pub fn completion(shell: Shell) -> Result<()> {
    let mut cmd = crate::cli::build();
    clap_complete::generate(shell, &mut cmd, "bd", &mut std::io::stdout());
    Ok(())
}

pub async fn config(ctx: &Ctx, cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Set { key, value } => {
            ctx.ensure_writable("set a config key")?;
            let store = ctx.store()?;
            store.set_config(&key, &value).await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({ "key": key, "value": value }))?;
            } else {
                ctx.out.line(format!("{key} = {value}"));
            }
            Ok(())
        }
        ConfigCmd::Get { key } => {
            let store = ctx.store()?;
            let v = store.get_config(&key).await?;
            if ctx.out.is_json() {
                ctx.out.json_value(&json!({ "key": key, "value": v }))?;
            } else {
                match v {
                    Some(v) => println!("{v}"),
                    None => bail!("no such config key: {key}"),
                }
            }
            Ok(())
        }
        ConfigCmd::List => {
            let store = ctx.store()?;
            let entries = store.list_config().await?;
            if ctx.out.is_json() {
                let map: serde_json::Map<String, serde_json::Value> = entries
                    .into_iter()
                    .map(|(k, v)| (k, serde_json::Value::String(v)))
                    .collect();
                ctx.out.json_value(&map)?;
            } else if entries.is_empty() {
                ctx.out.line("No configuration set.");
            } else {
                for (k, v) in entries {
                    println!("{k} = {v}");
                }
            }
            Ok(())
        }
        ConfigCmd::Unset { .. } => crate::commands::stub("config unset", ctx),
        ConfigCmd::Validate => crate::commands::stub("config validate", ctx),
        ConfigCmd::Show => crate::commands::stub("config show", ctx),
    }
}
