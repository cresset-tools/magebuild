//! `magebuild --report <file.json>`: each node's state, duration and
//! diagnostics as data, so CI can gate on a build and annotate the pull
//! request without parsing the progress output.
//!
//! The fixtures are bare Magento roots that only di-compile runs on
//! (`node di-compile --skip composer-install`).

#![cfg(not(windows))]

use std::path::PathBuf;
use std::process::{Command, Output};

use serde_json::Value;

struct Store(PathBuf);

impl Store {
    /// A module with an interface, a class implementing it, and the di.xml
    /// given (inside `<config>`).
    fn new(name: &str, di: &str) -> Self {
        let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
        let _ = std::fs::remove_dir_all(&root);
        let store = Store(root);
        store.write(
            "app/etc/config.php",
            "<?php\nreturn ['modules' => ['Acme_Probe' => 1]];\n",
        );
        store.write(
            "app/code/Acme/Probe/etc/module.xml",
            "<?xml version=\"1.0\"?>\n<config><module name=\"Acme_Probe\"/></config>\n",
        );
        store.write(
            "app/code/Acme/Probe/Model/ApiInterface.php",
            "<?php\nnamespace Acme\\Probe\\Model;\n\ninterface ApiInterface\n{\n}\n",
        );
        store.write(
            "app/code/Acme/Probe/Model/Real.php",
            "<?php\nnamespace Acme\\Probe\\Model;\n\nclass Real implements ApiInterface\n{\n}\n",
        );
        store.write(
            "app/code/Acme/Probe/etc/di.xml",
            &format!("<?xml version=\"1.0\"?>\n<config>\n{di}\n</config>\n"),
        );
        store.write("magebuild.toml", "");
        store
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.0.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Build di-compile alone, with a report; the exit status and the report.
    fn build(&self) -> (Output, Value) {
        let report = self.0.join("report.json");
        let out = Command::new(env!("CARGO_BIN_EXE_magebuild"))
            .arg("--root")
            .arg(&self.0)
            .arg("--report")
            .arg(&report)
            .args(["--skip", "composer-install", "node", "di-compile"])
            .output()
            .expect("run magebuild");
        let text = std::fs::read_to_string(&report).unwrap_or_else(|e| {
            panic!("no report ({e}):\n{}", String::from_utf8_lossy(&out.stderr))
        });
        (
            out,
            serde_json::from_str(&text).expect("the report is JSON"),
        )
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn node<'a>(report: &'a Value, id: &str) -> &'a Value {
    report["nodes"]
        .as_array()
        .expect("nodes")
        .iter()
        .find(|n| n["id"] == id)
        .unwrap_or_else(|| panic!("no node {id} in {report:#}"))
}

fn diagnostics<'a>(node: &'a Value, kind: &str) -> Vec<&'a Value> {
    node["diagnostics"]
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter(|d| d["kind"] == kind)
        .collect()
}

#[test]
fn a_clean_build_reports_every_node() {
    let store = Store::new(
        "mb-report-clean",
        r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Real"/>"#,
    );
    let (out, report) = store.build();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(report["version"], 1);
    assert_eq!(report["failed"], false);
    assert!(report["wall_ms"].is_u64());
    let di = node(&report, "di-compile");
    assert_eq!(di["state"], "done");
    assert!(di["duration_ms"].is_u64());
    assert!(di.get("error").is_none());
    assert_eq!(node(&report, "composer-install")["state"], "skipped");
}

#[test]
fn a_refused_compile_reports_where_and_why() {
    let store = Store::new(
        "mb-report-refused",
        r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Gone"/>"#,
    );
    let (out, report) = store.build();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(report["failed"], true);
    let di = node(&report, "di-compile");
    assert_eq!(di["state"], "failed");
    assert!(
        di["error"]
            .as_str()
            .is_some_and(|e| e.contains("setup:di:compile")),
        "{di:#}"
    );
    let refused = diagnostics(di, "rejected");
    assert_eq!(refused.len(), 1, "{di:#}");
    let r = refused[0];
    assert_eq!(r["severity"], "error");
    assert_eq!(r["code"], "preference-missing");
    assert_eq!(r["file"], "app/code/Acme/Probe/etc/di.xml");
    assert_eq!(r["line"], 3);
    assert!(
        r["message"]
            .as_str()
            .unwrap()
            .starts_with("Preference declared for"),
        "{r:#}"
    );
}

/// A type whose di.xml arguments are lost: it extends a missing class, so
/// its constructor cannot be read.
const LOST: &str = r#"<type name="Acme\Probe\Model\Child"><arguments><argument name="x" xsi:type="string">y</argument></arguments></type>"#;

fn lost_store(name: &str) -> Store {
    let store = Store::new(name, LOST);
    store.write(
        "app/code/Acme/Probe/Model/Child.php",
        "<?php\nnamespace Acme\\Probe\\Model;\n\nclass Child extends \\Missing\\Base\n{\n}\n",
    );
    store
}

#[test]
fn lost_arguments_are_warnings_unless_they_fail_the_build() {
    let store = lost_store("mb-report-lost");
    let (out, report) = store.build();
    assert!(out.status.success());
    let lost = diagnostics(node(&report, "di-compile"), "lost-arguments");
    assert_eq!(lost.len(), 1);
    assert_eq!(lost[0]["severity"], "warning");
    assert_eq!(lost[0]["subject"], "Acme\\Probe\\Model\\Child");

    store.write(
        "magebuild.toml",
        "[nodes.di-compile]\nfail_on = [\"lost-arguments\"]\n",
    );
    let (out, report) = store.build();
    assert_eq!(out.status.code(), Some(1));
    let di = node(&report, "di-compile");
    assert_eq!(di["state"], "failed");
    assert_eq!(
        diagnostics(di, "lost-arguments")[0]["severity"],
        "error",
        "what fails the build is an error"
    );
}

#[test]
fn doctor_findings_carry_their_code_and_location() {
    let store = Store::new(
        "mb-report-doctor",
        r#"<preference for="Acme\Probe\Model\ApiInterface" type="Acme\Probe\Model\Real"/>
<type name="Acme\Probe\Model\Real"><arguments><argument name="dep" xsi:type="object">Acme\Probe\Model\Gone</argument></arguments></type>"#,
    );
    store.write("magebuild.toml", "[doctor]\n");
    let (_, report) = store.build();
    let found = diagnostics(node(&report, "di-compile"), "doctor");
    let gone = found
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .is_some_and(|m| m.contains("Acme\\Probe\\Model\\Gone"))
        })
        .unwrap_or_else(|| panic!("{report:#}"));
    assert_eq!(gone["code"], "E2006");
    assert_eq!(gone["file"], "app/code/Acme/Probe/etc/di.xml");
    assert_eq!(gone["line"], 4);
    assert!(matches!(
        gone["severity"].as_str(),
        Some("error" | "warning" | "info")
    ));
}

#[test]
fn the_report_is_written_where_asked_even_when_the_graph_is_bad() {
    let store = Store::new("mb-report-badgraph", "");
    store.write(
        "magebuild.toml",
        "[nodes.di-compile]\nfail_on = [\"no-such-kind\"]\n",
    );
    let report = store.0.join("report.json");
    let out = Command::new(env!("CARGO_BIN_EXE_magebuild"))
        .arg("--root")
        .arg(&store.0)
        .arg("--report")
        .arg(&report)
        .args(["node", "di-compile"])
        .output()
        .expect("run magebuild");
    assert_eq!(out.status.code(), Some(1));
    let report: Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("a report")).unwrap();
    assert_eq!(report["failed"], true);
    assert!(
        report["error"]
            .as_str()
            .is_some_and(|e| e.contains("no-such-kind")),
        "{report:#}"
    );
    assert_eq!(report["nodes"], serde_json::json!([]));
}
