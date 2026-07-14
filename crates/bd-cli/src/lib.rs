//! `bd` — the beads command line.
//!
//! Three jobs, in order: parse, build a [`Ctx`](context::Ctx), dispatch. The
//! interesting part is the exit code — see [`exit`], which explains why "not
//! ported yet" (64) must not look like "it failed" (1).
//!
//! This is a library so that the tests can inspect the command tree directly;
//! the binary is [`run`] and nothing else.

pub mod cli;
pub mod commands;
pub mod context;
pub mod doctor;
pub mod exit;
pub mod integrations;
pub mod output;
pub mod parse;

use clap::FromArgMatches;

use crate::cli::Cli;
use crate::context::Ctx;
use crate::exit::SilentExit;

/// The whole program. Returns the process exit code rather than exiting, so
/// that nothing here has to know it is the end of the world.
pub fn run() -> i32 {
    let matches = match cli::build().try_get_matches() {
        Ok(m) => m,
        Err(e) => {
            let _ = e.print();
            // `--help` and `--version` arrive here too, and they are a success.
            return if e.use_stderr() { exit::USAGE } else { exit::OK };
        }
    };
    let cli = match Cli::from_arg_matches(&matches) {
        Ok(c) => c,
        Err(e) => {
            let _ = e.print();
            return exit::USAGE;
        }
    };

    init_tracing(&cli);

    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start the async runtime: {e}");
            return exit::FAILURE;
        }
    };

    match rt.block_on(execute(cli)) {
        Ok(()) => exit::OK,
        Err((e, json)) => report(&e, json),
    }
}

/// Returns the error *and* whether to render it as JSON — `cli` is consumed by
/// dispatch, so the flag has to come back out with the failure.
async fn execute(cli: Cli) -> Result<(), (anyhow::Error, bool)> {
    let json = cli.json();
    let need = cli.command.need();

    let ctx = match Ctx::build(&cli, need).await {
        Ok(c) => c,
        Err(e) => return Err((e, json)),
    };

    let result = commands::dispatch(cli.command, &ctx).await;
    ctx.close().await;
    result.map_err(|e| (e, json))
}

fn report(err: &anyhow::Error, json: bool) -> i32 {
    // Already printed by whoever raised it: stubs and capability gaps format
    // themselves, because their shape differs under --json.
    if let Some(SilentExit(code)) = err.downcast_ref::<SilentExit>() {
        return *code;
    }

    // A backend saying "I cannot do that" is a contract, not a crash.
    let code = match err.downcast_ref::<bd_storage::Error>() {
        Some(e) if e.is_unsupported() => exit::CAPABILITY,
        _ => exit::FAILURE,
    };

    if json {
        let doc = serde_json::json!({
            "error": if code == exit::CAPABILITY { "unsupported" } else { "failure" },
            // `{:#}` flattens anyhow's chain onto one line: agents parse this
            // string, and a multi-line message is a worse string.
            "message": format!("{err:#}"),
        });
        println!("{doc}");
    } else {
        eprintln!("error: {err:#}");
    }
    code
}

fn init_tracing(cli: &Cli) {
    let level = if cli.quiet {
        "error"
    } else {
        match cli.verbose {
            0 => "warn",
            1 => "info",
            2 => "debug",
            _ => "trace",
        }
    };
    // Logs are diagnostics: stderr only. stdout may be JSON on its way to jq.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_writer(std::io::stderr)
        .with_ansi(!cli.no_color && std::env::var_os("NO_COLOR").is_none())
        .without_time()
        .try_init();
}
