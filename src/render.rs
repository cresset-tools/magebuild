//! Human + JSON rendering: the `--dry-run`/`plan` DAG view, the live-progress
//! observer, and the final run summary.

use std::time::Duration;

use crate::graph::{Graph, When};
use crate::json::Json;
use crate::scheduler::{NodeOutcome, NodeState, Observer, RunReport};

/// Render the resolved DAG + parallel schedule (`--dry-run` / `plan`).
pub fn plan_text(graph: &Graph, jobs: usize) -> String {
    let mut out = String::new();
    let waves = match graph.waves() {
        Ok(w) => w,
        Err(e) => return format!("invalid graph: {e}\n"),
    };
    out.push_str(&format!(
        "build plan — {} node(s), {} wave(s), jobs={}\n\n",
        graph.nodes().len(),
        waves.len(),
        jobs
    ));
    for (w, ids) in waves.iter().enumerate() {
        let parallel = if ids.len() > 1 { "  (parallel)" } else { "" };
        out.push_str(&format!("wave {w}{parallel}\n"));
        for id in ids {
            let node = graph.get(id).unwrap();
            let skip = if node.when == When::Never {
                "  [skipped]"
            } else {
                ""
            };
            let deps = if node.after.is_empty() {
                String::new()
            } else {
                format!("  (after {})", node.after.join(", "))
            };
            out.push_str(&format!(
                "  - {}{}\n      {}{}\n",
                id,
                skip,
                node.kind.describe(),
                deps
            ));
        }
        out.push('\n');
    }
    let active = graph
        .nodes()
        .iter()
        .filter(|n| n.when != When::Never)
        .count();
    out.push_str(&format!(
        "critical path length: {} wave(s); {} node(s) will run, {} skipped.\n",
        waves.len(),
        active,
        graph.nodes().len() - active
    ));
    out
}

/// Machine-readable plan.
pub fn plan_json(graph: &Graph, jobs: usize) -> Json {
    let waves = graph.waves().unwrap_or_default();
    let nodes = graph
        .nodes()
        .iter()
        .map(|n| {
            Json::Obj(vec![
                ("id".into(), Json::s(&n.id)),
                (
                    "after".into(),
                    Json::Arr(n.after.iter().map(Json::s).collect()),
                ),
                ("kind".into(), Json::s(n.kind.describe())),
                ("cost".into(), Json::s(format!("{:?}", n.kind.cost()))),
                ("skipped".into(), Json::Bool(n.when == When::Never)),
            ])
        })
        .collect();
    let waves = waves
        .into_iter()
        .map(|w| Json::Arr(w.iter().map(Json::s).collect()))
        .collect();
    Json::Obj(vec![
        ("jobs".into(), Json::Num(jobs as u128)),
        ("nodes".into(), Json::Arr(nodes)),
        ("waves".into(), Json::Arr(waves)),
    ])
}

/// A line-oriented live-progress observer (compact `state · node · elapsed`).
pub struct ProgressObserver {
    json: bool,
}

impl ProgressObserver {
    pub fn new(json: bool) -> ProgressObserver {
        ProgressObserver { json }
    }
}

impl Observer for ProgressObserver {
    fn on_start(&self, id: &str) {
        if !self.json {
            eprintln!("▶  {id}  running");
        }
    }
    fn on_skip(&self, id: &str) {
        if !self.json {
            eprintln!("∘  {id}  skipped");
        }
    }
    fn on_finish(&self, id: &str, state: &NodeState, dur: Duration) {
        if self.json {
            return;
        }
        match state {
            NodeState::Done => eprintln!("✓  {id}  done  {}", human(dur)),
            NodeState::Failed(msg) => eprintln!("✗  {id}  FAILED  {}\n     {msg}", human(dur)),
            NodeState::Cancelled => eprintln!("-  {id}  cancelled"),
            NodeState::Skipped => {}
        }
    }
}

fn human(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 1.0 {
        format!("{:.0}ms", s * 1000.0)
    } else {
        format!("{s:.2}s")
    }
}

/// Final summary line + optional per-node profile.
pub fn summary_text(report: &RunReport, profile: bool) -> String {
    let mut out = String::new();
    let done = report
        .outcomes
        .iter()
        .filter(|o| o.state == NodeState::Done)
        .count();
    let skipped = report
        .outcomes
        .iter()
        .filter(|o| o.state == NodeState::Skipped)
        .count();
    let failed = report
        .outcomes
        .iter()
        .filter(|o| matches!(o.state, NodeState::Failed(_)))
        .count();
    let cancelled = report
        .outcomes
        .iter()
        .filter(|o| o.state == NodeState::Cancelled)
        .count();

    if profile {
        out.push_str("per-node timings:\n");
        let mut rows: Vec<&NodeOutcome> = report
            .outcomes
            .iter()
            .filter(|o| o.state == NodeState::Done || matches!(o.state, NodeState::Failed(_)))
            .collect();
        rows.sort_by_key(|o| std::cmp::Reverse(o.duration));
        for o in rows {
            out.push_str(&format!("  {:<20} {}\n", o.id, human(o.duration)));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "{} in {} — {done} done, {skipped} skipped, {failed} failed, {cancelled} cancelled",
        if report.failed {
            "BUILD FAILED"
        } else {
            "build ok"
        },
        human(report.wall),
    ));
    out.push('\n');
    out
}

/// Machine-readable run summary.
pub fn summary_json(report: &RunReport) -> Json {
    let nodes = report
        .outcomes
        .iter()
        .map(|o| {
            let state = match &o.state {
                NodeState::Done => "done".to_string(),
                NodeState::Skipped => "skipped".to_string(),
                NodeState::Cancelled => "cancelled".to_string(),
                NodeState::Failed(m) => format!("failed: {m}"),
            };
            Json::Obj(vec![
                ("id".into(), Json::s(&o.id)),
                ("state".into(), Json::s(state)),
                ("ms".into(), Json::Num(o.duration.as_millis())),
            ])
        })
        .collect();
    Json::Obj(vec![
        ("failed".into(), Json::Bool(report.failed)),
        ("wall_ms".into(), Json::Num(report.wall.as_millis())),
        ("nodes".into(), Json::Arr(nodes)),
    ])
}
