//! `magebuild db-status` — a native `bin/magento setup:db:status`, so a deploy can
//! gate `setup:upgrade` without bootstrapping Magento. The check itself is
//! magequery-core's `Magento::db_status()`; this is the CLI around it.

use std::fmt::Write as _;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use magequery_core::Magento;
use magequery_core::model::DbStatus;

/// `setup:db:status`'s "upgrade required" exit code
/// (`DbStatusCommand::EXIT_CODE_UPGRADE_REQUIRED`). Deployer's magento2 recipe
/// upgrades on exactly this code and fails the deploy on any other non-zero one.
const EXIT_UPGRADE_REQUIRED: u8 = 2;

/// Exits 0 when up to date, 2 when an upgrade is required (including an
/// unreachable database, as Magento does), and 1 on an error such as a store
/// with no install date.
pub fn run(root: &Path, json: bool) -> Result<ExitCode> {
    let mage = Magento::open(root).map_err(|e| anyhow!(e))?;
    let status = mage.db_status().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        for n in &status.notes {
            eprintln!("note: {n}");
        }
        print!("{}", render(&status, mage.root()));
    }
    Ok(if status.is_up_to_date() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_UPGRADE_REQUIRED)
    })
}

fn render(status: &DbStatus, root: &Path) -> String {
    let mut out = String::new();
    let rel = |p: &Path| p.strip_prefix(root).unwrap_or(p).display().to_string();

    if !status.schema_changes.is_empty() {
        let _ = writeln!(
            out,
            "Declarative schema is not up to date ({})",
            status.schema_changes.len()
        );
        for c in &status.schema_changes {
            let target = match &c.element {
                Some(e) => format!("{}.{e}", c.table),
                None => c.table.clone(),
            };
            let _ = writeln!(out, "  {:<20} {target}", c.operation);
            for d in &c.detail {
                let _ = writeln!(out, "  {:<20} {d}", "");
            }
        }
        out.push('\n');
    }
    for (title, patches) in [
        ("Schema patches are not up to date", &status.schema_patches),
        ("Data patches are not up to date", &status.data_patches),
    ] {
        if patches.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{title} ({})", patches.len());
        for p in patches {
            let _ = writeln!(out, "  {}  # {}", p.class.as_str(), rel(&p.file));
        }
        out.push('\n');
    }
    if !status.module_versions.is_empty() {
        let _ = writeln!(
            out,
            "Module versions differ from setup_module ({})",
            status.module_versions.len()
        );
        for v in &status.module_versions {
            let _ = writeln!(
                out,
                "  {}  {:<6}  {} → {}",
                v.module.as_str(),
                v.kind.to_string(),
                v.current.as_deref().unwrap_or("none"),
                v.required,
            );
        }
        out.push('\n');
    }
    for e in &status.errors {
        let _ = writeln!(out, "check failed: {e}");
    }
    if status.is_up_to_date() {
        out.push_str("All modules are up to date.\n");
    } else {
        out.push_str("Run 'setup:upgrade' to update your DB schema and data.\n");
    }
    out
}
