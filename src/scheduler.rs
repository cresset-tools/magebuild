//! The parallel, DAG-ordered ready-queue scheduler.
//!
//! A node is *ready* when all its `after` deps are Done (or Skipped). Ready
//! nodes are dispatched concurrently up to `jobs`. CPU-bound native nodes run
//! on a `jobs`-sized rayon pool (so `di-compile`'s nested `par_iter`s and any
//! other CPU node work-steal against each other); I/O-bound / subprocess nodes
//! run on their own OS thread so they never pin a rayon worker. The first
//! failure cancels not-yet-started nodes, lets in-flight ones finish, and
//! reports.
//!
//! The scheduler is generic over a `runner` closure, so the DAG behavior
//! (ordering, parallelism cap, failure propagation) is unit-tested with fake
//! nodes — no Magento checkout, no engines.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::graph::{Cost, Graph};

/// Final state of a node after a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeState {
    Done,
    Failed(String),
    /// Declared `When::Never`, or cancelled because it never started after a
    /// prior failure.
    Skipped,
    Cancelled,
}

/// Per-node result, in graph-declaration order.
#[derive(Debug, Clone)]
pub struct NodeOutcome {
    pub id: String,
    pub state: NodeState,
    pub duration: Duration,
}

/// The whole run.
#[derive(Debug, Clone)]
pub struct RunReport {
    pub outcomes: Vec<NodeOutcome>,
    pub failed: bool,
    pub wall: Duration,
}

impl RunReport {
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn get(&self, id: &str) -> Option<&NodeOutcome> {
        self.outcomes.iter().find(|o| o.id == id)
    }
}

/// Progress sink. The CLI plugs a printing/JSON observer in; tests use a no-op.
pub trait Observer: Send + Sync {
    fn on_start(&self, _id: &str) {}
    fn on_finish(&self, _id: &str, _state: &NodeState, _dur: Duration) {}
    fn on_skip(&self, _id: &str) {}
}

/// A no-op observer.
pub struct SilentObserver;
impl Observer for SilentObserver {}

/// The node-execution callback: run this node to completion, `Err` = failure.
pub type Runner = dyn Fn(&crate::graph::Node) -> anyhow::Result<()> + Send + Sync;

/// Run `graph` with a bounded parallel ready-queue.
///
/// `graph` must already be validated (`Graph::validate`). `jobs` bounds the
/// number of concurrently-running nodes (min 1).
pub fn run(
    graph: &Graph,
    jobs: usize,
    runner: Arc<Runner>,
    observer: Arc<dyn Observer>,
) -> RunReport {
    let jobs = jobs.max(1);
    let nodes = graph.nodes();
    let n = nodes.len();
    let started = Instant::now();

    // id -> index, and each node's dependents (reverse edges).
    let idx: HashMap<&str, usize> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| (node.id.as_str(), i))
        .collect();
    let mut indeg = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, node) in nodes.iter().enumerate() {
        for dep in &node.after {
            if let Some(&d) = idx.get(dep.as_str()) {
                indeg[i] += 1;
                dependents[d].push(i);
            }
        }
    }

    let mut state: Vec<Option<NodeState>> = vec![None; n];
    let mut duration = vec![Duration::ZERO; n];

    // Seed the roots (original indeg 0). `seed` recursively resolves any
    // `When::Never` node (marking it Skipped and satisfying its dependents, so a
    // chain of skips cascades) and enqueues the genuinely-ready ones. Each node
    // reaches indeg 0 exactly once, so nothing is double-enqueued.
    let roots: Vec<usize> = (0..n).filter(|&i| indeg[i] == 0).collect();
    let mut ready: Vec<usize> = Vec::new();
    for i in roots {
        seed(
            i,
            nodes,
            &mut indeg,
            &dependents,
            &mut ready,
            &mut state,
            &observer,
        );
    }

    // A jobs-sized rayon pool: CPU-bound nodes run here so their nested
    // par_iters share these threads. Falls back to running inline if the pool
    // can't be built (never observed).
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs)
        .thread_name(|i| format!("magebuild-{i}"))
        .build()
        .ok();

    let (tx, rx) = mpsc::channel::<(usize, Result<(), String>, Duration)>();
    let mut in_flight = 0usize;
    let mut cancel = false;

    loop {
        // Dispatch as many ready nodes as the job budget allows.
        while !cancel && in_flight < jobs {
            let Some(i) = ready.pop() else { break };
            let node = nodes[i].clone();
            let cost = node.kind.cost();
            observer.on_start(&node.id);
            let tx = tx.clone();
            let runner = runner.clone();
            let task = move || {
                let start = Instant::now();
                let res = runner(&node).map_err(|e| format!("{e:#}"));
                let _ = tx.send((i, res, start.elapsed()));
            };
            match (cost, pool.as_ref()) {
                (Cost::CpuBound, Some(p)) => p.spawn(task),
                _ => {
                    std::thread::spawn(task);
                }
            }
            in_flight += 1;
        }

        if in_flight == 0 {
            break;
        }

        let (i, res, dur) = rx.recv().expect("worker channel closed early");
        in_flight -= 1;
        duration[i] = dur;
        match res {
            Ok(()) => {
                state[i] = Some(NodeState::Done);
                observer.on_finish(&nodes[i].id, &NodeState::Done, dur);
                for &d in &dependents[i] {
                    indeg[d] -= 1;
                    if indeg[d] == 0 && !cancel {
                        seed(
                            d,
                            nodes,
                            &mut indeg,
                            &dependents,
                            &mut ready,
                            &mut state,
                            &observer,
                        );
                    }
                }
            }
            Err(msg) => {
                let st = NodeState::Failed(msg);
                observer.on_finish(&nodes[i].id, &st, dur);
                state[i] = Some(st);
                cancel = true;
                ready.clear();
            }
        }
    }

    // Anything never dispatched (blocked behind a failure) is Cancelled.
    for (i, slot) in state.iter_mut().enumerate() {
        if slot.is_none() {
            *slot = Some(NodeState::Cancelled);
            observer.on_finish(&nodes[i].id, &NodeState::Cancelled, Duration::ZERO);
        }
    }

    let outcomes: Vec<NodeOutcome> = nodes
        .iter()
        .enumerate()
        .map(|(i, node)| NodeOutcome {
            id: node.id.clone(),
            state: state[i].take().unwrap(),
            duration: duration[i],
        })
        .collect();
    let failed = outcomes
        .iter()
        .any(|o| matches!(o.state, NodeState::Failed(_)));

    RunReport {
        outcomes,
        failed,
        wall: started.elapsed(),
    }
}

/// A newly indeg-0 node: skip it (satisfying dependents) or enqueue it.
#[allow(clippy::too_many_arguments)]
fn seed(
    i: usize,
    nodes: &[crate::graph::Node],
    indeg: &mut [usize],
    dependents: &[Vec<usize>],
    ready: &mut Vec<usize>,
    state: &mut [Option<NodeState>],
    observer: &Arc<dyn Observer>,
) {
    if state[i].is_some() {
        return;
    }
    if nodes[i].is_skipped() {
        state[i] = Some(NodeState::Skipped);
        observer.on_skip(&nodes[i].id);
        // Satisfy dependents; a chain of skips cascades.
        let deps = dependents[i].clone();
        for d in deps {
            indeg[d] -= 1;
            if indeg[d] == 0 {
                seed(d, nodes, indeg, dependents, ready, state, observer);
            }
        }
    } else {
        ready.push(i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::{BuiltinStep, Node, NodeKind};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn cpu(id: &str, after: &[&str]) -> Node {
        Node::native(id, after, BuiltinStep::DiCompile { fused: false })
    }

    // An I/O node forces the OS-thread path (Command cost is IoBound).
    fn io(id: &str, after: &[&str]) -> Node {
        Node {
            id: id.into(),
            after: after.iter().map(|s| s.to_string()).collect(),
            kind: NodeKind::Command {
                run: "true".into(),
                cwd: None,
                env: Default::default(),
            },
            when: crate::graph::When::Always,
        }
    }

    #[test]
    fn runs_all_in_dependency_order() {
        let graph = Graph::new(vec![
            cpu("a", &[]),
            cpu("b", &["a"]),
            cpu("c", &["a"]),
            cpu("d", &["b", "c"]),
        ]);
        let log = Arc::new(Mutex::new(Vec::<String>::new()));
        let log2 = log.clone();
        let runner: Arc<Runner> = Arc::new(move |node| {
            log2.lock().unwrap().push(node.id.clone());
            Ok(())
        });
        let report = run(&graph, 4, runner, Arc::new(SilentObserver));
        assert!(!report.failed);
        let seen = log.lock().unwrap();
        let pos = |x: &str| seen.iter().position(|y| y == x).unwrap();
        assert!(pos("a") < pos("b") && pos("a") < pos("c"));
        assert!(pos("d") > pos("b") && pos("d") > pos("c"));
        assert_eq!(seen.len(), 4);
    }

    #[test]
    fn respects_the_job_limit() {
        // Fan of 6 independent nodes, jobs=2 → never more than 2 concurrent.
        let graph = Graph::new((0..6).map(|i| io(&format!("n{i}"), &[])).collect());
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (live2, peak2) = (live.clone(), peak.clone());
        let runner: Arc<Runner> = Arc::new(move |_node| {
            let now = live2.fetch_add(1, Ordering::SeqCst) + 1;
            peak2.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(30));
            live2.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        });
        let report = run(&graph, 2, runner, Arc::new(SilentObserver));
        assert!(!report.failed);
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak concurrency exceeded jobs"
        );
    }

    #[test]
    fn parallelism_actually_overlaps() {
        // Two independent 60ms nodes with jobs=2 finish well under the 120ms
        // serial wall — proves overlap.
        let graph = Graph::new(vec![io("x", &[]), io("y", &[])]);
        let runner: Arc<Runner> = Arc::new(|_node| {
            std::thread::sleep(Duration::from_millis(60));
            Ok(())
        });
        let report = run(&graph, 2, runner, Arc::new(SilentObserver));
        assert!(
            report.wall < Duration::from_millis(115),
            "no overlap: {:?}",
            report.wall
        );
    }

    #[test]
    fn first_failure_cancels_downstream_but_drains_inflight() {
        let graph = Graph::new(vec![
            cpu("a", &[]),
            cpu("b", &["a"]),
            cpu("c", &["b"]), // downstream of the failure → cancelled
        ]);
        let runner: Arc<Runner> = Arc::new(|node| {
            if node.id == "b" {
                anyhow::bail!("boom");
            }
            Ok(())
        });
        let report = run(&graph, 4, runner, Arc::new(SilentObserver));
        assert!(report.failed);
        assert_eq!(report.get("a").unwrap().state, NodeState::Done);
        assert!(matches!(
            report.get("b").unwrap().state,
            NodeState::Failed(_)
        ));
        assert_eq!(report.get("c").unwrap().state, NodeState::Cancelled);
    }

    #[test]
    fn skipped_nodes_satisfy_dependents() {
        let mut graph = Graph::new(vec![cpu("a", &[]), cpu("b", &["a"])]);
        graph.skip(&["a".into()]).unwrap();
        let ran = Arc::new(AtomicUsize::new(0));
        let ran2 = ran.clone();
        let runner: Arc<Runner> = Arc::new(move |_| {
            ran2.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let report = run(&graph, 2, runner, Arc::new(SilentObserver));
        assert!(!report.failed);
        assert_eq!(report.get("a").unwrap().state, NodeState::Skipped);
        assert_eq!(report.get("b").unwrap().state, NodeState::Done);
        assert_eq!(ran.load(Ordering::SeqCst), 1); // only b ran
    }
}
