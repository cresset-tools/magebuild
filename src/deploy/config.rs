//! `[deploy]` in `magebuild.toml`: where a release goes and what the agent does
//! on the host. The runner reads it and sends it to the agent as part of the
//! plan, so the host needs no copy of this file.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The shared directories of Deployer's magento2 recipe.
const DEFAULT_SHARED_DIRS: &[&str] = &[
    "var/composer_home",
    "var/log",
    "var/export",
    "var/report",
    "var/import",
    "var/import_history",
    "var/session",
    "var/importexport",
    "var/backups",
    "var/tmp",
    "pub/sitemap",
    "pub/media",
    "pub/static/_cache",
];

/// The writable directories of Deployer's magento2 recipe.
const DEFAULT_WRITABLE_DIRS: &[&str] = &[
    "var",
    "pub/static",
    "pub/media",
    "generated",
    "var/page_cache",
];

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploySection {
    /// `user@host` to deploy to, as `ssh` takes it.
    pub host: String,
    /// The deploy root on the host: `releases/`, `shared/`, `current`, `.dep/`.
    pub path: String,
    /// Releases to keep, the live one included.
    #[serde(default = "default_keep")]
    pub keep_releases: usize,
    /// The PHP binary for the few `bin/magento` commands that still run.
    #[serde(default = "default_php")]
    pub php: String,
    /// Directories linked from `shared/` into every release.
    #[serde(default = "default_shared_dirs")]
    pub shared_dirs: Vec<String>,
    /// Files linked from `shared/` into every release (created empty if missing).
    #[serde(default = "default_shared_files")]
    pub shared_files: Vec<String>,
    /// Extra absolute-target links: release path → path relative to the deploy
    /// root, e.g. `"auth.json" = "shared/auth.json"`. Unlike `shared_files`
    /// the target is never created or touched.
    #[serde(default)]
    pub links: BTreeMap<String, String>,
    /// Directories created in every release and given `writable_mode`.
    #[serde(default = "default_writable_dirs")]
    pub writable_dirs: Vec<String>,
    /// Octal mode for `writable_dirs`.
    #[serde(default = "default_writable_mode")]
    pub writable_mode: String,
    /// Derive each release's `app/etc/env.php` from a template on the host.
    pub env_php: Option<EnvPhp>,
    /// How to pause the store's cron and queue consumers during the deploy.
    #[serde(default)]
    pub cron: Cron,
    /// Reset opcache over FastCGI after the swap.
    pub opcache: Option<Opcache>,
    /// Run `setup:db:status` / `app:config:status` checks and the upgrade or
    /// import they call for. `check` only reports what would run.
    #[serde(default)]
    pub upgrade: UpgradeMode,
}

/// Derive env.php from `source` (relative to the deploy root), rolling the cache
/// `id_prefix` per release so the outgoing release keeps its warm cache until
/// the swap. The old prefix's Redis keys are deleted after the swap.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvPhp {
    pub source: String,
    /// Roll `cache/frontend/*/id_prefix` to `r<release>_`.
    #[serde(default = "yes")]
    pub roll_cache_prefix: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields, tag = "backend", rename_all = "lowercase")]
pub enum Cron {
    /// Leave cron alone.
    #[default]
    None,
    /// systemd units, stopped and started one `systemctl` call per unit (a
    /// polkit rule scoped to units cannot match a batched call). `stop` runs in
    /// order before the deploy's database work, `start` after the swap.
    Systemd {
        stop: Vec<String>,
        start: Vec<String>,
        /// Also SIGTERM this user's `bin/magento cron:run` and
        /// `queue:consumers:start` processes after stopping the units.
        #[serde(default = "yes")]
        kill_stragglers: bool,
    },
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Opcache {
    /// php-fpm's FastCGI address: `host:port` or a unix socket path.
    pub fastcgi: String,
    /// A directory php-fpm sees at the same path, for the one-line reset script.
    pub tmp_dir: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum UpgradeMode {
    #[default]
    Auto,
    Check,
}

fn default_keep() -> usize {
    5
}
fn default_php() -> String {
    "php".into()
}
fn default_shared_dirs() -> Vec<String> {
    DEFAULT_SHARED_DIRS.iter().map(|s| s.to_string()).collect()
}
fn default_shared_files() -> Vec<String> {
    vec!["app/etc/env.php".into(), "var/.maintenance.ip".into()]
}
fn default_writable_dirs() -> Vec<String> {
    DEFAULT_WRITABLE_DIRS
        .iter()
        .map(|s| s.to_string())
        .collect()
}
fn default_writable_mode() -> String {
    "0755".into()
}
fn yes() -> bool {
    true
}

impl DeploySection {
    /// Reject combinations the agent would only trip over on the host.
    pub fn validate(&self) -> anyhow::Result<()> {
        use anyhow::bail;
        if !self.path.starts_with('/') {
            bail!("[deploy] path must be absolute, got `{}`", self.path);
        }
        if self.keep_releases == 0 {
            bail!("[deploy] keep_releases must be at least 1");
        }
        u32::from_str_radix(self.writable_mode.trim_start_matches("0o"), 8).map_err(|_| {
            anyhow::anyhow!(
                "[deploy] writable_mode `{}` is not octal",
                self.writable_mode
            )
        })?;
        if self.env_php.is_some() && self.shared_files.iter().any(|f| f == "app/etc/env.php") {
            bail!(
                "[deploy] env.php is both derived ([deploy.env_php]) and shared (shared_files); \
                 drop app/etc/env.php from shared_files"
            );
        }
        for p in self
            .shared_dirs
            .iter()
            .chain(&self.shared_files)
            .chain(&self.writable_dirs)
            .chain(self.links.keys())
        {
            if p.starts_with('/') || p.split('/').any(|c| c == "..") {
                bail!("[deploy] `{p}` must be a relative path inside the release");
            }
        }
        Ok(())
    }

    pub fn writable_mode_bits(&self) -> u32 {
        u32::from_str_radix(self.writable_mode.trim_start_matches("0o"), 8).unwrap_or(0o755)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> DeploySection {
        toml::from_str(s).unwrap()
    }

    #[test]
    fn defaults_follow_deployers_magento_recipe() {
        let d = parse("host = 'm@h'\npath = '/srv/shop'");
        assert_eq!(d.keep_releases, 5);
        assert!(d.shared_dirs.contains(&"pub/media".to_string()));
        assert_eq!(d.shared_files, ["app/etc/env.php", "var/.maintenance.ip"]);
        assert!(matches!(d.cron, Cron::None));
        assert_eq!(d.upgrade, UpgradeMode::Auto);
        d.validate().unwrap();
    }

    #[test]
    fn systemd_cron_and_derived_env() {
        let d = parse(
            "host = 'm@h'\npath = '/srv/shop'\nshared_files = ['var/.maintenance.ip']\n\
             [env_php]\nsource = 'shared/app/etc/env.php'\n\
             [cron]\nbackend = 'systemd'\nstop = ['a.timer', 'a.service']\nstart = ['a.timer']",
        );
        d.validate().unwrap();
        let Cron::Systemd {
            stop,
            kill_stragglers,
            ..
        } = &d.cron
        else {
            panic!()
        };
        assert_eq!(stop, &["a.timer", "a.service"]);
        assert!(kill_stragglers);
        assert!(d.env_php.as_ref().unwrap().roll_cache_prefix);
    }

    #[test]
    fn derived_env_php_cannot_also_be_shared() {
        let d =
            parse("host = 'm@h'\npath = '/srv/shop'\n[env_php]\nsource = 'shared/app/etc/env.php'");
        assert!(d.validate().is_err());
    }

    #[test]
    fn paths_must_stay_inside_the_release() {
        let d = parse("host = 'm@h'\npath = '/srv/shop'\nshared_dirs = ['../etc']");
        assert!(d.validate().is_err());
    }
}
