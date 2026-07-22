//! The built-in default Magento build graph, and `magebuild.toml` — the
//! declarative overlay that overrides node fields, converts nodes to commands,
//! and adds project nodes.
//!
//! Precedence: built-in defaults ← `magebuild.toml` ← CLI flags.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::graph::{BuiltinStep, Graph, Node, NodeKind, When};

/// Graph-shaping inputs from the CLI (applied last, over the toml).
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// `--artifact` — when set, the `package` node is active with this output.
    pub artifact: Option<PathBuf>,
    /// `--exclude-from` — package excludes file.
    pub exclude_from: Option<PathBuf>,
    /// `--jobs`.
    pub jobs: Option<usize>,
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
    pub cache_root: Option<PathBuf>,
    pub themes: Option<Vec<String>>,
    pub locales: Option<Vec<String>>,
    pub areas: Option<Vec<String>>,
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
pub fn resolve(file: &FileConfig, opts: &BuildOptions) -> Result<(Graph, usize)> {
    let mut graph = default_graph();

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

    graph.validate().context("invalid build graph")?;

    let jobs = opts
        .jobs
        .or(file.build.jobs)
        .unwrap_or_else(default_jobs)
        .max(1);
    Ok((graph, jobs))
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
        BuiltinStep::ComposerInstall { no_dev, cache_root } => {
            if let Some(v) = spec.no_dev {
                *no_dev = v;
            }
            if let Some(v) = &spec.cache_root {
                *cache_root = Some(v.clone());
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
        let (g, jobs) = resolve(&file, &BuildOptions::default()).unwrap();
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
        let (g, _) = resolve(&file, &BuildOptions::default()).unwrap();
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
        let (g, _) = resolve(&file, &BuildOptions::default()).unwrap();
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
        assert!(resolve(&file, &BuildOptions::default()).is_err());
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        assert!(toml::from_str::<FileConfig>("[nope]\nx = 1\n").is_err());
    }
}
