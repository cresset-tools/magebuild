//! magebuild — a parallel, DAG-ordered Magento build orchestrator.
//!
//! `composer-install → { di-compile → autoload-dump } ∥ { static-deploy } →
//! package`, run with a bounded parallel ready-queue over a shared rayon pool,
//! with each step a linked in-process engine call where one exists.

mod config;
mod graph;
mod json;
mod preflight;
mod render;
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
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let cli = Cli::parse();
    let root = cli.root.clone().unwrap_or_else(|| PathBuf::from("."));

    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(|| root.join("magebuild.toml"));
    let file = FileConfig::load(&config_path)?;

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
