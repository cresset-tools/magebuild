//! The built-in default Magento build graph, and `magebuild.toml` — the
//! declarative overlay that overrides node fields, converts nodes to commands,
//! and adds project nodes.
//!
//! Precedence: built-in defaults ← `magebuild.toml` ← CLI flags.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::graph::{BuiltinStep, Graph, Node, NodeKind, When};

/// A named bundle of graph presets — a one-flag shortcut for a common stack's
/// build shape. Applied over the built-in defaults but UNDER `magebuild.toml`,
/// so a project can still override any field the preset set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Preset {
    /// Hyvä production build (per the Hyvä deploy docs): a Tailwind
    /// `npm run build` per discovered Hyvä theme, then a static-content deploy
    /// with `--no-parent --no-less --no-js-bundle --no-html-minify --symlink file`.
    Hyva,
}

/// Graph-shaping inputs from the CLI (applied last, over the toml).
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// `--artifact` — when set, the `package` node is active with this output.
    pub artifact: Option<PathBuf>,
    /// `--exclude-from` — package excludes file.
    pub exclude_from: Option<PathBuf>,
    /// `--jobs`.
    pub jobs: Option<usize>,
    /// `--deployed-version` — the static-deploy content-version signature
    /// written to `pub/static/deployed_version.txt`.
    pub deployed_version: Option<String>,
    /// `--preset` — a named graph preset applied over the defaults, under the toml.
    pub preset: Option<Preset>,
}

/// The five-node default Magento build graph:
/// `composer-install → { di-compile → autoload-dump } ∥ { static-deploy } → package`.
pub fn default_graph() -> Graph {
    Graph::new(vec![
        Node::native(
            "composer-install",
            &[],
            BuiltinStep::ComposerInstall {
                no_dev: true,
                cache_root: None,
                hardlink: false,
            },
        ),
        Node::native(
            "di-compile",
            &["composer-install"],
            BuiltinStep::DiCompile { fused: false },
        ),
        Node::native(
            "autoload-dump",
            &["di-compile"],
            BuiltinStep::AutoloadDump {
                no_dev: true,
                optimize: true,
            },
        ),
        Node::native(
            "static-deploy",
            &["composer-install"],
            BuiltinStep::StaticDeploy {
                themes: vec!["*".into()],
                locales: vec!["en_US".into()],
                areas: vec!["frontend".into(), "adminhtml".into()],
                no_parent: false,
                no_less: false,
                no_js_bundle: false,
                no_html_minify: false,
                symlink: false,
                deployed_version: None,
                command: None,
            },
        ),
        // `package` starts inactive; `--artifact` (or toml) turns it on.
        Node {
            id: "package".into(),
            after: vec!["autoload-dump".into(), "static-deploy".into()],
            kind: NodeKind::Native(BuiltinStep::Package {
                output: PathBuf::from("artifact.tar.gz"),
                exclude_from: None,
            }),
            when: When::Never,
        },
    ])
}

/// `magebuild.toml`.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub build: BuildSection,
    #[serde(default)]
    pub nodes: BTreeMap<String, NodeSpec>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct BuildSection {
    pub jobs: Option<usize>,
}

/// A per-node override / addition. Every field is optional; unset fields keep
/// the built-in default.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NodeSpec {
    /// Setting `run` converts the node to a `Command` (or defines a new one).
    pub run: Option<String>,
    pub cwd: Option<PathBuf>,
    pub env: Option<BTreeMap<String, String>>,
    pub after: Option<Vec<String>>,
    // Built-in field overrides (ignored when `run` is set):
    pub no_dev: Option<bool>,
    pub optimize: Option<bool>,
    pub fused: Option<bool>,
    pub hardlink: Option<bool>,
    pub cache_root: Option<PathBuf>,
    pub themes: Option<Vec<String>>,
    pub locales: Option<Vec<String>>,
    pub areas: Option<Vec<String>>,
    pub no_parent: Option<bool>,
    pub no_less: Option<bool>,
    pub no_js_bundle: Option<bool>,
    pub no_html_minify: Option<bool>,
    pub symlink: Option<bool>,
    pub deployed_version: Option<String>,
    pub exclude_from: Option<PathBuf>,
    pub output: Option<PathBuf>,
}

impl FileConfig {
    /// Load from a path. A missing file is not an error — it yields the empty
    /// (defaults-only) config. A present-but-malformed file *is* an error.
    pub fn load(path: &std::path::Path) -> Result<FileConfig> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(FileConfig::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
}

/// Build the resolved graph: defaults ← file ← CLI. Returns the graph and the
/// effective job count.
pub fn resolve(file: &FileConfig, opts: &BuildOptions, root: &Path) -> Result<(Graph, usize)> {
    let mut graph = default_graph();

    // A `--preset` shapes the defaults BEFORE the toml, so an explicit
    // `magebuild.toml` still wins over the preset's choices.
    if let Some(preset) = opts.preset {
        apply_preset(&mut graph, preset, root);
    }

    // Apply the toml node overrides/additions (sorted for determinism).
    for (id, spec) in &file.nodes {
        apply_spec(&mut graph, id, spec)?;
    }

    // CLI: --artifact activates `package` and sets its output; --exclude-from
    // sets its excludes. These win over the toml.
    if let Some(node) = graph.get_mut("package")
        && let NodeKind::Native(BuiltinStep::Package {
            output,
            exclude_from,
        }) = &mut node.kind
    {
        if let Some(art) = &opts.artifact {
            *output = art.clone();
            node.when = When::Always;
        }
        if let Some(ex) = &opts.exclude_from {
            *exclude_from = Some(ex.clone());
        }
    }

    // --deployed-version overrides the static-deploy node's content version.
    if let Some(dv) = &opts.deployed_version
        && let Some(node) = graph.get_mut("static-deploy")
        && let NodeKind::Native(BuiltinStep::StaticDeploy {
            deployed_version, ..
        }) = &mut node.kind
    {
        *deployed_version = Some(dv.clone());
    }

    graph.validate().context("invalid build graph")?;

    let jobs = opts
        .jobs
        .or(file.build.jobs)
        .unwrap_or_else(default_jobs)
        .max(1);
    Ok((graph, jobs))
}

/// Shape the default graph for a named [`Preset`].
fn apply_preset(graph: &mut Graph, preset: Preset, root: &Path) {
    match preset {
        Preset::Hyva => apply_hyva_preset(graph, root),
    }
}

/// The Hyvä production build (Hyvä deploy docs): a Tailwind `npm run build` per
/// discovered Hyvä theme, wired before a static-content deploy that runs with
/// `--no-parent --no-less --no-js-bundle --no-html-minify --symlink file`.
fn apply_hyva_preset(graph: &mut Graph, root: &Path) {
    // Step 2 — the deploy flags on `static-deploy`.
    if let Some(node) = graph.get_mut("static-deploy")
        && let NodeKind::Native(BuiltinStep::StaticDeploy {
            no_parent,
            no_less,
            no_js_bundle,
            no_html_minify,
            symlink,
            ..
        }) = &mut node.kind
    {
        *no_parent = true;
        *no_less = true;
        *no_js_bundle = true;
        *no_html_minify = true;
        *symlink = true;
    }

    // Step 1 — a Tailwind build per discovered Hyvä theme, before static-deploy.
    let tailwinds = find_hyva_tailwind(root);
    if tailwinds.is_empty() {
        eprintln!(
            "warning: --preset hyva found no Hyvä theme web/tailwind (with package.json) \
             under app/design/frontend or vendor/hyva-themes; skipping the npm build step. \
             Add it in magebuild.toml if your Tailwind lives elsewhere."
        );
        return;
    }
    let mut ids: Vec<String> = Vec::new();
    for dir in tailwinds {
        let mut id = tailwind_node_id(&dir);
        // Disambiguate a rare leaf-name collision so both themes still build.
        while graph.get(&id).is_some() || ids.contains(&id) {
            id.push('_');
        }
        graph.push(Node {
            id: id.clone(),
            after: vec!["composer-install".into()],
            kind: NodeKind::Command {
                run: "npm ci --ignore-scripts && npm run build".into(),
                cwd: Some(dir),
                env: BTreeMap::new(),
            },
            when: When::Always,
        });
        ids.push(id);
    }
    // static-deploy waits for every theme's styles.css to be built.
    if let Some(node) = graph.get_mut("static-deploy") {
        for id in ids {
            if !node.after.contains(&id) {
                node.after.push(id);
            }
        }
    }
}

/// Discover Hyvä `web/tailwind` build dirs (those with a `package.json`).
/// Project themes under `app/design/frontend/<Vendor>/<name>/web/tailwind` are
/// preferred; only if none exist do we fall back to the composer-installed
/// `vendor/hyva-themes/<pkg>/web/tailwind` (the demo case — a vendor default
/// theme used directly). Returns ABSOLUTE dirs (so a Command node's `cwd`
/// resolves regardless of the process cwd), sorted for determinism.
fn find_hyva_tailwind(root: &Path) -> Vec<PathBuf> {
    let root = std::path::absolute(root).unwrap_or_else(|_| root.to_path_buf());
    let mut dirs = Vec::new();
    collect_tailwind(&root.join("app/design/frontend"), 2, &mut dirs);
    if dirs.is_empty() {
        collect_tailwind(&root.join("vendor/hyva-themes"), 1, &mut dirs);
    }
    dirs.sort();
    dirs.dedup();
    dirs
}

/// Walk `base` down `wildcard_levels` of subdirectories, collecting each
/// `<dir>/web/tailwind` that holds a `package.json`.
fn collect_tailwind(base: &Path, wildcard_levels: usize, out: &mut Vec<PathBuf>) {
    if wildcard_levels == 0 {
        let tw = base.join("web/tailwind");
        if tw.join("package.json").is_file() {
            out.push(tw);
        }
        return;
    }
    if let Ok(rd) = std::fs::read_dir(base) {
        for entry in rd.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                collect_tailwind(&entry.path(), wildcard_levels - 1, out);
            }
        }
    }
}

/// A readable, stable node id for a theme's Tailwind build, from the theme dir
/// name (the parent of `web/tailwind`): `hyva-tailwind-<theme>`.
fn tailwind_node_id(tailwind_dir: &Path) -> String {
    let theme = tailwind_dir
        .parent() // .../web
        .and_then(Path::parent) // .../<theme>
        .and_then(Path::file_name)
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| "theme".into());
    let slug: String = theme
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("hyva-tailwind-{slug}")
}

/// Apply one `NodeSpec` to the graph (mutating an existing node or adding a new
/// one).
fn apply_spec(graph: &mut Graph, id: &str, spec: &NodeSpec) -> Result<()> {
    let exists = graph.get(id).is_some();

    if !exists {
        // A new node must be a command.
        let Some(run) = &spec.run else {
            bail!("new node `{id}` needs a `run` (only built-in nodes can be field-tuned)");
        };
        graph.push(Node {
            id: id.to_string(),
            after: spec.after.clone().unwrap_or_default(),
            kind: NodeKind::Command {
                run: run.clone(),
                cwd: spec.cwd.clone(),
                env: spec.env.clone().unwrap_or_default(),
            },
            when: When::Always,
        });
        return Ok(());
    }

    // Existing node.
    if let Some(after) = &spec.after {
        graph.get_mut(id).unwrap().after = after.clone();
    }

    if let Some(run) = &spec.run {
        // Convert to a command node.
        let node = graph.get_mut(id).unwrap();
        node.kind = NodeKind::Command {
            run: run.clone(),
            cwd: spec.cwd.clone(),
            env: spec.env.clone().unwrap_or_default(),
        };
        return Ok(());
    }

    // Field overrides on the built-in step.
    let node = graph.get_mut(id).unwrap();
    if let NodeKind::Native(step) = &mut node.kind {
        override_step(step, spec);
        if matches!(step, BuiltinStep::Package { .. }) && spec.output.is_some() {
            node.when = When::Always;
        }
    }
    Ok(())
}

fn override_step(step: &mut BuiltinStep, spec: &NodeSpec) {
    match step {
        BuiltinStep::ComposerInstall {
            no_dev,
            cache_root,
            hardlink,
        } => {
            if let Some(v) = spec.no_dev {
                *no_dev = v;
            }
            if let Some(v) = &spec.cache_root {
                *cache_root = Some(v.clone());
            }
            if let Some(v) = spec.hardlink {
                *hardlink = v;
            }
        }
        BuiltinStep::DiCompile { fused } => {
            if let Some(v) = spec.fused {
                *fused = v;
            }
        }
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
            ..
        } => {
            if let Some(v) = &spec.themes {
                *themes = v.clone();
            }
            if let Some(v) = &spec.locales {
                *locales = v.clone();
            }
            if let Some(v) = &spec.areas {
                *areas = v.clone();
            }
            if let Some(v) = spec.no_parent {
                *no_parent = v;
            }
            if let Some(v) = spec.no_less {
                *no_less = v;
            }
            if let Some(v) = spec.no_js_bundle {
                *no_js_bundle = v;
            }
            if let Some(v) = spec.no_html_minify {
                *no_html_minify = v;
            }
            if let Some(v) = spec.symlink {
                *symlink = v;
            }
            if let Some(v) = &spec.deployed_version {
                *deployed_version = Some(v.clone());
            }
        }
        BuiltinStep::AutoloadDump { no_dev, optimize } => {
            if let Some(v) = spec.no_dev {
                *no_dev = v;
            }
            if let Some(v) = spec.optimize {
                *optimize = v;
            }
        }
        BuiltinStep::Package {
            output,
            exclude_from,
        } => {
            if let Some(v) = &spec.output {
                *output = v.clone();
            }
            if let Some(v) = &spec.exclude_from {
                *exclude_from = Some(v.clone());
            }
        }
    }
}

/// Default parallelism: available CPUs (min 1).
pub fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_graph_is_valid_and_shaped_right() {
        let g = default_graph();
        g.validate().unwrap();
        let waves = g.waves().unwrap();
        assert_eq!(waves[0], vec!["composer-install"]);
        // di-compile and static-deploy overlap.
        assert_eq!(waves[1], vec!["di-compile", "static-deploy"]);
        // package is present but skipped by default.
        assert!(g.get("package").unwrap().is_skipped());
    }

    #[test]
    fn artifact_flag_activates_package() {
        let (g, _) = resolve(
            &FileConfig::default(),
            &BuildOptions {
                artifact: Some(PathBuf::from("out.tar.gz")),
                ..Default::default()
            },
            Path::new("."),
        )
        .unwrap();
        let pkg = g.get("package").unwrap();
        assert!(!pkg.is_skipped());
        match &pkg.kind {
            NodeKind::Native(BuiltinStep::Package { output, .. }) => {
                assert_eq!(output, &PathBuf::from("out.tar.gz"));
            }
            _ => panic!("package is not a Package step"),
        }
    }

    #[test]
    fn toml_overrides_builtin_fields() {
        let file: FileConfig = toml::from_str(
            r#"
            [build]
            jobs = 3
            [nodes.di-compile]
            fused = true
            [nodes.static-deploy]
            locales = ["en_US", "nl_NL"]
            "#,
        )
        .unwrap();
        let (g, jobs) = resolve(&file, &BuildOptions::default(), Path::new(".")).unwrap();
        assert_eq!(jobs, 3);
        match &g.get("di-compile").unwrap().kind {
            NodeKind::Native(BuiltinStep::DiCompile { fused }) => assert!(*fused),
            _ => panic!(),
        }
        match &g.get("static-deploy").unwrap().kind {
            NodeKind::Native(BuiltinStep::StaticDeploy { locales, .. }) => {
                assert_eq!(locales, &vec!["en_US".to_string(), "nl_NL".to_string()]);
            }
            _ => panic!(),
        }
    }

    #[test]
    fn toml_can_convert_node_to_command() {
        let file: FileConfig = toml::from_str(
            r#"
            [nodes.autoload-dump]
            run = "composer dump-autoload -o --no-dev"
            "#,
        )
        .unwrap();
        let (g, _) = resolve(&file, &BuildOptions::default(), Path::new(".")).unwrap();
        match &g.get("autoload-dump").unwrap().kind {
            NodeKind::Command { run, .. } => {
                assert_eq!(run, "composer dump-autoload -o --no-dev");
            }
            _ => panic!("expected a command node"),
        }
    }

    #[test]
    fn toml_can_add_and_hook_a_node() {
        let file: FileConfig = toml::from_str(
            r#"
            [nodes.my-assets]
            after = ["composer-install"]
            run = "php bin/generate.php"
            [nodes.package]
            after = ["autoload-dump", "static-deploy", "my-assets"]
            output = "release.tar"
            "#,
        )
        .unwrap();
        let (g, _) = resolve(&file, &BuildOptions::default(), Path::new(".")).unwrap();
        assert!(g.get("my-assets").is_some());
        let pkg = g.get("package").unwrap();
        assert!(pkg.after.contains(&"my-assets".to_string()));
        // output set via toml also activates package.
        assert!(!pkg.is_skipped());
    }

    #[test]
    fn new_node_without_run_errors() {
        let file: FileConfig = toml::from_str(
            r#"
            [nodes.bogus]
            after = ["composer-install"]
            "#,
        )
        .unwrap();
        assert!(resolve(&file, &BuildOptions::default(), Path::new(".")).is_err());
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        assert!(toml::from_str::<FileConfig>("[nope]\nx = 1\n").is_err());
    }

    #[test]
    fn hyva_preset_sets_flags_and_wires_tailwind_build() {
        // A synthetic root with a composer-installed Hyvä theme's web/tailwind.
        let base = std::env::temp_dir().join(format!("magebuild-hyva-{}", std::process::id()));
        let tw = base.join("vendor/hyva-themes/acme-theme/web/tailwind");
        std::fs::create_dir_all(&tw).unwrap();
        std::fs::write(tw.join("package.json"), "{}").unwrap();

        let opts = BuildOptions {
            preset: Some(Preset::Hyva),
            ..Default::default()
        };
        let (g, _) = resolve(&FileConfig::default(), &opts, &base).unwrap();

        // Step 2 — every Hyvä deploy flag is set on static-deploy.
        match &g.get("static-deploy").unwrap().kind {
            NodeKind::Native(BuiltinStep::StaticDeploy {
                no_parent,
                no_less,
                no_js_bundle,
                no_html_minify,
                symlink,
                ..
            }) => assert!(
                *no_parent && *no_less && *no_js_bundle && *no_html_minify && *symlink,
                "all Hyvä deploy flags must be on"
            ),
            _ => panic!("static-deploy is not a StaticDeploy step"),
        }

        // Step 1 — a Tailwind build node the deploy depends on, cwd at the theme.
        let id = "hyva-tailwind-acme-theme";
        let node = g.get(id).expect("tailwind node added");
        match &node.kind {
            NodeKind::Command { run, cwd, .. } => {
                assert_eq!(run, "npm ci --ignore-scripts && npm run build");
                assert_eq!(cwd.as_deref(), Some(tw.as_path()));
            }
            _ => panic!("tailwind node is not a command"),
        }
        assert!(node.after.contains(&"composer-install".to_string()));
        assert!(
            g.get("static-deploy")
                .unwrap()
                .after
                .contains(&id.to_string()),
            "static-deploy must wait for the tailwind build"
        );

        std::fs::remove_dir_all(&base).ok();
    }
}
