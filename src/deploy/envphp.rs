//! Per-release `app/etc/env.php`: the host's template with every cache
//! frontend's `id_prefix` rolled to `r<release>_`, so the incoming release
//! warms its own namespace while the outgoing one keeps serving from its own.

use std::path::Path;

use anyhow::{Context, Result, anyhow};
use magequery_core::phparray::{self, PhpValue};

/// Write `target` from `template`, rolling the cache prefix when asked.
pub fn derive(template: &Path, target: &Path, release: &str, roll_prefix: bool) -> Result<()> {
    let src = std::fs::read_to_string(template)
        .with_context(|| format!("reading the env.php template {}", template.display()))?;
    let mut env =
        phparray::parse(&src).map_err(|e| anyhow!("parsing {}: {e}", template.display()))?;
    if roll_prefix {
        set_cache_prefix(&mut env, &prefix_for(release));
    }
    if let Some(dir) = target.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Replace whatever the artifact shipped (it should ship none) without
    // following a symlink, then make it read-only like the template.
    let _ = std::fs::remove_file(target);
    std::fs::write(target, env.to_php_file())?;
    set_mode(target, 0o440)?;
    Ok(())
}

pub fn prefix_for(release: &str) -> String {
    format!("r{release}_")
}

fn set_cache_prefix(env: &mut PhpValue, prefix: &str) {
    let Some(PhpValue::Array(frontends)) = env.get_mut("cache").and_then(|c| c.get_mut("frontend"))
    else {
        return;
    };
    for (_, fe) in frontends.iter_mut() {
        if matches!(fe, PhpValue::Array(_)) {
            fe.set("id_prefix", PhpValue::Str(prefix.to_string()));
        }
    }
}

/// A Redis-backed cache frontend, as an env.php configures it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedisCache {
    pub frontend: String,
    pub server: String,
    pub port: u16,
    pub database: u32,
    pub password: Option<String>,
    pub prefix: String,
}

/// The Redis cache frontends of an env.php that have an `id_prefix`.
/// Frontends on other backends (files, Varnish for the page cache) are skipped.
pub fn redis_caches(env_php: &Path) -> Result<Vec<RedisCache>> {
    let src = std::fs::read_to_string(env_php)
        .with_context(|| format!("reading {}", env_php.display()))?;
    let env = phparray::parse(&src).map_err(|e| anyhow!("parsing {}: {e}", env_php.display()))?;
    let Some(frontends) = env
        .get("cache")
        .and_then(|c| c.get("frontend"))
        .and_then(PhpValue::as_array)
    else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (name, fe) in frontends {
        let backend = fe
            .get("backend")
            .and_then(PhpValue::as_str)
            .unwrap_or_default();
        if !backend.to_ascii_lowercase().contains("redis") {
            continue;
        }
        let Some(prefix) = fe
            .get("id_prefix")
            .and_then(PhpValue::scalar_string)
            .filter(|p| !p.is_empty())
        else {
            continue;
        };
        let opts = fe.get("backend_options");
        let opt = |k: &str| {
            opts.and_then(|o| o.get(k))
                .and_then(PhpValue::scalar_string)
        };
        out.push(RedisCache {
            frontend: name.scalar_string().unwrap_or_default(),
            server: opt("server").unwrap_or_else(|| "127.0.0.1".into()),
            port: opt("port").and_then(|p| p.parse().ok()).unwrap_or(6379),
            database: opt("database").and_then(|d| d.parse().ok()).unwrap_or(0),
            password: opt("password").filter(|p| !p.is_empty()),
            prefix,
        });
    }
    Ok(out)
}

fn set_mode(p: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEMPLATE: &str = r#"<?php
return [
    'crypt' => ['key' => 'secret'],
    'cache' => [
        'frontend' => [
            'default' => [
                'id_prefix' => '69d_',
                'backend' => 'Magento\\Framework\\Cache\\Backend\\Redis',
                'backend_options' => ['server' => '127.0.0.1', 'port' => '6379', 'database' => '0'],
            ],
            'page_cache' => [
                'backend' => 'Magento\\Framework\\Cache\\Backend\\Redis',
                'backend_options' => ['server' => '/run/redis.sock', 'database' => '1', 'password' => 'pw'],
            ],
        ],
    ],
    'session' => ['save' => 'redis', 'redis' => ['database' => '2']],
];
"#;

    #[test]
    fn derive_rolls_every_cache_frontends_prefix_and_keeps_the_rest() {
        let dir = tempdir();
        let tpl = dir.join("tpl.php");
        std::fs::write(&tpl, TEMPLATE).unwrap();
        let out = dir.join("app/etc/env.php");
        derive(&tpl, &out, "36", true).unwrap();

        let caches = redis_caches(&out).unwrap();
        assert_eq!(caches.len(), 2);
        assert!(caches.iter().all(|c| c.prefix == "r36_"));
        assert_eq!(caches[0].database, 0);
        assert_eq!(caches[1].server, "/run/redis.sock");
        assert_eq!(caches[1].password.as_deref(), Some("pw"));

        let written = phparray::parse(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(
            written
                .get("crypt")
                .and_then(|c| c.get("key"))
                .and_then(PhpValue::as_str),
            Some("secret")
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&out).unwrap().permissions().mode() & 0o777,
            0o440
        );
    }

    #[test]
    fn derive_without_rolling_copies_the_template() {
        let dir = tempdir();
        let tpl = dir.join("tpl.php");
        std::fs::write(&tpl, TEMPLATE).unwrap();
        let out = dir.join("env.php");
        derive(&tpl, &out, "36", false).unwrap();
        assert_eq!(
            phparray::parse(&std::fs::read_to_string(&out).unwrap()).unwrap(),
            phparray::parse(TEMPLATE).unwrap()
        );
    }

    fn tempdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!(
            "magebuild-envphp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }
}
