//! the `mbm` binary.
//!
//! everything the program does is a function in `mbm-app`; this file only
//! wires the arguments to those functions and turns an error into an exit code.
//! that split is what makes every command testable without a terminal.

use std::process::ExitCode;
use std::sync::{Arc, Mutex};

use clap::Parser;
use mbm_app::Config;
use mbm_app::cli::{self, Cli, Command, Output};
use mbm_core::error::{Error, Result};
use mbm_core::port::EnrichStage;
use mbm_store::Repo;
use tracing_subscriber::EnvFilter;

fn main() -> ExitCode {
    let cli = Cli::parse();

    // the filter goes in before anything else, because a log line written
    // before the subscriber exists goes nowhere and a person debugging a run
    // needs them.
    //
    // `-vv` turns on debug for *this program's* crates and nothing else. a bare
    // `debug` turns it on for the http stack as well, and what a person then
    // sees first is three lines about certificate roots and one line per
    // connection attempt: on a run against a real source that is a screenful of
    // a program's own plumbing between the line they asked for and the result.
    // the transport is not what `-vv` is for. `MBM_LOG` takes the whole filter
    // when it is.
    let filter = if cli.quiet {
        "error".to_owned()
    } else {
        match cli.verbose {
            0 => "warn,mbm_app=info".to_owned(),
            1 => "info".to_owned(),
            _ => [
                "info",
                "mbm_agent=debug",
                "mbm_core=debug",
                "mbm_enrich=debug",
                "mbm_extract=debug",
                "mbm_ingest=debug",
                "mbm_jev=debug",
                "mbm_sink=debug",
                "mbm_store=debug",
            ]
            .join(","),
        }
    };
    let filter = std::env::var("MBM_LOG").unwrap_or(filter);
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_target(false)
        .without_time()
        .try_init();

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(dispatch(&cli)) {
        Ok(output) => {
            if !cli.quiet {
                for line in &output.lines {
                    println!("{line}");
                }
            }
            if output.failed { ExitCode::FAILURE } else { ExitCode::SUCCESS }
        }
        Err(e) => {
            eprintln!("{e}");
            // the class is what a script branches on, so it is in the message
            tracing::debug!(class = ?e.class(), "the command failed");
            ExitCode::from(exit_code(&e))
        }
    }
}

/// run the command the arguments named.
async fn dispatch(cli: &Cli) -> Result<Output> {
    let config = cli.config()?;

    // the commands that touch nothing else never need the store
    // the config subcommand has to resolve the path the same way every other
    // command does, or `--config` silently does nothing for it
    if let Command::Config(args) = &cli.command {
        return cli::config_command(args, &cli::config_path(cli));
    }

    let conn = mbm_app::pipeline::open(&config)?;

    match &cli.command {
        Command::Add(args) => cli::add(&conn, args).await,
        Command::Import(args) => cli::import(&conn, args),
        Command::Run(args) => cli::run(&conn, &config, args).await,
        Command::Search(args) => cli::search(&conn, args),
        Command::Show(args) => cli::show(&conn, args),
        Command::List(args) => cli::list(&conn, args),
        Command::Tag(args) => cli::tag(&conn, args),
        Command::Delete(args) => cli::delete(&conn, args),
        Command::Stats => Ok(stats(&conn)),
        Command::Export(args) => cli::export(&conn, &config, args).await,
        Command::Enrich(args) => cli::enrich(&conn, &config, args).await,
        Command::Rebuild(args) => cli::rebuild(&conn, &config, args).await,
        Command::Tui(args) => {
            let conn = Arc::new(Mutex::new(conn));
            mbm_app::tui::run(conn, &args.query)?;
            Ok(Output::empty())
        }
        Command::Config(_) => unreachable!("handled above"),
    }
}

/// the counts a person asks `mbm stats` for.
fn stats(conn: &rusqlite::Connection) -> Output {
    let repo = Repo::new(conn);
    let mut out = Output::empty();

    let total = repo.count().unwrap_or(0);
    out = out.with(format!("{total} bookmarks"));

    for stage in EnrichStage::ALL {
        let waiting = repo.pending(mbm_enrich::column_for(*stage)).unwrap_or(0);
        out = out.with(format!("  {stage:<12} {waiting} waiting"));
    }

    if let Ok(mut stmt) =
        conn.prepare("SELECT medium, count(*) FROM bookmark GROUP BY medium ORDER BY 2 DESC, 1")
    {
        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map(|found| found.flatten().collect())
            .unwrap_or_default();
        for (medium, count) in rows {
            out = out.with(format!("  {medium:<12} {count}"));
        }
    }

    if let Ok(tags) = repo.tags_with_counts(20)
        && !tags.is_empty()
    {
        out = out.with("top tags:");
        for (tag, count) in tags {
            out = out.with(format!("  {tag:<24} {count}"));
        }
    }
    out
}

/// the exit code a failure gets.
///
/// distinct codes for the classes a script would want to branch on, so a cron
/// job can tell a rate limit from a missing credential without parsing text.
fn exit_code(error: &Error) -> u8 {
    use mbm_core::error::Class;
    match error.class() {
        // worth trying again later
        Class::Transient | Class::RateLimited => 3,
        // a credential or a configuration problem, which retrying will not fix
        Class::Auth | Class::NotFound | Class::Permanent => 2,
    }
}

/// the config a caller would use, for the library's own tests.
#[must_use]
pub fn default_config() -> Config {
    Config::default()
}
