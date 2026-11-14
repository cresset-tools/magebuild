//! `magebuild config-status` — a native `bin/magento app:config:status`, so a
//! deploy can gate `app:config:import` without bootstrapping Magento. The check
//! itself is magequery-core's `Magento::config_status()`; this is the CLI
//! around it.

use std::path::Path;
use std::process::ExitCode;

use anyhow::{Result, anyhow};
use magequery_core::Magento;

/// `ConfigStatusCommand::EXIT_CODE_CONFIG_IMPORT_REQUIRED`.
const EXIT_IMPORT_REQUIRED: u8 = 2;

/// Exits 0 when the config is up to date, 2 when an import is needed, and 1 on
/// an error, including a config this port can't hash exactly (run
/// `bin/magento app:config:status` then).
pub fn run(root: &Path, json: bool) -> Result<ExitCode> {
    let mage = Magento::open(root).map_err(|e| anyhow!(e))?;
    let status = mage.config_status().map_err(|e| anyhow!(e))?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        for s in status.sections.iter().filter(|s| s.changed) {
            println!("{}: changed since the last import", s.name);
        }
        if status.import_required() {
            println!(
                "Config files have changed. Run app:config:import or setup:upgrade command to synchronize configuration."
            );
        } else {
            println!("Config files are up to date.");
        }
    }
    Ok(if status.import_required() {
        ExitCode::from(EXIT_IMPORT_REQUIRED)
    } else {
        ExitCode::SUCCESS
    })
}
