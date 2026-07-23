//! Node execution: the in-process engine calls behind each [`BuiltinStep`],
//! plus subprocess `Command` nodes.
//!
//! All five built-ins are linked (in-process) engine calls:
//!
//! - `ComposerInstall` → `composer-install`
//! - `DiCompile`       → `magecommand-engine`
//! - `StaticDeploy`    → `magecommand` (`static_deploy::deploy::deploy_to_disk`)
//! - `AutoloadDump`    → `composer-autoload`
//! - `Package`         → native `tar` + `flate2`
//!
//! `StaticDeploy` still honors an explicit `command` override by shelling out
//! (the escape hatch for a bespoke deploy invocation).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use gzp::ZWriter;
use gzp::deflate::Gzip;
use gzp::par::compress::ParCompressBuilder;

use crate::graph::{BuiltinStep, Node, NodeKind};

/// Shared execution context: the project root every step operates on, and the
/// job budget (used by the packaging step to size its parallel-gzip pool).
pub struct Ctx {
    pub root: PathBuf,
    pub jobs: usize,
}

/// Run one node to completion. `Err` = the node failed.
pub fn execute(node: &Node, ctx: &Ctx) -> Result<()> {
    match &node.kind {
        NodeKind::Native(step) => run_builtin(step, ctx),
        NodeKind::Command { run, cwd, env } => run_command(run, cwd.as_deref(), env, &ctx.root),
    }
}

fn run_builtin(step: &BuiltinStep, ctx: &Ctx) -> Result<()> {
    match step {
        BuiltinStep::ComposerInstall {
            no_dev,
            cache_root,
            hardlink,
        } => composer_install(&ctx.root, *no_dev, cache_root.as_deref(), *hardlink),
        BuiltinStep::DiCompile { fused } => di_compile(&ctx.root, *fused),
        BuiltinStep::StaticDeploy {
            themes,
            locales,
            areas,
            no_parent,
            deployed_version,
            command,
        } => static_deploy(
            &ctx.root,
            themes,
            locales,
            areas,
            *no_parent,
            deployed_version.as_deref(),
            command.as_deref(),
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
    let env = composer_install::InstallEnv {
        fetcher: &fetcher,
        progress: &composer_install::NoProgress,
        cache_root: &cache,
    };
    let summary = composer_install::install_from_lock_with_patches(
        &env,
        root,
        composer_install::InstallOptions { no_dev, link_mode },
        None,
        None,
    )
    .map_err(|e| anyhow::anyhow!("{e:#}"))?;
    for w in &summary.warnings {
        eprintln!("  warning (composer): {w}");
    }
    Ok(())
}

/// `setup:di:compile`, in-process — the exact sequence the `magecommand` CLI
/// runs (`lib.rs::compile`), including the from-empty bring-up nuance: the class
/// universe scans the frozen `generated/_code` archive when present, else the
/// live `generated/code` (which we clear first).
fn di_compile(root: &Path, fused: bool) -> Result<()> {
    // Magento's `BP` must be absolute (it is baked into generated regexes).
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());

    let magento = magequery_core::Magento::open(&root)
        .with_context(|| format!("not a Magento root: {}", root.display()))?;

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
    let out = magecommand_engine::build::compute_outputs_opts(&magento, &mut defs, &root, fused);
    if !out.unresolved.is_empty() {
        eprintln!(
            "  note (di): {} class name(s) unresolvable via autoload maps (first: {})",
            out.unresolved.len(),
            out.unresolved.first().map(String::as_str).unwrap_or("")
        );
    }
    let written = magecommand_engine::metadata::write_generated(&root, &out.files)
        .context("writing generated/")?;
    eprintln!("  di-compile: wrote {written} generated/ file(s)");
    Ok(())
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
fn static_deploy(
    root: &Path,
    themes: &[String],
    locales: &[String],
    areas: &[String],
    no_parent: bool,
    deployed_version: Option<&str>,
    command: Option<&str>,
) -> Result<()> {
    // An explicit override wins — honor a bespoke deploy command verbatim.
    if let Some(cmd) = command {
        return run_command(cmd, None, &BTreeMap::new(), root);
    }

    use magecommand::static_deploy::deploy as sdd;

    // magebuild's `"*"` sentinel = "all deployable themes"; an empty theme
    // filter makes `deploy_to_disk` discover every registered theme.
    let themes: Vec<String> = themes.iter().filter(|t| *t != "*").cloned().collect();

    let req = sdd::DeployRequest {
        locales: locales.to_vec(),
        themes,
        areas: areas.to_vec(),
        out: None,                      // default: <root>/pub/static
        order: sdd::Order::Probe(None), // the CLI default — byte-faithful readdir order
        no_parent,                      // default false: a child theme pulls in its parents
        deployed_version: deployed_version.map(str::to_string),
        jobs: None,         // rayon global pool — overlaps di-compile's own pool
        no_compress: false, // production-mode compressed CSS
    };

    let summary = sdd::deploy_to_disk(root, &req).map_err(|e| anyhow::anyhow!("{e:#}"))?;

    for s in &summary.skipped {
        eprintln!("  warning (scd): skipping theme {} — {}", s.id, s.reason);
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
    Ok(())
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
enum Compress {
    /// `.tar` — no compression.
    None,
    /// `.tar.gz` / `.tgz` — parallel gzip via `gzp`.
    Gzip,
    /// `.tar.zst` / `.tzst` — multi-threaded zstd.
    Zstd,
}

/// Package the project tree into `output`, honoring an excludes file.
/// Compression is chosen by extension (`.tar.gz`/`.tgz` → gzip, `.tar.zst`/
/// `.tzst` → zstd, else uncompressed) and runs multi-threaded across `jobs`
/// (the tar serializer runs on this thread; the compression pipelines behind
/// it).
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

    let file = std::fs::File::create(output)
        .with_context(|| format!("creating archive {}", output.display()))?;
    let written = match compress {
        Compress::Gzip => {
            // Parallel gzip: the tar bytes stream into a worker pool that
            // compresses blocks concurrently (standard multi-member gzip;
            // `gunzip`/`tar xzf` read it). Level 6 matches flate2's default so
            // the artifact size is unchanged. Must `.finish()` explicitly
            // (dropping does not finalize).
            let par = ParCompressBuilder::<Gzip>::new()
                .compression_level(gzp::Compression::new(6))
                .num_threads(jobs.max(1))
                .map_err(|e| anyhow::anyhow!("sizing gzip pool: {e}"))?
                .from_writer(file);
            let (n, mut par) = write_tar(root, par, &matcher, &output_abs)?;
            par.finish()
                .map_err(|e| anyhow::anyhow!("finalizing gzip stream: {e}"))?;
            n
        }
        Compress::Zstd => {
            // zstd with libzstd's own worker pool (feature `zstdmt`): one `.zst`
            // stream, compressed multi-threaded internally. Must `.finish()`.
            let mut enc = zstd::stream::write::Encoder::new(file, ZSTD_LEVEL)
                .context("initializing zstd encoder")?;
            if jobs > 1 {
                enc.multithread(jobs as u32)
                    .context("enabling zstd multi-threading")?;
            }
            let (n, enc) = write_tar(root, enc, &matcher, &output_abs)?;
            enc.finish().context("finalizing zstd stream")?;
            n
        }
        Compress::None => {
            let (n, _file) = write_tar(root, file, &matcher, &output_abs)?;
            n
        }
    };
    eprintln!(
        "  package: {} entr{} -> {}",
        written,
        if written == 1 { "y" } else { "ies" },
        output.display()
    );
    Ok(())
}

/// Walk `root`, appending every non-excluded file to a tar stream. Returns the
/// entry count and the finalized underlying writer (so the caller can close a
/// compression stream that needs an explicit finish).
fn write_tar<W: Write>(
    root: &Path,
    writer: W,
    matcher: &ExcludeMatcher,
    output_abs: &Path,
) -> Result<(usize, W)> {
    let mut builder = tar::Builder::new(writer);
    builder.follow_symlinks(false);
    let mut count = 0usize;
    let mut stack = vec![root.to_path_buf()];
    // Deterministic walk: sort each directory's entries.
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        entries.sort();
        for path in entries {
            let rel = match path.strip_prefix(root) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if std::path::absolute(&path).ok().as_deref() == Some(output_abs) {
                continue; // don't archive the output into itself
            }
            let is_dir = meta.is_dir();
            if matcher.excluded(&rel_str, is_dir) {
                continue;
            }
            if is_dir {
                stack.push(path.clone());
                continue; // tar records files; empty dirs are rare in a Magento tree
            }
            builder
                .append_path_with_name(&path, rel)
                .with_context(|| format!("archiving {}", path.display()))?;
            count += 1;
        }
    }
    let writer = builder.into_inner().context("finalizing archive")?;
    Ok((count, writer))
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
