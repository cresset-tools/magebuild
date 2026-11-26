//! `--report <file.json>`: the run as data. Each node's state, duration and
//! the diagnostics its step found, so CI can gate on a build and annotate a
//! pull request without parsing the progress output.
//!
//! ```json
//! { "version": 1, "failed": true, "wall_ms": 812,
//!   "nodes": [ { "id": "di-compile", "state": "failed", "error": "…", "duration_ms": 640,
//!                "diagnostics": [ { "kind": "rejected", "severity": "error",
//!                                   "code": "preference-missing", "message": "…",
//!                                   "file": "app/code/Acme/Probe/etc/di.xml", "line": 3 } ] } ] }
//! ```
//!
//! A run that fails before any node starts (a bad magebuild.toml, a root that
//! is not Magento) still writes a report, with `error` at the top and no
//! nodes, so a CI step that reads it never finds it missing.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::json::Json;
use crate::scheduler::{NodeState, RunReport};

/// The report's format; raised when a field changes meaning or goes away.
pub const VERSION: u128 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Failed the build, or would have stopped Magento's own command.
    Error,
    Warning,
    Info,
}

impl Severity {
    fn name(self) -> &'static str {
        match self {
            Severity::Error => "error",
            Severity::Warning => "warning",
            Severity::Info => "info",
        }
    }

    /// An error when `gated` (a `fail_on` kind that fails the build), a
    /// warning otherwise.
    pub fn warning_unless(gated: bool) -> Severity {
        if gated {
            Severity::Error
        } else {
            Severity::Warning
        }
    }
}

/// One thing a step found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// What it is, stable across releases: `rejected`, `lost-arguments`,
    /// `compile-finding`, `doctor`, `doctor-baseline` (di-compile);
    /// `not-deployed`, `skipped-theme`, `less-warning` (static-deploy).
    pub kind: &'static str,
    pub severity: Severity,
    pub message: String,
    /// A finer kind: the doctor lint code, or the rejection's kind.
    pub code: Option<String>,
    /// The class, module or theme it is about.
    pub subject: Option<String>,
    /// Root-relative, or a theme's logical path for static-deploy.
    pub file: Option<String>,
    /// 1-based; 0 when unknown.
    pub line: u32,
    /// The areas it applies to; empty when all, or when areas don't apply.
    pub areas: Vec<String>,
    pub theme: Option<String>,
    pub locale: Option<String>,
}

impl Diagnostic {
    pub fn new(kind: &'static str, severity: Severity, message: impl Into<String>) -> Self {
        Diagnostic {
            kind,
            severity,
            message: message.into(),
            code: None,
            subject: None,
            file: None,
            line: 0,
            areas: Vec::new(),
            theme: None,
            locale: None,
        }
    }

    fn to_json(&self) -> Json {
        let mut fields = vec![
            ("kind".into(), Json::s(self.kind)),
            ("severity".into(), Json::s(self.severity.name())),
        ];
        let mut opt = |key: &str, value: &Option<String>| {
            if let Some(v) = value {
                fields.push((key.into(), Json::s(v)));
            }
        };
        opt("code", &self.code);
        opt("subject", &self.subject);
        opt("file", &self.file);
        if self.line > 0 {
            fields.push(("line".into(), Json::Num(self.line.into())));
        }
        if !self.areas.is_empty() {
            fields.push((
                "areas".into(),
                Json::Arr(self.areas.iter().map(Json::s).collect()),
            ));
        }
        let mut opt = |key: &str, value: &Option<String>| {
            if let Some(v) = value {
                fields.push((key.into(), Json::s(v)));
            }
        };
        opt("theme", &self.theme);
        opt("locale", &self.locale);
        fields.push(("message".into(), Json::s(&self.message)));
        Json::Obj(fields)
    }
}

/// What every node has found so far, by node id. Steps run in parallel, so
/// each records through the lock.
#[derive(Debug, Default)]
pub struct Diagnostics(Mutex<BTreeMap<String, Vec<Diagnostic>>>);

impl Diagnostics {
    pub fn push(&self, node: &str, diagnostic: Diagnostic) {
        let mut map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(node.to_owned()).or_default().push(diagnostic);
    }

    pub fn of(&self, node: &str) -> Vec<Diagnostic> {
        let map = self.0.lock().unwrap_or_else(|e| e.into_inner());
        map.get(node).cloned().unwrap_or_default()
    }
}

/// The report of a run that got as far as the scheduler.
pub fn of_run(report: &RunReport, diagnostics: &Diagnostics) -> Json {
    let nodes = report
        .outcomes
        .iter()
        .map(|o| {
            let (state, error) = match &o.state {
                NodeState::Done => ("done", None),
                NodeState::Skipped => ("skipped", None),
                NodeState::Cancelled => ("cancelled", None),
                NodeState::Failed(m) => ("failed", Some(m)),
            };
            let mut fields = vec![
                ("id".into(), Json::s(&o.id)),
                ("state".into(), Json::s(state)),
            ];
            if let Some(e) = error {
                fields.push(("error".into(), Json::s(e)));
            }
            fields.push(("duration_ms".into(), Json::Num(o.duration.as_millis())));
            fields.push((
                "diagnostics".into(),
                Json::Arr(
                    diagnostics
                        .of(&o.id)
                        .iter()
                        .map(Diagnostic::to_json)
                        .collect(),
                ),
            ));
            Json::Obj(fields)
        })
        .collect();
    Json::Obj(vec![
        ("version".into(), Json::Num(VERSION)),
        ("failed".into(), Json::Bool(report.failed)),
        ("wall_ms".into(), Json::Num(report.wall.as_millis())),
        ("nodes".into(), Json::Arr(nodes)),
    ])
}

/// The report of a run that failed before any node started.
pub fn of_error(error: &str) -> Json {
    Json::Obj(vec![
        ("version".into(), Json::Num(VERSION)),
        ("failed".into(), Json::Bool(true)),
        ("error".into(), Json::s(error)),
        ("wall_ms".into(), Json::Num(0)),
        ("nodes".into(), Json::Arr(Vec::new())),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_fields_are_left_out() {
        let d = Diagnostic::new("compile-finding", Severity::Info, "x");
        let text = d.to_json().to_pretty();
        assert!(text.contains("\"kind\": \"compile-finding\""));
        assert!(!text.contains("\"line\""), "{text}");
        assert!(!text.contains("\"areas\""), "{text}");
        assert!(!text.contains("\"file\""), "{text}");
    }

    #[test]
    fn a_static_deploy_diagnostic_names_its_package() {
        let mut d = Diagnostic::new(
            "not-deployed",
            Severity::Error,
            "entry css/styles-m.less: …",
        );
        d.file = Some("css/styles-m.css".into());
        d.areas = vec!["frontend".into()];
        d.theme = Some("Acme/shop".into());
        d.locale = Some("en_US".into());
        let text = d.to_json().to_pretty();
        for want in [
            "\"severity\": \"error\"",
            "\"areas\": [\n    \"frontend\"\n  ]",
            "\"theme\": \"Acme/shop\"",
            "\"locale\": \"en_US\"",
        ] {
            assert!(text.contains(want), "{want} in {text}");
        }
    }

    #[test]
    fn diagnostics_are_kept_per_node() {
        let all = Diagnostics::default();
        all.push("a", Diagnostic::new("x", Severity::Warning, "1"));
        all.push("b", Diagnostic::new("x", Severity::Warning, "2"));
        all.push("a", Diagnostic::new("x", Severity::Warning, "3"));
        let a: Vec<String> = all.of("a").into_iter().map(|d| d.message).collect();
        assert_eq!(a, ["1", "3"]);
        assert!(all.of("c").is_empty());
    }
}
