//! Cheap, doctor-style pre-flight checks run before a real build (never for
//! `--dry-run`). Fail fast on a non-Magento root; warn on the DB-less-build
//! contract gaps (`app/etc/config.php` missing the dumped `scopes`/`themes`).

use std::path::Path;

use anyhow::{Result, bail};

/// Returns warnings (never fatal beyond the root check).
pub fn check(root: &Path) -> Result<Vec<String>> {
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
    if !root.join("vendor").is_dir() {
        warnings.push(
            "vendor/ is absent — native di-compile/static-deploy need it; \
             composer-install must run first"
                .to_string(),
        );
    }
    Ok(warnings)
}
