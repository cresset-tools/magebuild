//! The build DAG: node model, validation (cycles + dangling deps), topological
//! order, and the parallel-wave view used by `--dry-run`/`plan`.
//!
//! The graph is intentionally engine-free: it knows about node ids, edges, and
//! node *kinds*, but never executes anything. Execution lives in
//! [`crate::steps`]; scheduling in [`crate::scheduler`]. That split is what makes
//! the DAG unit-testable without a Magento checkout.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;

/// Scheduling hint: only CPU-bound native work runs on the shared rayon pool
/// (so `di-compile`'s nested `par_iter`s work-steal against other CPU nodes).
/// I/O-bound work (downloads, subprocess waits, tar) runs on its own OS thread
/// so it never pins a rayon worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cost {
    CpuBound,
    IoBound,
}

/// A built-in step with an in-process (linked) Rust implementation, except
/// [`BuiltinStep::StaticDeploy`] — see its docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuiltinStep {
    /// `composer install` from `composer.lock` (in-process via `composer-install`).
    ComposerInstall {
        no_dev: bool,
        /// Dist-archive cache dir; `None` ⇒ the persistent user cache
        /// (`MAGEBUILD_CACHE_DIR` or `~/.cache/magebuild/composer-dist`).
        cache_root: Option<PathBuf>,
        /// Hard-link packages out of a decompress-once store instead of
        /// extracting each install. Off by default — only wins with a
        /// persistent, uncompressed store (self-hosted CI, a docker layer,
        /// repeated local builds), not `actions/cache` (which re-decompresses
        /// on restore). `MAGEBUILD_HARDLINK` in the env forces it on too.
        hardlink: bool,
    },
    /// `setup:di:compile` (in-process via `magecommand-engine`).
    DiCompile { fused: bool },
    /// `setup:static-content:deploy` over the theme×locale×area matrix — an
    /// in-process linked call into `magecommand`'s `static_deploy` engine
    /// (`deploy::deploy_to_disk`), the same entry point the `magecommand static
    /// deploy` CLI drives. An explicit `command` override still shells out.
    StaticDeploy {
        themes: Vec<String>,
        locales: Vec<String>,
        areas: Vec<String>,
        /// Do NOT auto-deploy a theme's parent(s) (magecommand `--no-parent`).
        /// Default `false`: a child theme pulls its ancestors into the deploy
        /// (Magento's quick strategy). Set `true` when the deployed child tree
        /// is self-contained (the parent→child fallback is resolved from source
        /// at deploy time) and the parent theme is never served — e.g. a Hyvä
        /// storefront whose only Luma use is the fallback checkout: Magento/luma
        /// ships, but its parent Magento/blank need not.
        no_parent: bool,
        /// Skip LESS compilation (magecommand `--no-less`). Hyvä's Tailwind
        /// output is a plain `.css`, so there is no `.less` to compile.
        no_less: bool,
        /// Skip `js/bundle/bundle<N>.js` generation (magecommand
        /// `--no-js-bundle`). Hyvä doesn't use RequireJS bundles.
        no_js_bundle: bool,
        /// Accepted for Magento parity (`--no-html-minify`); a no-op in
        /// magecommand, which byte-copies `.html` and never minifies it.
        no_html_minify: bool,
        /// Materialize pure-copy assets as relative symlinks to their
        /// `vendor/app/lib` source (magecommand `--symlink file`) instead of
        /// copying — smaller, faster output when the artifact ships `vendor`
        /// beside `pub/static` (magebuild's model).
        symlink: bool,
        /// `pub/static/deployed_version.txt` contents — the asset-version
        /// signature stock SCD takes as `--content-version` (cache-busting).
        /// `None` ⇒ don't write the file (never an invented timestamp).
        deployed_version: Option<String>,
        /// Shell-out override for a bespoke deploy invocation; `None` ⇒ the
        /// in-process engine call.
        command: Option<String>,
    },
    /// `composer dump-autoload -o --no-dev` (in-process via `composer-autoload`).
    AutoloadDump { no_dev: bool, optimize: bool },
    /// Native tar packaging (in-process via `tar` + `flate2`).
    Package {
        /// Output archive; gzip is chosen from the extension.
        output: PathBuf,
        /// One exclude pattern per line.
        exclude_from: Option<PathBuf>,
    },
}

impl BuiltinStep {
    /// Scheduling cost. `DiCompile` is CPU-bound and runs its nested `par_iter`s
    /// on magebuild's pool, so it is `CpuBound`. `StaticDeploy` is linked too now
    /// but stays `IoBound` on purpose: it runs on its own OS thread and fans out
    /// over rayon's global pool internally, so it overlaps di-compile without
    /// nesting a second pool inside a magebuild-pool worker.
    pub fn cost(&self) -> Cost {
        match self {
            BuiltinStep::DiCompile { .. } => Cost::CpuBound,
            _ => Cost::IoBound,
        }
    }

    fn label(&self) -> String {
        match self {
            BuiltinStep::ComposerInstall {
                no_dev, hardlink, ..
            } => {
                format!(
                    "composer-install{}{}",
                    if *no_dev { " --no-dev" } else { "" },
                    if *hardlink { " hardlink" } else { "" }
                )
            }
            BuiltinStep::DiCompile { fused } => {
                format!("di-compile{}", if *fused { " --fused" } else { "" })
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
            } => format!(
                "static-deploy themes={} locales={} areas={}{}{}{}{}{}{}",
                themes.join(","),
                locales.join(","),
                areas.join(","),
                if *no_parent { " --no-parent" } else { "" },
                if *no_less { " --no-less" } else { "" },
                if *no_js_bundle { " --no-js-bundle" } else { "" },
                if *no_html_minify { " --no-html-minify" } else { "" },
                if *symlink { " --symlink=file" } else { "" },
                deployed_version
                    .as_deref()
                    .map(|v| format!(" version={v}"))
                    .unwrap_or_default(),
            ),
            BuiltinStep::AutoloadDump { no_dev, optimize } => format!(
                "autoload-dump{}{}",
                if *optimize { " -o" } else { "" },
                if *no_dev { " --no-dev" } else { "" }
            ),
            BuiltinStep::Package { output, .. } => {
                format!("package -> {}", output.display())
            }
        }
    }
}

/// What a node does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    /// A linked (in-process) built-in step.
    Native(BuiltinStep),
    /// An external subprocess (`sh -c <run>`).
    Command {
        run: String,
        cwd: Option<PathBuf>,
        env: BTreeMap<String, String>,
    },
}

impl NodeKind {
    pub fn cost(&self) -> Cost {
        match self {
            NodeKind::Native(step) => step.cost(),
            NodeKind::Command { .. } => Cost::IoBound,
        }
    }

    /// A one-line human description used by `plan`/progress.
    pub fn describe(&self) -> String {
        match self {
            NodeKind::Native(step) => format!("native: {}", step.label()),
            NodeKind::Command { run, .. } => format!("command: {run}"),
        }
    }
}

/// Whether a node runs. `Never` = declared-but-skipped (e.g. `package` with no
/// `--artifact`, or a `--skip`ped id); it still *satisfies* its dependents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Always,
    Never,
}

/// A DAG node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub id: String,
    pub after: Vec<String>,
    pub kind: NodeKind,
    pub when: When,
}

impl Node {
    pub fn native(id: &str, after: &[&str], step: BuiltinStep) -> Node {
        Node {
            id: id.to_string(),
            after: after.iter().map(|s| s.to_string()).collect(),
            kind: NodeKind::Native(step),
            when: When::Always,
        }
    }

    pub fn is_skipped(&self) -> bool {
        self.when == When::Never
    }
}

/// A validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GraphError {
    DuplicateId(String),
    UnknownDep {
        node: String,
        dep: String,
    },
    /// A cycle exists; the vector holds the ids still tangled after Kahn drains.
    Cycle(Vec<String>),
    UnknownNode(String),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::DuplicateId(id) => write!(f, "duplicate node id: {id}"),
            GraphError::UnknownDep { node, dep } => {
                write!(f, "node `{node}` depends on unknown node `{dep}`")
            }
            GraphError::Cycle(ids) => {
                write!(f, "dependency cycle among: {}", ids.join(", "))
            }
            GraphError::UnknownNode(id) => write!(f, "unknown node id: {id}"),
        }
    }
}

impl std::error::Error for GraphError {}

/// A validated-on-demand build graph.
#[derive(Debug, Clone, Default)]
pub struct Graph {
    nodes: Vec<Node>,
}

impl Graph {
    pub fn new(nodes: Vec<Node>) -> Graph {
        Graph { nodes }
    }

    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    pub fn get(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id == id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut Node> {
        self.nodes.iter_mut().find(|n| n.id == id)
    }

    pub fn push(&mut self, node: Node) {
        self.nodes.push(node);
    }

    fn index(&self) -> HashMap<&str, usize> {
        self.nodes
            .iter()
            .enumerate()
            .map(|(i, n)| (n.id.as_str(), i))
            .collect()
    }

    /// No duplicate ids, every `after` resolves, and the graph is acyclic.
    pub fn validate(&self) -> Result<(), GraphError> {
        let mut seen = BTreeSet::new();
        for n in &self.nodes {
            if !seen.insert(n.id.as_str()) {
                return Err(GraphError::DuplicateId(n.id.clone()));
            }
        }
        let idx = self.index();
        for n in &self.nodes {
            for dep in &n.after {
                if !idx.contains_key(dep.as_str()) {
                    return Err(GraphError::UnknownDep {
                        node: n.id.clone(),
                        dep: dep.clone(),
                    });
                }
            }
        }
        // Kahn's algorithm — completing = acyclic.
        self.topo().map(|_| ())
    }

    /// A deterministic topological order (node ids). Ready nodes are always
    /// drained in declaration order, so the result is stable across runs.
    pub fn topo(&self) -> Result<Vec<String>, GraphError> {
        let idx = self.index();
        let mut indeg = vec![0usize; self.nodes.len()];
        let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); self.nodes.len()];
        for (i, n) in self.nodes.iter().enumerate() {
            for dep in &n.after {
                let Some(&d) = idx.get(dep.as_str()) else {
                    return Err(GraphError::UnknownDep {
                        node: n.id.clone(),
                        dep: dep.clone(),
                    });
                };
                indeg[i] += 1;
                dependents[d].push(i);
            }
        }
        // Ready = declaration order among indeg-0 nodes (stable).
        let mut ready: Vec<usize> = (0..self.nodes.len()).filter(|&i| indeg[i] == 0).collect();
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut cursor = 0;
        while cursor < ready.len() {
            let i = ready[cursor];
            cursor += 1;
            out.push(self.nodes[i].id.clone());
            for &d in &dependents[i] {
                indeg[d] -= 1;
                if indeg[d] == 0 {
                    ready.push(d);
                }
            }
        }
        if out.len() != self.nodes.len() {
            let done: BTreeSet<&str> = out.iter().map(|s| s.as_str()).collect();
            let tangled: Vec<String> = self
                .nodes
                .iter()
                .filter(|n| !done.contains(n.id.as_str()))
                .map(|n| n.id.clone())
                .collect();
            return Err(GraphError::Cycle(tangled));
        }
        Ok(out)
    }

    /// Parallel schedule: `waves[w]` is the set of node ids that may run
    /// concurrently once every earlier wave is done. `wave(n) =
    /// max(wave(dep))+1`. Ids within a wave are sorted for stable rendering.
    pub fn waves(&self) -> Result<Vec<Vec<String>>, GraphError> {
        let order = self.topo()?;
        let idx = self.index();
        let mut level: HashMap<&str, usize> = HashMap::new();
        for id in &order {
            let n = &self.nodes[idx[id.as_str()]];
            let w = n
                .after
                .iter()
                .map(|dep| level.get(dep.as_str()).copied().unwrap_or(0) + 1)
                .max()
                .unwrap_or(0);
            level.insert(n.id.as_str(), w);
        }
        let max = level.values().copied().max().unwrap_or(0);
        let mut waves = vec![Vec::new(); max + 1];
        for n in &self.nodes {
            waves[level[n.id.as_str()]].push(n.id.clone());
        }
        for w in &mut waves {
            w.sort();
        }
        Ok(waves)
    }

    /// Transitive dependency closure of `targets` (inclusive), for `--only`
    /// and `magebuild node <id>`. Errors on an unknown target id.
    pub fn ancestors_of(&self, targets: &[String]) -> Result<BTreeSet<String>, GraphError> {
        let idx = self.index();
        let mut keep = BTreeSet::new();
        let mut stack = Vec::new();
        for t in targets {
            if !idx.contains_key(t.as_str()) {
                return Err(GraphError::UnknownNode(t.clone()));
            }
            stack.push(t.clone());
        }
        while let Some(id) = stack.pop() {
            if !keep.insert(id.clone()) {
                continue;
            }
            for dep in &self.nodes[idx[id.as_str()]].after {
                stack.push(dep.clone());
            }
        }
        Ok(keep)
    }

    /// Keep only `targets` + their transitive deps; drop the rest.
    pub fn restrict_to(&mut self, targets: &[String]) -> Result<(), GraphError> {
        let keep = self.ancestors_of(targets)?;
        self.nodes.retain(|n| keep.contains(n.id.as_str()));
        Ok(())
    }

    /// Mark ids `Never` (skipped-but-satisfying). Errors on an unknown id.
    pub fn skip(&mut self, ids: &[String]) -> Result<(), GraphError> {
        for id in ids {
            let node = self
                .get_mut(id)
                .ok_or_else(|| GraphError::UnknownNode(id.clone()))?;
            node.when = When::Never;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step() -> BuiltinStep {
        BuiltinStep::DiCompile { fused: false }
    }

    fn g(edges: &[(&str, &[&str])]) -> Graph {
        Graph::new(
            edges
                .iter()
                .map(|(id, after)| Node::native(id, after, step()))
                .collect(),
        )
    }

    #[test]
    fn topo_is_stable_and_respects_edges() {
        let graph = g(&[("a", &[]), ("b", &["a"]), ("c", &["a"]), ("d", &["b", "c"])]);
        let order = graph.topo().unwrap();
        let pos = |x: &str| order.iter().position(|y| y == x).unwrap();
        assert!(pos("a") < pos("b"));
        assert!(pos("a") < pos("c"));
        assert!(pos("d") > pos("b") && pos("d") > pos("c"));
        // Declaration order breaks ties: b before c.
        assert!(pos("b") < pos("c"));
    }

    #[test]
    fn detects_cycle() {
        let graph = g(&[("a", &["c"]), ("b", &["a"]), ("c", &["b"])]);
        match graph.validate() {
            Err(GraphError::Cycle(ids)) => {
                assert_eq!(ids.len(), 3);
            }
            other => panic!("expected cycle, got {other:?}"),
        }
    }

    #[test]
    fn detects_dangling_dep() {
        let graph = g(&[("a", &["missing"])]);
        assert_eq!(
            graph.validate(),
            Err(GraphError::UnknownDep {
                node: "a".into(),
                dep: "missing".into()
            })
        );
    }

    #[test]
    fn detects_duplicate_id() {
        let graph = g(&[("a", &[]), ("a", &[])]);
        assert_eq!(graph.validate(), Err(GraphError::DuplicateId("a".into())));
    }

    #[test]
    fn waves_group_parallel_work() {
        let graph = g(&[
            ("composer", &[]),
            ("di", &["composer"]),
            ("dump", &["di"]),
            ("scd", &["composer"]),
            ("package", &["dump", "scd"]),
        ]);
        let waves = graph.waves().unwrap();
        assert_eq!(waves[0], vec!["composer"]);
        // di and scd overlap.
        assert_eq!(waves[1], vec!["di", "scd"]);
        assert_eq!(waves[2], vec!["dump"]);
        assert_eq!(waves[3], vec!["package"]);
    }

    #[test]
    fn restrict_to_keeps_deps() {
        let mut graph = g(&[
            ("composer", &[]),
            ("di", &["composer"]),
            ("dump", &["di"]),
            ("scd", &["composer"]),
        ]);
        graph.restrict_to(&["dump".into()]).unwrap();
        let ids: BTreeSet<&str> = graph.nodes().iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, BTreeSet::from(["composer", "di", "dump"]));
    }

    #[test]
    fn skip_marks_never() {
        let mut graph = g(&[("a", &[]), ("b", &["a"])]);
        graph.skip(&["a".into()]).unwrap();
        assert!(graph.get("a").unwrap().is_skipped());
        assert!(graph.skip(&["nope".into()]).is_err());
    }
}
