//! Node execution: the in-process engine calls behind each [`BuiltinStep`],
//! plus subprocess `Command` nodes.
//!
//! All five built-ins are linked (in-process) engine calls:
//!
//! - `ComposerInstall` → `composer-install`
//! - `DiCompile`       → `magecommand-engine`
//! - `StaticDeploy`    → `magecommand` (`static_deploy::deploy::deploy_to_disk`)
//! - `AutoloadDump`    → `composer-autoload`
//! - `Package`         → native `tar` + `flate2`, plus the deploy manifest
//!
//! `StaticDeploy` still honors an explicit `command` override by shelling out
//! (the escape hatch for a bespoke deploy invocation).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::deploy::manifest;
use crate::graph::{BuiltinStep, Node, NodeKind, Symlink};
use crate::report::{Diagnostic, Diagnostics, Severity};

/// Shared execution context: the project root every step operates on, and the
/// job budget (used by the packaging step to size its parallel-gzip pool).
pub struct Ctx {
    pub root: PathBuf,
    pub jobs: usize,
    /// What each node found, for `--report`.
    pub diagnostics: Diagnostics,
}

/// Run one node to completion. `Err` = the node failed.
pub fn execute(node: &Node, ctx: &Ctx) -> Result<()> {
    let diag = |d: Diagnostic| ctx.diagnostics.push(&node.id, d);
    match &node.kind {
        NodeKind::Native(step) => run_builtin(step, ctx, &diag),
        NodeKind::Command { run, cwd, env } => run_command(run, cwd.as_deref(), env, &ctx.root),
    }
}

fn run_builtin(step: &BuiltinStep, ctx: &Ctx, diag: &dyn Fn(Diagnostic)) -> Result<()> {
    match step {
        BuiltinStep::ComposerInstall {
            no_dev,
            cache_root,
            hardlink,
        } => composer_install(&ctx.root, *no_dev, cache_root.as_deref(), *hardlink),
        BuiltinStep::DiCompile {
            fused,
            target,
            gate,
        } => di_compile(&ctx.root, *fused, target, gate, diag),
        BuiltinStep::StaticDeploy {
            themes,
            locales,
            areas,
            no_parent,
            no_less,
            no_js_bundle,
            no_html_minify,
            symlink,
            deployed_version,
            command,
            fail_on,
        } => static_deploy(
            &ctx.root,
            themes,
            locales,
            areas,
            *no_parent,
            *no_less,
            *no_js_bundle,
            *no_html_minify,
            *symlink,
            deployed_version.as_deref(),
            command.as_deref(),
            fail_on,
            diag,
        ),
        BuiltinStep::AutoloadDump { no_dev, optimize } => {
            autoload_dump(&ctx.root, *no_dev, *optimize)
        }
        BuiltinStep::Package {
            output,
            exclude_from,
        } => package(&ctx.root, output, exclude_from.as_deref(), ctx.jobs),
    }
}

/// Where composer dist archives are cached when a node sets no explicit
/// `cache_root`. A PERSISTENT, user-global location so repeated builds (and CI
/// with a warmed cache) reuse downloads instead of re-fetching every package —
/// a project-local `var/cache` is excluded from the artifact and cold every run.
/// `MAGEBUILD_CACHE_DIR` overrides it exactly: point it at a cache your CI
/// already warms (e.g. share the one setup-bougie keys on composer.lock) to
/// reuse those downloads with no cold first run.
fn composer_cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MAGEBUILD_CACHE_DIR").filter(|s| !s.is_empty()) {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|s| !s.is_empty())
                .map(|h| PathBuf::from(h).join(".cache"))
        })
        .unwrap_or_else(|| PathBuf::from(".magebuild-cache"));
    base.join("magebuild").join("composer-dist")
}

/// `composer install` from `composer.lock`, in-process.
fn composer_install(
    root: &Path,
    no_dev: bool,
    cache_root: Option<&Path>,
    hardlink: bool,
) -> Result<()> {
    let cache = cache_root
        .map(Path::to_path_buf)
        .unwrap_or_else(composer_cache_dir);
    std::fs::create_dir_all(&cache)
        .with_context(|| format!("creating composer dist cache {}", cache.display()))?;

    // Hard-link packages out of a decompress-once store instead of extracting
    // each install. OFF by default: it only wins with a PERSISTENT, uncompressed
    // store (self-hosted CI, a docker layer, repeated local builds). With an
    // ephemeral `actions/cache` the compressed store re-decompresses on restore,
    // so there is no gain over a plain extract. Opt in via magebuild.toml
    // (`[nodes.composer-install] hardlink = true`) or the MAGEBUILD_HARDLINK env.
    let link_mode = if hardlink || std::env::var_os("MAGEBUILD_HARDLINK").is_some() {
        composer_install::LinkMode::Hardlink
    } else {
        composer_install::LinkMode::Extract
    };

    let fetcher = composer_install::ReqwestFetcher::new()
        .map_err(|e| anyhow::anyhow!("building HTTP fetcher: {e:#}"))?;
    // Packages the lock pins to a git source (no dist) clone through the
    // system git, with bare mirrors cached beside the dists.
    let git = composer_install::GitSourceFetcher::new(cache.join("vcs"));
    let env = composer_install::InstallEnv::new(&fetcher, &composer_install::NoProgress, &cache)
        .with_source_fetcher(&git);
    // Build the root patch plan and hand it to the installer. The in-process
    // installer skips composer plugins, so without this a patch-dependent project
    // (e.g. Hyvä checkout modules whose CSS a `Tailwind-4.patch` makes
    // v4-compatible) would get an UNPATCHED vendor tree here — diverging from
    // production and breaking downstream steps (the Hyvä Tailwind build,
    // di-compile). The installer applies the plan idempotently: it re-extracts
    // any package whose patch fingerprint changed to a pristine state before
    // patching, and records the applied set in `patches.lock.json`, so a second
    // run is a no-op rather than a double-apply.
    let plan = build_patch_plan(root, &cache)?;

    let summary = composer_install::install_from_lock_with_patches(
        &env,
        root,
        composer_install::InstallOptions { no_dev, link_mode },
        None,
        plan.as_ref(),
    )
    .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    for w in &summary.warnings {
        eprintln!("  warning (composer): {w}");
    }
    if let Some(plan) = &plan {
        let n = plan.patches.values().map(Vec::len).sum::<usize>() + plan.root_patches.len();
        eprintln!("  composer-install: patch plan = {n} patch(es)");
    }

    Ok(())
}

/// The root project's patch plan, built by composer-patches exactly as
/// `bougie composer install` builds it (`extra.patches`, `patches-file` and
/// the patches directory; see [`composer_patches::build_plan`]), with
/// magebuild's own policy on top: a patch that fails to apply fails the build,
/// since a production build must never ship an unpatched tree, and
/// `patches.lock.json` records what was applied in readable form. Remote
/// patches are cached next to the dist cache. `None` when there is nothing to
/// apply and nothing to undo.
fn build_patch_plan(root: &Path, dist_cache: &Path) -> Result<Option<composer_patches::PatchPlan>> {
    let opts = composer_patches::PlanOptions {
        exit_on_failure: Some(true),
        write_lock: true,
        cache_dir: Some(dist_cache.parent().unwrap_or(dist_cache).join("patches")),
        ..composer_patches::PlanOptions::default()
    };
    let fetcher = composer_install::ReqwestFetcher::new()
        .map_err(|e| anyhow::anyhow!("building HTTP fetcher: {e:#}"))?;
    composer_patches::build_plan(root, &opts, &fetcher)
        .map_err(|e| anyhow::anyhow!("building the patch plan: {e:#}"))
}

/// `setup:di:compile`, in-process — the exact sequence the `magecommand` CLI
/// runs (`lib.rs::compile`), including the from-empty bring-up nuance: the class
/// universe scans the frozen `generated/_code` archive when present, else the
/// live `generated/code` (which we clear first).
fn di_compile(
    root: &Path,
    fused: bool,
    target: &crate::graph::DiTarget,
    gate: &crate::graph::DiGate,
    diag: &dyn Fn(Diagnostic),
) -> Result<()> {
    // Magento's `BP` must be absolute (it is baked into generated regexes).
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let target = target.as_target();
    // Checked here, not when the config loads: the default comes from the
    // checkout's `setup/`, which composer-install has only now put in place.
    if magecommand_engine::arealayout::AreaLayout::resolve(&root, &target).is_none() {
        anyhow::bail!(
            "di-compile: area_config = \"diff\" needs a Mage-OS 4.0+ runtime, which reads area \
files written as differences from global; this one would drop global's configuration. \
Set target_platform = \"mageos-4.0\" if the store runs 4.0, or drop area_config."
        );
    }

    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;

    // Doctor first, on the store just loaded: what it fails, nothing is
    // generated for.
    if let Some(policy) = &gate.doctor {
        doctor(&magento, policy, diag)?;
    }

    // Clear the output tree first so stale artifacts don't leak into the scan
    // universe / class resolver (what `setup:di:compile` does).
    magecommand_engine::metadata::clear_generated_dir(&root, "code")
        .context("clearing generated/code")?;
    magecommand_engine::metadata::clear_generated_dir(&root, "metadata")
        .context("clearing generated/metadata")?;

    let generated_code = if root.join("generated/_code").is_dir() {
        root.join("generated/_code")
    } else {
        root.join("generated/code")
    };
    let mut defs =
        magecommand_engine::definitions::Definitions::scan(&magento, &root, &generated_code);
    let out =
        magecommand_engine::build::compute_outputs_with(&magento, &mut defs, &root, fused, &target);
    // What setup:di:compile refuses fails the build, whatever the gate says.
    if !out.rejections.is_empty() {
        for r in &out.rejections {
            eprintln!("  di-compile: error: {r}");
            let mut d = Diagnostic::new("rejected", Severity::Error, r.message());
            d.code = Some(r.kind.name().to_owned());
            d.file = Some(r.file.clone());
            d.line = r.line;
            d.areas = r.areas.clone();
            diag(d);
        }
        anyhow::bail!(
            "di-compile: setup:di:compile refuses this configuration too ({} error(s)); nothing was written",
            out.rejections.len()
        );
    }
    let written = magecommand_engine::metadata::write_generated(&root, &out.files)
        .context("writing generated/")?;
    eprintln!("  di-compile: wrote {written} generated/ file(s)");
    use magecommand_engine::build::FailOn;
    let lost_gated = gate.fail_on.contains(&FailOn::LostArguments);
    for (name, why) in &out.lost_arguments {
        let mut d = Diagnostic::new("lost-arguments", Severity::warning_unless(lost_gated), why);
        d.subject = Some(name.clone());
        diag(d);
    }
    for finding in &out.findings {
        diag(Diagnostic::new("compile-finding", Severity::Info, finding));
    }
    if !out.lost_arguments.is_empty() {
        eprintln!(
            "  di-compile: warning: {} type(s) lose their di.xml arguments (their constructor \
             could not be resolved statically, so Magento builds them by runtime reflection, \
             which ignores di.xml):",
            out.lost_arguments.len()
        );
        for (name, why) in &out.lost_arguments {
            eprintln!("    {name}  ({why})");
        }
    }
    if !out.findings.is_empty() {
        eprintln!(
            "  di-compile: note: {} compile finding(s), first: {}",
            out.findings.len(),
            out.findings[0]
        );
    }
    let failed: Vec<&str> = gate
        .fail_on
        .iter()
        .filter(|kind| match kind {
            FailOn::LostArguments => !out.lost_arguments.is_empty(),
        })
        .map(|kind| kind.name())
        .collect();
    if !failed.is_empty() {
        anyhow::bail!("di-compile: fail_on = {failed:?}");
    }
    Ok(())
}

/// `magequery doctor` under the `[doctor]` section's policy: its findings,
/// then an error when they fail the build.
fn doctor(
    magento: &magequery_core::Magento,
    policy: &magequery_core::DoctorPolicy,
    diag: &dyn Fn(Diagnostic),
) -> Result<()> {
    use magequery_core::Severity as Found;
    let check = magento
        .doctor_check(policy)
        .map_err(|e| anyhow::anyhow!("doctor: {e}"))?;
    let root = magento.root();
    for f in &check.report.findings {
        let tag = match f.severity {
            Found::Error => "error",
            Found::Warning => "warn",
            Found::Info => "info",
        };
        let at = f.source.as_ref().map_or(String::new(), |s| {
            let file = s.file.strip_prefix(root).unwrap_or(&s.file);
            format!("  {}:{}", file.display(), s.line)
        });
        eprintln!("  doctor: {tag}[{}] {}{at}", f.code, f.message);
        let severity = match f.severity {
            Found::Error => Severity::Error,
            Found::Warning => Severity::Warning,
            Found::Info => Severity::Info,
        };
        let mut d = Diagnostic::new("doctor", severity, &f.message);
        d.code = Some(f.code.to_owned());
        d.subject = f.subject.clone();
        if let Some(s) = &f.source {
            d.file = Some(relative(&s.file, root));
            d.line = s.line;
        }
        diag(d);
    }
    let baseline_file = check
        .baseline
        .as_ref()
        .map(|(_, path)| relative(path, root));
    for s in check.stale() {
        let message = format!(
            "{} \"{}\" is accepted {} time(s) but occurs {}",
            s.entry.code, s.entry.message, s.entry.count, s.matched
        );
        eprintln!("  doctor: error[baseline] {message}");
        let mut d = Diagnostic::new("doctor-baseline", Severity::Error, message);
        d.code = Some(s.entry.code.clone());
        d.file = baseline_file.clone();
        d.line = s.line;
        diag(d);
    }
    eprintln!(
        "  doctor: {} error(s), {} warning(s), {} info",
        check.errors(),
        check.report.warnings(),
        check.report.infos()
    );
    if check.failed() {
        anyhow::bail!(
            "doctor: the [doctor] section fails the build on {}",
            if check.errors() > 0 {
                "its errors"
            } else {
                "warnings (fail_on_warnings)"
            }
        );
    }
    Ok(())
}

/// `file` relative to `root`, `/`-separated, where it lies inside it.
fn relative(file: &Path, root: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

/// `composer dump-autoload -o --no-dev`, in-process. Run AFTER `di-compile` so
/// `generated/code` is classmapped.
fn autoload_dump(root: &Path, no_dev: bool, optimize: bool) -> Result<()> {
    let req = composer_autoload::DumpRequest {
        project_root: root,
        optimize,
        classmap_authoritative: false,
        no_dev,
        apcu_autoloader: false,
        apcu_prefix: None,
        autoloader_suffix: None,
    };
    let report = composer_autoload::dump_autoload(&req)
        .map_err(|e| anyhow::anyhow!("autoload dump: {e}"))?;
    eprintln!("  autoload-dump: {} class(es) mapped", report.class_count);
    Ok(())
}

/// The theme×locale×area matrix — an in-process `magecommand` engine call
/// (`static_deploy::deploy::deploy_to_disk`), the same entry point the
/// `magecommand static deploy` CLI drives. An explicit `command` override still
/// shells out (the escape hatch for a bespoke deploy invocation).
#[allow(clippy::too_many_arguments)]
fn static_deploy(
    root: &Path,
    themes: &[String],
    locales: &[String],
    areas: &[String],
    no_parent: bool,
    no_less: bool,
    no_js_bundle: bool,
    no_html_minify: bool,
    symlink: Symlink,
    deployed_version: Option<&str>,
    command: Option<&str>,
    fail_on: &[crate::graph::DeployFailOn],
    diag: &dyn Fn(Diagnostic),
) -> Result<()> {
    // An explicit override wins — honor a bespoke deploy command verbatim.
    if let Some(cmd) = command {
        return run_command(cmd, None, &BTreeMap::new(), root);
    }

    use magecommand::static_deploy::deploy as sdd;
    use magecommand::static_deploy::files as sdf;

    // magebuild's `"*"` sentinel = "all deployable themes"; an empty theme
    // filter makes `deploy_to_disk` discover every registered theme.
    let themes: Vec<String> = themes.iter().filter(|t| *t != "*").cloned().collect();

    // Link instead of writing the same bytes once per theme and locale: per
    // file into `vendor/`/`app/`/`lib/web/` (safe here, the artifact ships
    // those sources beside `pub/static`), or per identical directory inside
    // `pub/static`. Opt in via magebuild.toml / `--preset hyva`; the
    // MAGEBUILD_SYMLINK env turns it on where the config left it off (file
    // links, or `MAGEBUILD_SYMLINK=dir`), mirroring MAGEBUILD_HARDLINK.
    let symlink = match (symlink, std::env::var_os("MAGEBUILD_SYMLINK")) {
        (Symlink::Off, Some(v)) if v == "dir" => Symlink::Dir,
        (Symlink::Off, Some(_)) => Symlink::File,
        (mode, _) => mode,
    };

    let req = sdd::DeployRequest {
        locales: locales.to_vec(),
        themes,
        areas: areas.to_vec(),
        out: None, // default: <root>/pub/static
        // Bundle in byte-sorted order, not the build filesystem's readdir
        // order. The probe reproduces what PHP would bundle on THIS machine,
        // and on ext4 that order follows the filesystem's hash seed, so two CI
        // runners on different images split the same modules into different
        // bundles and every deploy resends them. Sorted is the same on every
        // machine; RequireJS doesn't care about the order.
        order: sdd::Order::Sorted,
        no_parent,      // default false: a child theme pulls in its parents
        no_less,        // Hyvä: no LESS to compile (Tailwind is plain css)
        no_js_bundle,   // Hyvä: no RequireJS bundles
        no_html_minify, // parity no-op (magecommand never minifies html)
        symlink: match symlink {
            Symlink::Off => sdf::Symlink::None,
            Symlink::File => sdf::Symlink::ToSource,
            Symlink::Dir => sdf::Symlink::IdenticalDirs,
        },
        deployed_version: deployed_version.map(str::to_string),
        jobs: None,         // rayon global pool — overlaps di-compile's own pool
        no_compress: false, // production-mode compressed CSS
    };

    let summary = sdd::deploy_to_disk(root, &req).map_err(|e| anyhow::anyhow!("{e:#}"))?;
    for d in deploy_diagnostics(
        summary
            .skipped
            .iter()
            .map(|s| (s.id.as_str(), s.reason.as_str())),
        summary.stats.iter().flat_map(|stat| {
            stat.warnings.iter().map(move |(file, message)| {
                (
                    (
                        stat.area.as_str(),
                        stat.theme.as_str(),
                        stat.locale.as_str(),
                    ),
                    file.as_str(),
                    message.as_str(),
                )
            })
        }),
        fail_on,
    ) {
        diag(d);
    }

    for s in &summary.skipped {
        eprintln!("  warning (scd): skipping theme {} — {}", s.id, s.reason);
    }
    // A stylesheet that does not compile is left out and the deploy goes on,
    // as `setup:static-content:deploy` does. Say so: a store without
    // styles-m.css is not something to find out from a customer. The same
    // error usually repeats for every theme and locale, so group by it. A
    // failed entry's message starts with "entry <file>:" (LessDeployError);
    // anything else is a compiler warning on a stylesheet that did deploy.
    let mut failed: BTreeMap<(&str, &str), Vec<String>> = BTreeMap::new();
    let mut compiler_warnings = 0usize;
    for stat in &summary.stats {
        for (file, message) in &stat.warnings {
            if !message.starts_with("entry ") {
                compiler_warnings += 1;
                continue;
            }
            failed
                .entry((file.as_str(), message.as_str()))
                .or_default()
                .push(format!("{}/{}/{}", stat.area, stat.theme, stat.locale));
        }
    }
    for ((file, message), packages) in &failed {
        let first = message.lines().next().unwrap_or(message);
        let more = match packages.len() {
            1 => String::new(),
            n => format!(" and {} more", n - 1),
        };
        eprintln!(
            "  warning (scd): {file} NOT DEPLOYED for {}{more}: {first}",
            packages[0]
        );
    }
    if !failed.is_empty() {
        let stylesheets: usize = failed.values().map(Vec::len).sum();
        eprintln!(
            "  warning (scd): {stylesheets} stylesheet(s) did not compile and are missing from pub/static"
        );
    }
    if compiler_warnings > 0 {
        eprintln!(
            "  static-deploy: {compiler_warnings} LESS compiler warning(s) on deployed stylesheets"
        );
    }
    if let Some(v) = deployed_version {
        eprintln!("  static-deploy: deployed_version.txt = {v}");
    }
    let files: usize = summary.stats.iter().map(|s| s.files).sum();
    let bytes: usize = summary.stats.iter().map(|s| s.bytes).sum();
    eprintln!(
        "  static-deploy: {} package(s), {} file(s), {:.1} MB in {:.2}s",
        summary.stats.len(),
        files,
        bytes as f64 / (1024.0 * 1024.0),
        summary.elapsed.as_secs_f64(),
    );
    use crate::graph::DeployFailOn;
    let failed: Vec<&str> = fail_on
        .iter()
        .filter(|kind| match kind {
            DeployFailOn::NotDeployed => !failed.is_empty(),
            DeployFailOn::SkippedTheme => !summary.skipped.is_empty(),
        })
        .map(|kind| kind.name())
        .collect();
    if !failed.is_empty() {
        anyhow::bail!("static-deploy: fail_on = {failed:?}");
    }
    Ok(())
}

/// static-deploy's diagnostics: each skipped theme `(id, reason)`, and each
/// compiler warning `((area, theme, locale), file, message)`, which is a
/// stylesheet left out when it starts "entry " and a warning on one that
/// deployed otherwise. What `fail_on` gates is an error.
fn deploy_diagnostics<'a>(
    skipped: impl Iterator<Item = (&'a str, &'a str)>,
    warnings: impl Iterator<Item = ((&'a str, &'a str, &'a str), &'a str, &'a str)>,
    fail_on: &[crate::graph::DeployFailOn],
) -> Vec<Diagnostic> {
    use crate::graph::DeployFailOn;
    let mut out = Vec::new();
    for (id, reason) in skipped {
        let gated = fail_on.contains(&DeployFailOn::SkippedTheme);
        let mut d = Diagnostic::new("skipped-theme", Severity::warning_unless(gated), reason);
        d.subject = Some(id.to_owned());
        // `adminhtml/Vendor/theme`.
        if let Some((area, theme)) = id.split_once('/') {
            d.areas = vec![area.to_owned()];
            d.theme = Some(theme.to_owned());
        }
        out.push(d);
    }
    for ((area, theme, locale), file, message) in warnings {
        let failed = message.starts_with("entry ");
        let (kind, severity) = if failed {
            let gated = fail_on.contains(&DeployFailOn::NotDeployed);
            ("not-deployed", Severity::warning_unless(gated))
        } else {
            ("less-warning", Severity::Info)
        };
        let mut d = Diagnostic::new(kind, severity, message);
        d.file = Some(file.to_owned());
        d.areas = vec![area.to_owned()];
        d.theme = Some(theme.to_owned());
        d.locale = Some(locale.to_owned());
        out.push(d);
    }
    out
}

/// A `sh -c` subprocess node.
fn run_command(
    run: &str,
    cwd: Option<&Path>,
    env: &BTreeMap<String, String>,
    root: &Path,
) -> Result<()> {
    let dir = cwd.unwrap_or(root);
    let mut cmd = std::process::Command::new("sh");
    cmd.arg("-c").arg(run).current_dir(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let status = cmd.status().with_context(|| format!("spawning `{run}`"))?;
    if !status.success() {
        bail!("command `{run}` exited with {status}");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Native tar packaging
// ---------------------------------------------------------------------------

/// zstd compression level for `.tar.zst` artifacts — zstd's fast default
/// (denser than gzip-6 at a fraction of the time).
const ZSTD_LEVEL: i32 = 3;

/// The artifact compression, chosen from the output extension.
#[derive(Clone, Copy)]
enum Compress {
    /// `.tar` — no compression.
    None,
    /// `.tar.gz` / `.tgz` — gzip, one member per segment.
    Gzip,
    /// `.tar.zst` / `.tzst` — zstd, one frame per segment.
    Zstd,
}

/// Package the project tree into `output`, honoring an excludes file.
/// Compression is chosen by extension (`.tar.gz`/`.tgz` → gzip, `.tar.zst`/
/// `.tzst` → zstd, else uncompressed); see [`write_tar`] for how the work is
/// spread over the pool.
fn package(root: &Path, output: &Path, exclude_from: Option<&Path>, jobs: usize) -> Result<()> {
    let patterns = match exclude_from {
        Some(p) => read_excludes(p)?,
        None => Vec::new(),
    };
    let matcher = ExcludeMatcher::new(patterns);

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // Never package the artifact into itself.
    let output_abs = std::path::absolute(output).unwrap_or_else(|_| output.to_path_buf());
    // Nor its manifest, and never leave one from an earlier build beside it.
    let sidecar = manifest::sidecar_path(output);
    match std::fs::remove_file(&sidecar) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            return Err(e).with_context(|| format!("removing {}", sidecar.display()));
        }
        _ => {}
    }
    let skip = [manifest::sidecar_path(&output_abs), output_abs];

    let ext = output
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let compress = match ext.as_str() {
        "gz" | "tgz" => Compress::Gzip,
        "zst" | "tzst" => Compress::Zstd,
        _ => Compress::None,
    };

    let mut file = std::fs::File::create(output)
        .with_context(|| format!("creating archive {}", output.display()))?;
    let Written {
        count: written,
        manifest: entries,
        len,
        sha256,
    } = write_tar(root, &mut file, compress, &matcher, &skip, jobs)?;
    drop(file);
    eprintln!(
        "  package: {} entr{} -> {}",
        written,
        if written == 1 { "y" } else { "ies" },
        output.display()
    );
    match entries {
        Some(entries) => manifest::write_sidecar(output, len, &sha256, &entries)?,
        None => eprintln!(
            "  package: no manifest written (the tree has entries it cannot describe), \
             so a deploy reads the artifact instead"
        ),
    }
    Ok(())
}

/// One walked tree entry destined for the archive (directories are never
/// emitted — tar records files, and empty dirs are rare in a Magento tree).
struct Entry {
    path: PathBuf,
    rel: PathBuf,
    kind: EntryKind,
}

/// A regular file or a symlink (each carries its `symlink_metadata`, so the
/// writer stamps the tar header without a second `stat`, and a symlink its
/// target, read during the walk) versus a special node the writer appends via
/// the `stat`-ing `append_path_with_name`.
enum EntryKind {
    File(std::fs::Metadata),
    Symlink {
        meta: std::fs::Metadata,
        target: PathBuf,
    },
    Other,
}

/// The deterministic walk: a directory's own entries in sorted order, then its
/// subdirectories in reverse sorted order, each in full (the order of the
/// explicit stack this replaced, so the archive's bytes are unchanged).
/// Directories are walked in parallel and their results joined in that order:
/// a static deploy with symlinks leaves several hundred thousand entries that
/// cost an `lstat` and a `readlink` each.
fn collect_entries(root: &Path, matcher: &ExcludeMatcher, skip: &[PathBuf]) -> Result<Vec<Entry>> {
    walk_dir(root, root, matcher, skip)
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    matcher: &ExcludeMatcher,
    skip: &[PathBuf],
) -> Result<Vec<Entry>> {
    use rayon::prelude::*;
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    paths.sort();
    // `Ok` is a file to archive, `Err` a directory to descend into.
    let classified: Vec<std::result::Result<Entry, PathBuf>> = paths
        .into_par_iter()
        .with_min_len(64)
        .filter_map(|path| {
            let rel = path.strip_prefix(root).ok()?.to_path_buf();
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let meta = std::fs::symlink_metadata(&path).ok()?;
            // Don't archive the output or its manifest into itself.
            if skip.iter().any(|s| s.file_name() == path.file_name())
                && std::path::absolute(&path).is_ok_and(|p| skip.contains(&p))
            {
                return None;
            }
            let is_dir = meta.is_dir();
            if matcher.excluded(&rel_str, is_dir) {
                return None;
            }
            if is_dir {
                return Some(Err(path));
            }
            let kind = if meta.file_type().is_file() {
                EntryKind::File(meta)
            } else {
                match std::fs::read_link(&path) {
                    Ok(target) if meta.file_type().is_symlink() => {
                        EntryKind::Symlink { meta, target }
                    }
                    _ => EntryKind::Other,
                }
            };
            Some(Ok(Entry { path, rel, kind }))
        })
        .collect();
    let mut out = Vec::with_capacity(classified.len());
    let mut dirs = Vec::new();
    for c in classified {
        match c {
            Ok(entry) => out.push(entry),
            Err(dir) => dirs.push(dir),
        }
    }
    let below: Vec<Vec<Entry>> = dirs
        .par_iter()
        .rev()
        .map(|d| walk_dir(root, d, matcher, skip))
        .collect::<Result<_>>()?;
    out.extend(below.into_iter().flatten());
    Ok(out)
}

/// How much tar a worker serializes and compresses in one piece. Small enough
/// that a few per thread fit in memory and every core has work, large enough
/// that compressing each piece by itself costs next to nothing in size.
const SEGMENT_BYTES: u64 = 8 << 20;

/// A run of consecutive entries, serialized and compressed on one worker.
struct Segment {
    /// The compressed bytes, to be written as they are.
    frame: Vec<u8>,
    count: usize,
    /// `None` when the run holds something the manifest can't describe.
    manifest: Option<Vec<manifest::Entry>>,
}

/// Compress one piece of the tar stream as a stream of its own: a zstd frame
/// or a gzip member. Either format is defined as a concatenation of those, so
/// the pieces written one after another are an ordinary `.tar.zst`/`.tar.gz`.
fn compress_frame(tar: Vec<u8>, compress: Compress) -> Result<Vec<u8>> {
    match compress {
        Compress::None => Ok(tar),
        Compress::Zstd => zstd::bulk::compress(&tar, ZSTD_LEVEL).context("compressing (zstd)"),
        Compress::Gzip => {
            // Level 6: flate2's default, and gzip's.
            let mut enc = flate2::write::GzEncoder::new(
                Vec::with_capacity(tar.len() / 3),
                flate2::Compression::new(6),
            );
            enc.write_all(&tar).context("compressing (gzip)")?;
            enc.finish().context("compressing (gzip)")
        }
    }
}

/// Serialize `entries` as tar (without the end-of-archive marker), hashing each
/// file as it is read, and compress the result.
fn tar_segment(entries: Vec<Entry>, compress: Compress) -> Result<Segment> {
    use std::os::unix::fs::PermissionsExt;
    let size: u64 = entries
        .iter()
        .map(|e| match &e.kind {
            EntryKind::File(meta) => 1024 + meta.len(),
            _ => 512,
        })
        .sum();
    let mut described = Some(Vec::with_capacity(entries.len()));
    let count = entries.len();
    let mut builder = tar::Builder::new(Vec::with_capacity(size as usize + 2048));
    builder.follow_symlinks(false);
    for Entry { path, rel, kind } in entries {
        match kind {
            EntryKind::File(meta) => {
                let bytes =
                    std::fs::read(&path).with_context(|| format!("reading {}", rel.display()))?;
                let mut header = tar::Header::new_gnu();
                header.set_metadata(&meta);
                header.set_size(bytes.len() as u64);
                builder
                    .append_data(&mut header, &rel, &bytes[..])
                    .with_context(|| format!("archiving {}", rel.display()))?;
                let kind = manifest::Kind::File {
                    mode: meta.permissions().mode() & 0o7777,
                    size: bytes.len() as u64,
                    sha256: crate::deploy::runner::hex(&Sha256::digest(&bytes)),
                    mtime: None,
                };
                describe(&mut described, &rel, kind);
            }
            EntryKind::Symlink { meta, target } => {
                let mut header = tar::Header::new_gnu();
                header.set_metadata(&meta);
                builder
                    .append_link(&mut header, &rel, &target)
                    .with_context(|| format!("archiving {}", rel.display()))?;
                match target.to_str() {
                    Some(target) => {
                        let target = target.to_string();
                        describe(&mut described, &rel, manifest::Kind::Symlink { target })
                    }
                    None => described = None,
                }
            }
            // A socket, a fifo: archived, but the manifest can't describe it.
            EntryKind::Other => {
                builder
                    .append_path_with_name(&path, &rel)
                    .with_context(|| format!("archiving {}", path.display()))?;
                described = None;
            }
        }
    }
    // The builder ends what it writes with the end-of-archive marker, which
    // belongs after the last segment only.
    let mut tar = builder.into_inner().context("serializing tar")?;
    let end = tar.len().saturating_sub(TAR_END.len());
    anyhow::ensure!(tar[end..] == TAR_END, "tar segment has no end marker");
    tar.truncate(end);
    Ok(Segment {
        frame: compress_frame(tar, compress)?,
        count,
        manifest: described,
    })
}

/// tar's end-of-archive marker: two zero blocks.
const TAR_END: [u8; 1024] = [0; 1024];

/// Walk `root` and write every non-excluded file to `out` as a (compressed)
/// tar. Returns the entry count, the deploy manifest (`None` when the tree
/// holds something it can't describe) and the length and sha256 of what was
/// written.
///
/// tar's byte stream is sequential, but nothing in it spans entries, and zstd
/// and gzip both define a file as a concatenation of independently compressed
/// pieces. So the walked entries (in their deterministic order) are cut into
/// runs of about [`SEGMENT_BYTES`], and each run is read, hashed, serialized
/// and compressed on the rayon pool. This thread only writes the finished
/// pieces in order, and a second one hashes them for the manifest, so nothing
/// that scales with the tree runs on a single core. At most `window` runs are
/// in flight (`sync_channel(window)` back-pressures the producer), so memory
/// stays at about that many segments, not the whole tree.
fn write_tar(
    root: &Path,
    out: &mut impl Write,
    compress: Compress,
    matcher: &ExcludeMatcher,
    skip: &[PathBuf],
    jobs: usize,
) -> Result<Written> {
    let entries = collect_entries(root, matcher, skip)?;
    let mut described: Option<Vec<manifest::Entry>> = Some(Vec::with_capacity(entries.len()));
    let mut count = 0usize;

    let window = (jobs.max(1) * 2).max(4);
    let (job_tx, job_rx) = mpsc::sync_channel::<mpsc::Receiver<Result<Segment>>>(window);
    let (hash_tx, hash_rx) = mpsc::sync_channel::<Vec<u8>>(window);

    let (len, sha256) = std::thread::scope(|s| -> Result<(u64, String)> {
        // Producer: cut the entries into runs and hand each to the pool,
        // forwarding the pending results IN ORDER.
        s.spawn(move || {
            let mut entries = entries.into_iter().peekable();
            while entries.peek().is_some() {
                let mut run = Vec::new();
                let mut size = 0u64;
                while size < SEGMENT_BYTES
                    && let Some(e) = entries.next()
                {
                    size += match &e.kind {
                        EntryKind::File(meta) => 1024 + meta.len(),
                        _ => 512,
                    };
                    run.push(e);
                }
                let (tx, rx) = mpsc::sync_channel(1);
                rayon::spawn(move || {
                    let _ = tx.send(tar_segment(run, compress));
                });
                if job_tx.send(rx).is_err() {
                    break; // the writer failed and dropped the receiver
                }
            }
        });
        let hasher = s.spawn(move || {
            let mut h = Sha256::new();
            let mut len = 0u64;
            for frame in hash_rx {
                len += frame.len() as u64;
                h.update(&frame);
            }
            (len, crate::deploy::runner::hex(&h.finalize()))
        });

        // Writer (this thread): the pieces in walk order. On error, returning
        // drops `job_rx` and `hash_tx`, so the producer's next `send` fails,
        // the hasher's loop ends, and the scope joins.
        let mut write = |frame: Vec<u8>| -> Result<()> {
            out.write_all(&frame).context("writing the archive")?;
            hash_tx
                .send(frame)
                .map_err(|_| anyhow::anyhow!("archive hasher vanished"))
        };
        for pending in job_rx {
            let segment = pending
                .recv()
                .map_err(|_| anyhow::anyhow!("a packaging worker vanished"))??;
            write(segment.frame)?;
            count += segment.count;
            match (described.as_mut(), segment.manifest) {
                (Some(all), Some(part)) => all.extend(part),
                _ => described = None,
            }
        }
        write(compress_frame(TAR_END.to_vec(), compress)?)?;
        drop(write);
        drop(hash_tx);
        hasher
            .join()
            .map_err(|_| anyhow::anyhow!("archive hasher panicked"))
    })?;
    out.flush().context("writing the archive")?;

    Ok(Written {
        count,
        manifest: described,
        len,
        sha256,
    })
}

/// What [`write_tar`] archived.
struct Written {
    count: usize,
    /// `None` when the tree holds something the manifest can't describe.
    manifest: Option<Vec<manifest::Entry>>,
    /// The length and sha256 of the archive as written.
    len: u64,
    sha256: String,
}

fn describe(manifest: &mut Option<Vec<manifest::Entry>>, rel: &Path, kind: manifest::Kind) {
    let path = rel.to_str().map(|p| p.replace('\\', "/"));
    match (manifest.as_mut(), path) {
        (Some(m), Some(path)) => m.push(manifest::Entry { path, kind }),
        _ => *manifest = None,
    }
}

fn read_excludes(path: &Path) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading excludes file {}", path.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            l.trim_start_matches("./")
                .trim_start_matches('/')
                .to_string()
        })
        .collect())
}

/// A small path-exclude matcher: exact match, directory-prefix, and `*.ext`.
struct ExcludeMatcher {
    exact: Vec<String>,
    suffix: Vec<String>,
}

impl ExcludeMatcher {
    fn new(patterns: Vec<String>) -> ExcludeMatcher {
        let mut exact = Vec::new();
        let mut suffix = Vec::new();
        for p in patterns {
            if let Some(ext) = p.strip_prefix("*.") {
                suffix.push(format!(".{ext}"));
            } else {
                exact.push(p.trim_end_matches('/').to_string());
            }
        }
        ExcludeMatcher { exact, suffix }
    }

    /// `rel` is a `/`-joined path relative to the project root.
    fn excluded(&self, rel: &str, _is_dir: bool) -> bool {
        if self.suffix.iter().any(|s| rel.ends_with(s.as_str())) {
            return true;
        }
        self.exact
            .iter()
            .any(|p| rel == p || rel.starts_with(&format!("{p}/")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_deploy_diagnostics_name_their_package_and_follow_fail_on() {
        use crate::graph::DeployFailOn;
        let skipped = [("frontend/Acme/broken", "parent Acme/gone is not installed")];
        let warnings = [
            (
                ("frontend", "Acme/shop", "en_US"),
                "css/styles-m.css",
                "entry css/styles-m.less: undefined variable",
            ),
            (
                ("frontend", "Acme/shop", "nl_NL"),
                "css/print.css",
                "deprecated syntax",
            ),
        ];
        let all = |fail_on: &[DeployFailOn]| {
            deploy_diagnostics(skipped.iter().copied(), warnings.iter().copied(), fail_on)
        };
        let by_kind =
            |ds: &[Diagnostic], kind: &str| ds.iter().find(|d| d.kind == kind).cloned().unwrap();

        let ds = all(&[]);
        let theme = by_kind(&ds, "skipped-theme");
        assert_eq!(theme.severity, Severity::Warning);
        assert_eq!(theme.areas, ["frontend"]);
        assert_eq!(theme.theme.as_deref(), Some("Acme/broken"));
        let missing = by_kind(&ds, "not-deployed");
        assert_eq!(missing.severity, Severity::Warning);
        assert_eq!(missing.file.as_deref(), Some("css/styles-m.css"));
        assert_eq!(missing.locale.as_deref(), Some("en_US"));
        assert_eq!(by_kind(&ds, "less-warning").severity, Severity::Info);

        let ds = all(&[DeployFailOn::NotDeployed, DeployFailOn::SkippedTheme]);
        assert_eq!(by_kind(&ds, "skipped-theme").severity, Severity::Error);
        assert_eq!(by_kind(&ds, "not-deployed").severity, Severity::Error);
        assert_eq!(
            by_kind(&ds, "less-warning").severity,
            Severity::Info,
            "never gated"
        );
    }

    #[test]
    fn the_package_step_writes_the_manifest_a_deploy_would_read() {
        use crate::deploy::manifest::{Sidecar, from_artifact, read_sidecar, sidecar_path};
        use crate::deploy::protocol::ArtifactFormat;
        use std::os::unix::fs::{PermissionsExt, symlink};

        let d = std::env::temp_dir().join(format!("magebuild-package-{}", std::process::id()));
        let root = d.join("tree");
        std::fs::create_dir_all(root.join("vendor/a/deep")).unwrap();
        std::fs::create_dir_all(root.join("artifacts")).unwrap();
        std::fs::write(root.join("vendor/a/deep/x.php"), "<?php // x").unwrap();
        std::fs::write(root.join("vendor/a/empty"), "").unwrap();
        std::fs::write(root.join("bin-magento"), "#!/usr/bin/env php").unwrap();
        std::fs::set_permissions(
            root.join("bin-magento"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("vendor/a/deep/x.php", root.join("link")).unwrap();
        // Enough for several segments, so the artifact is several frames.
        for n in 0..3u8 {
            let body = vec![n; SEGMENT_BYTES as usize + 1];
            std::fs::write(root.join(format!("vendor/a/big{n}")), body).unwrap();
        }

        for name in ["artifact.tar", "artifact.tar.gz", "artifact.tar.zst"] {
            let out = root.join("artifacts").join(name);
            // A manifest left over from an earlier build is replaced, and is
            // never archived.
            std::fs::write(sidecar_path(&out), "stale").unwrap();
            package(&root, &out, None, 2).unwrap();
            let format = ArtifactFormat::from_path(&out).unwrap();
            let Sidecar::Found(built, _) = read_sidecar(&out).unwrap() else {
                panic!("{name}: no usable manifest beside the artifact");
            };
            let read = from_artifact(&out, format).unwrap().unwrap();
            assert_eq!(built, read, "{name}");
            assert_eq!(built.len(), 7, "{name}");
            std::fs::remove_file(&out).unwrap();
            std::fs::remove_file(sidecar_path(&out)).unwrap();
        }

        // A manifest describes only the artifact it was written with.
        let out = root.join("artifacts/artifact.tar");
        package(&root, &out, None, 2).unwrap();
        std::fs::write(root.join("vendor/a/empty"), "now not").unwrap();
        let sidecar = std::fs::read(sidecar_path(&out)).unwrap();
        package(&root, &out, None, 2).unwrap();
        std::fs::write(sidecar_path(&out), sidecar).unwrap();
        assert!(matches!(read_sidecar(&out).unwrap(), Sidecar::Unusable(_)));
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// A scratch Magento-ish project: a lock with two packages and the given
    /// `composer.json` and patch files.
    fn patch_project(name: &str, composer_json: &str, files: &[(&str, &str)]) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("magebuild-patches-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("composer.json"), composer_json).unwrap();
        std::fs::write(
            root.join("composer.lock"),
            r#"{"packages":[{"name":"acme/foo"},{"name":"acme/bar"}],"packages-dev":[]}"#,
        )
        .unwrap();
        for (path, body) in files {
            let f = root.join(path);
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, body).unwrap();
        }
        root
    }

    /// A project-root-relative diff touching one file in `vendor/<pkg>`.
    fn diff_for(pkg: &str) -> String {
        format!(
            "--- a/vendor/{pkg}/src/A.php\n+++ b/vendor/{pkg}/src/A.php\n@@ -1 +1 @@\n-old\n+new\n"
        )
    }

    #[test]
    fn patch_plan_aborts_on_failure_and_writes_the_readable_lock() {
        // The plan's sources and parsing are composer-patches' (and tested
        // there); magebuild only sets its policy on top.
        let root = patch_project(
            "policy",
            r#"{"extra":{"patches":{"acme/foo":["patches/foo.patch"]}}}"#,
            &[("patches/foo.patch", &diff_for("acme/foo"))],
        );
        let plan = build_patch_plan(&root, &root.join("cache/composer-dist"))
            .unwrap()
            .expect("a plan");
        assert_eq!(plan.patches["acme/foo"].len(), 1);
        assert_eq!(plan.failure_mode, composer_patches::FailureMode::Abort);
        assert!(plan.write_lock);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn exclude_matcher_matches_dir_prefix_and_suffix() {
        let m = ExcludeMatcher::new(vec![
            "var".into(),
            "generated/".into(),
            "*.log".into(),
            ".git".into(),
        ]);
        assert!(m.excluded("var", true));
        assert!(m.excluded("var/cache/x", false));
        assert!(m.excluded("generated/code/Foo.php", false));
        assert!(m.excluded("app/error.log", false));
        assert!(m.excluded(".git/HEAD", false));
        assert!(!m.excluded("app/code/Vendor/Module/registration.php", false));
        // A prefix must be a whole path component, not a substring.
        assert!(!m.excluded("variables/x", false));
    }
}
