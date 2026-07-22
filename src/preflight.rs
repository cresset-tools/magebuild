//! Cheap, doctor-style pre-flight checks run before a real build (never for
//! `--dry-run`). Fail fast on a non-Magento root; warn on the DB-less-build
//! contract gaps (`app/etc/config.php` missing the dumped `scopes`/`themes`).

use std::path::Path;

use anyhow::{Result, bail};

/// Returns warnings (never fatal beyond the root check). `installs_vendor` is
/// true when the resolved graph has an active composer-install node — an absent
/// `vendor/` is then the normal starting state (it's about to be created), so
/// it isn't worth a warning.
pub fn check(root: &Path, installs_vendor: bool) -> Result<Vec<String>> {
    if magequery_core::Magento::find_root(root).is_none() {
        bail!(
            "no Magento root found at or above {} (need app/etc/config.php)",
            root.display()
        );
    }
    let mut warnings = Vec::new();
    let config_php = root.join("app/etc/config.php");
    if let Ok(text) = std::fs::read_to_string(&config_php)
        && !text.contains("'scopes'")
        && !text.contains("\"scopes\"")
    {
        warnings.push(
            "app/etc/config.php has no `scopes` node — a DB-less static-content \
             deploy needs `bin/magento app:config:dump scopes themes` committed"
                .to_string(),
        );
    }
    // Only warn about a missing vendor/ when nothing in this run installs it.
    if !installs_vendor && !root.join("vendor").is_dir() {
        warnings.push(
            "vendor/ is absent and composer-install is not in this run — native \
             di-compile/static-deploy need it; install dependencies first"
                .to_string(),
        );
    }
    Ok(warnings)
}
