//! magebuild — a parallel, DAG-ordered Magento build orchestrator.
//!
//! `composer-install → { di-compile → autoload-dump } ∥ { static-deploy } →
//! package`, run with a bounded parallel ready-queue over a shared rayon pool,
//! with each step a linked in-process engine call where one exists.

// The release binaries are static musl builds, and musl's allocator is slow
// under threads: on the demo host it cost a deploy's agent about 0.3s across
// manifest decoding, reconciling a staged release and recording the new one.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod config;
mod deploy;
mod graph;
mod json;
mod preflight;
mod render;
mod report;
mod scheduler;
mod steps;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::config::{BuildOptions, FileConfig};
use crate::scheduler::{Observer, Runner, SilentObserver};
use crate::steps::Ctx;

/// Build a Magento release as fast as possible.
#[derive(Debug, Parser)]
#[command(name = "magebuild", version, about, long_about = None)]
struct Cli {
    /// Magento root (default: current directory).
    #[arg(long, global = true)]
    root: Option<PathBuf>,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    /// Also write the run to this JSON file: each node's state, duration and
    /// diagnostics (kind, severity, file, line, area/theme/locale, message), for
    /// CI to gate on and annotate with. Written even when the build fails before
    /// a node starts.
    #[arg(long, global = true, value_name = "FILE")]
    report: Option<PathBuf>,

    /// Path to magebuild.toml (default: <root>/magebuild.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Max concurrent nodes (default: CPU count).
    #[arg(long, global = true)]
    jobs: Option<usize>,

    /// Also package the build into this archive (.tar / .tar.gz).
    #[arg(long, global = true)]
    artifact: Option<PathBuf>,

    /// Package excludes file (one pattern per line).
    #[arg(long, global = true)]
    exclude_from: Option<PathBuf>,

    /// Static-deploy content version — written to pub/static/deployed_version.txt
    /// (asset-URL cache-busting). Omit to write no file.
    #[arg(long, global = true)]
    deployed_version: Option<String>,

    /// Run only these node ids (plus their transitive deps).
    #[arg(long, value_delimiter = ',', global = true)]
    only: Vec<String>,

    /// Skip these node ids (they still satisfy dependents).
    #[arg(long, value_delimiter = ',', global = true)]
    skip: Vec<String>,

    /// Render the resolved DAG + parallel schedule; run nothing.
    #[arg(long, global = true)]
    dry_run: bool,

    /// Dump per-node timings after the run.
    #[arg(long, global = true)]
    profile: bool,

    /// Apply a named build preset over the defaults (still under magebuild.toml).
    /// `hyva` = Tailwind `npm run build` per Hyvä theme, then a static deploy
    /// with --no-parent --no-less --no-js-bundle --no-html-minify --symlink=file.
    #[arg(long, value_enum, global = true)]
    preset: Option<crate::config::Preset>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Render the resolved DAG + parallel schedule (= --dry-run).
    Plan,
    /// Run a single node and its transitive dependencies.
    Node {
        /// The node id to build.
        id: String,
    },
    /// Ship a built artifact to the `[deploy]` host: one ssh session, a small
    /// agent on the host (uploaded only when that build is not there yet)
    /// doing the release, shared links, upgrade check, swap and cleanup.
    Deploy {
        /// The artifact to deploy (.tar.gz, .tar.zst or .tar).
        #[arg(long = "from")]
        from: PathBuf,
        /// The agent binary for the host, a static Linux build (default: this
        /// executable).
        #[arg(long)]
        agent: Option<PathBuf>,
        /// Override `[deploy] host`.
        #[arg(long)]
        host: Option<String>,
        /// The ssh command, split on whitespace.
        #[arg(long, default_value = "ssh")]
        ssh: String,
    },
    /// The host side of `deploy`, started by it over ssh. Speaks a JSON-lines
    /// protocol on stdin/stdout.
    #[command(hide = true)]
    Agent,
    /// Deletes the releases a deploy set aside, after the agent has exited,
    /// and with `--stage` prepares a copy of the live release for the next.
    #[command(hide = true)]
    AgentCleanup {
        releases: PathBuf,
        #[arg(long)]
        stage: bool,
        /// Stage by copying, not hard-linking (`[deploy] reuse = "copy"`).
        #[arg(long)]
        copy: bool,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let report = cli.report.clone();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            if let Some(path) = &report {
                write_report(path, &report::of_error(&format!("{e:#}")));
            }
            ExitCode::FAILURE
        }
    }
}

/// Write `--report`; a failure to is reported, not fatal: the build's own
/// outcome is what the exit code says.
fn write_report(path: &std::path::Path, json: &json::Json) {
    if let Err(e) = std::fs::write(path, json.to_pretty()) {
        eprintln!("error: writing the report to {}: {e}", path.display());
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    let root = cli.root.clone().unwrap_or_else(|| PathBuf::from("."));

    // Not a build: no magebuild.toml, no graph.
    if matches!(cli.command, Some(Command::Agent)) {
        return Ok(deploy::agent::run());
    }
    if let Some(Command::AgentCleanup {
        releases,
        stage,
        copy,
    }) = &cli.command
    {
        // Staging first: it is what the next deploy waits on.
        if *stage && let Some(root) = releases.parent() {
            let how = if *copy {
                deploy::config::Reuse::Copy
            } else {
                deploy::config::Reuse::Hardlink
            };
            if let Err(e) = deploy::agent::stage_next(root, how) {
                eprintln!("agent-cleanup: staging the next release failed: {e:#}");
            }
        }
        deploy::agent::delete_trash(releases);
        return Ok(ExitCode::SUCCESS);
    }

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| root.join("magebuild.toml"));
    let file = FileConfig::load(&config_path)?;

    if let Some(Command::Deploy {
        from,
        agent,
        host,
        ssh,
    }) = &cli.command
    {
        let section = file
            .deploy
            .clone()
            .with_context(|| format!("{} has no [deploy] section", config_path.display()))?;
        return deploy::runner::run(
            section,
            deploy::runner::Options {
                artifact: from.clone(),
                agent: agent.clone(),
                host: host.clone(),
                ssh: ssh.clone(),
            },
        );
    }

    let opts = BuildOptions {
        artifact: cli.artifact.clone(),
        exclude_from: cli.exclude_from.clone(),
        jobs: cli.jobs,
        deployed_version: cli.deployed_version.clone(),
        preset: cli.preset,
    };
    let (mut graph, jobs) = config::resolve(&file, &opts, &root)?;

    // `--only` / `node <id>` restrict; `--skip` marks Never.
    let target = match &cli.command {
        Some(Command::Node { id }) => Some(vec![id.clone()]),
        _ if !cli.only.is_empty() => Some(cli.only.clone()),
        _ => None,
    };
    if let Some(targets) = &target {
        graph
            .restrict_to(targets)
            .context("resolving --only / node target")?;
    }
    if !cli.skip.is_empty() {
        graph.skip(&cli.skip).context("applying --skip")?;
    }
    graph.validate().context("invalid build graph")?;

    // Plan / dry-run: render and exit without running.
    let dry = cli.dry_run || matches!(cli.command, Some(Command::Plan));
    if dry {
        if cli.json {
            print!("{}", render::plan_json(&graph, jobs).to_pretty());
        } else {
            print!("{}", render::plan_text(&graph, jobs));
        }
        return Ok(ExitCode::SUCCESS);
    }

    // Real build: pre-flight (fail fast on a bad root; warn on contract gaps).
    // An absent vendor/ is only worth flagging when nothing in this run installs
    // it — a full build's composer-install creates it.
    let installs_vendor = graph.nodes().iter().any(|n| {
        !n.is_skipped()
            && matches!(
                n.kind,
                crate::graph::NodeKind::Native(crate::graph::BuiltinStep::ComposerInstall { .. })
            )
    });
    let warnings = preflight::check(&root, installs_vendor)?;
    for w in &warnings {
        eprintln!("warning: {w}");
    }

    let ctx = Arc::new(Ctx {
        root: std::path::absolute(&root).unwrap_or(root),
        jobs,
        diagnostics: report::Diagnostics::default(),
    });
    let runner: Arc<Runner> = {
        let ctx = ctx.clone();
        Arc::new(move |node| steps::execute(node, &ctx))
    };
    let observer: Arc<dyn Observer> = if cli.json {
        Arc::new(SilentObserver)
    } else {
        Arc::new(render::ProgressObserver::new(cli.json))
    };

    let report = scheduler::run(&graph, jobs, runner, observer);
    if let Some(path) = &cli.report {
        write_report(path, &report::of_run(&report, &ctx.diagnostics));
    }

    if cli.json {
        print!("{}", render::summary_json(&report).to_pretty());
    } else {
        eprint!("{}", render::summary_text(&report, cli.profile));
    }

    Ok(if report.failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}
