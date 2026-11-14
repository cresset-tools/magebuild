//! `magebuild theme-status` and `magebuild queue-status`: what `setup:upgrade`
//! registers besides schema and patches, which `setup:db:status` does not look
//! at. Native versions of Mage-OS's `theme:registration:status` and
//! `queue:config:status`, with their exit codes: 0 when nothing is missing, 2
//! when `setup:upgrade` has something to register.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use magequery_core::Magento;

const EXIT_MISSING: u8 = 2;

pub fn themes(root: &Path, json: bool) -> Result<ExitCode> {
    let mage = Magento::open(root).map_err(|e| anyhow!(e))?;
    let missing = mage.unregistered_themes().map_err(|e| anyhow!(e))?;
    if json {
        let registered: Vec<String> = mage
            .registered_themes()
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let out = serde_json::json!({ "themes": registered, "unregistered": missing });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if missing.is_empty() {
        println!("All themes are registered.");
    } else {
        for t in &missing {
            println!("{t}: not registered");
        }
    }
    Ok(exit(missing.is_empty()))
}

/// Database queues only: RabbitMQ's state lives on the broker, so for it this
/// prints the topology fingerprint `magebuild deploy` compares between releases.
pub fn queues(root: &Path, json: bool) -> Result<ExitCode> {
    let mage = Magento::open(root).map_err(|e| anyhow!(e))?;
    let missing = mage.missing_db_queues().map_err(|e| anyhow!(e))?;
    let fingerprint = mage.amqp_topology_fingerprint().map_err(|e| anyhow!(e))?;
    if json {
        let configured = mage.topology_queues().map_err(|e| anyhow!(e))?;
        let out = serde_json::json!({
            "queues": configured,
            "missing": missing,
            "amqp_topology_fingerprint": fingerprint,
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        if missing.is_empty() {
            println!("All queues are in the queue table.");
        }
        for q in &missing {
            println!("{q}: not in the queue table");
        }
        if let Some(f) = &fingerprint {
            println!("AMQP topology fingerprint: {f}");
        }
    }
    Ok(exit(missing.is_empty()))
}

fn exit(ok: bool) -> ExitCode {
    if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_MISSING)
    }
}
