//! `magebuild agent`: the host side, started by the runner over ssh. Speaks
//! the protocol on stdin/stdout; everything human-readable goes to stderr,
//! which the runner shows as is.
//!
//! The layout is Deployer's, so a Deployer deploy still works as a fallback:
//! `releases/<N>`, `shared/`, a relative `current` symlink, and
//! `.dep/latest_release` + `.dep/releases_log`.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};

use super::config::{DeploySection, UpgradeMode};
use super::protocol::{self, ArtifactFormat, Event, PROTOCOL, Plan};
use super::{cron, envphp, fcgi, redis};

/// A child process's stdout, pointed at our stderr: stdout is the protocol.
pub fn stderr_stdio() -> Stdio {
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

pub fn run() -> ExitCode {
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut out = std::io::stdout().lock();
    match serve(&mut input, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agent: {e:#}");
            let _ = protocol::send(
                &mut out,
                &Event::Failed {
                    message: format!("{e:#}"),
                },
            );
            ExitCode::FAILURE
        }
    }
}

fn serve(input: &mut impl BufRead, out: &mut impl Write) -> Result<()> {
    protocol::send(
        out,
        &Event::Hello {
            protocol: PROTOCOL,
            version: env!("CARGO_PKG_VERSION").into(),
            sha256: own_sha256()?,
        },
    )?;
    let plan: Plan =
        protocol::recv(input)?.context("the runner closed the session before sending a plan")?;
    if plan.protocol != PROTOCOL {
        bail!(
            "runner speaks protocol {}, this agent speaks {PROTOCOL}",
            plan.protocol
        );
    }
    plan.deploy.validate()?;
    Deploy::new(plan, out)?.run(input)
}

fn own_sha256() -> Result<String> {
    let exe = fs::read("/proc/self/exe").context("reading /proc/self/exe")?;
    Ok(super::runner::hex(&Sha256::digest(&exe)))
}

struct Deploy<'o, W: Write> {
    plan: Plan,
    out: &'o mut W,
    root: PathBuf,
    started: Instant,
    // What to undo if a step fails before the swap.
    release: Option<(String, PathBuf)>,
    cron_paused: bool,
    maintenance_on: bool,
    _lock: Option<File>,
}

impl<'o, W: Write> Deploy<'o, W> {
    fn new(plan: Plan, out: &'o mut W) -> Result<Self> {
        let root = PathBuf::from(&plan.deploy.path);
        Ok(Deploy {
            plan,
            out,
            root,
            started: Instant::now(),
            release: None,
            cron_paused: false,
            maintenance_on: false,
            _lock: None,
        })
    }

    fn d(&self) -> &DeploySection {
        &self.plan.deploy
    }

    fn step(&mut self, name: &str, since: Instant, detail: Option<String>) -> Result<()> {
        protocol::send(
            self.out,
            &Event::Step {
                name: name.into(),
                ms: since.elapsed().as_millis() as u64,
                detail,
            },
        )
    }

    fn run(mut self, input: &mut impl BufRead) -> Result<()> {
        match self.run_inner(input) {
            Ok(()) => Ok(()),
            Err(e) => {
                self.undo();
                Err(e)
            }
        }
    }

    /// Put the store back the way it was: the swap has not happened yet, so the
    /// live release is untouched; only the deploy's side effects need undoing.
    fn undo(&mut self) {
        if self.maintenance_on
            && let Err(e) = self.magento_current(&["maintenance:disable"])
        {
            eprintln!("agent: could not disable maintenance mode: {e:#}");
        }
        if self.cron_paused
            && let Err(e) = cron::start(&self.plan.deploy.cron)
        {
            eprintln!("agent: could not resume cron: {e:#}");
        }
        if let Some((name, path)) = self.release.take() {
            let trash = self.root.join("releases").join(format!(".trash-{name}"));
            if let Err(e) = fs::rename(&path, &trash) {
                eprintln!(
                    "agent: could not set aside the failed release {}: {e}",
                    path.display()
                );
            }
        }
    }

    fn run_inner(&mut self, input: &mut impl BufRead) -> Result<()> {
        let t = Instant::now();
        for dir in [".dep", "releases", "shared"] {
            fs::create_dir_all(self.root.join(dir))?;
        }
        self._lock = Some(lock(&self.root)?);
        // Last deploy's pruned releases go while this one streams.
        let trash = spawn_trash_cleanup(self.root.join("releases"));

        let name = self.next_release_name()?;
        let release = self.root.join("releases").join(&name);
        self.record_release(&name)?;
        fs::create_dir(&release).with_context(|| format!("creating {}", release.display()))?;
        self.release = Some((name.clone(), release.clone()));
        let previous = fs::read_link(self.root.join("current"))
            .ok()
            .map(|l| self.root.join(l));
        self.step("prepare", t, None)?;
        protocol::send(
            self.out,
            &Event::Ready {
                release: name.clone(),
            },
        )?;

        let t = Instant::now();
        let len = self.plan.artifact_len;
        extract(input, len, self.plan.artifact_format, &release)?;
        self.step("extract", t, Some(format!("{:.1} MB", len as f64 / 1e6)))?;

        let t = Instant::now();
        self.link_shared(&release)?;
        if let Some(env) = &self.d().env_php {
            envphp::derive(
                &self.root.join(&env.source),
                &release.join("app/etc/env.php"),
                &name,
                env.roll_cache_prefix,
            )?;
        }
        self.make_writable(&release)?;
        self.step("shared", t, None)?;

        let t = Instant::now();
        let db_upgrade = db_upgrade_needed(&release)?;
        let config_import = self.config_import_needed(&release)?;
        let needs = match (db_upgrade, config_import) {
            (false, false) => "database and config up to date".to_string(),
            (true, false) => "database upgrade needed".to_string(),
            (false, true) => "config import needed".to_string(),
            (true, true) => "database upgrade and config import needed".to_string(),
        };
        self.step("check", t, Some(needs.clone()))?;

        if self.d().upgrade == UpgradeMode::Check && (db_upgrade || config_import) {
            bail!("{needs}, but [deploy] upgrade = \"check\" never runs it; not swapping");
        }
        let t = Instant::now();
        cron::stop(&self.d().cron)?;
        self.cron_paused = true;
        self.step("cron-stop", t, None)?;

        if db_upgrade || config_import {
            let t = Instant::now();
            if self.root.join("current").exists() {
                self.magento_current(&["maintenance:enable"])?;
                self.maintenance_on = true;
            }
            if config_import {
                magento(
                    &self.d().php,
                    &release,
                    &["app:config:import", "--no-interaction"],
                )?;
            }
            if db_upgrade {
                magento(&self.d().php, &release, &["cache:clean", "config"])?;
                magento(
                    &self.d().php,
                    &release,
                    &["setup:db-schema:upgrade", "--no-interaction"],
                )?;
                magento(
                    &self.d().php,
                    &release,
                    &["setup:db-data:upgrade", "--no-interaction"],
                )?;
            }
            if self.maintenance_on {
                self.magento_current(&["maintenance:disable"])?;
                self.maintenance_on = false;
            }
            self.step("upgrade", t, None)?;
        }

        let t = Instant::now();
        swap(&self.root, &name)?;
        // Past the swap there is nothing to roll back: the new release is live.
        self.release = None;
        self.step("swap", t, None)?;

        let t = Instant::now();
        let mut notes = Vec::new();
        if let Some(op) = &self.d().opcache
            && let Err(e) = fcgi::opcache_reset(&op.fastcgi, Path::new(&op.tmp_dir))
        {
            notes.push(format!("opcache reset failed: {e:#}"));
        }
        if let Some(prev) = &previous {
            match retire_cache_prefixes(prev, &release) {
                Ok(n) => notes.push(format!("{n} old cache keys deleted")),
                Err(e) => notes.push(format!("old cache cleanup failed: {e:#}")),
            }
        }
        let resumed = cron::start(&self.d().cron);
        self.cron_paused = false;
        let pruned = self.prune(&name)?;
        if pruned > 0 {
            notes.push(format!("{pruned} old release(s) set aside"));
        }
        for n in notes.iter().filter(|n| n.contains("failed")) {
            eprintln!("agent: warning: {n}");
        }
        self.step("finish", t, Some(notes.join("; ")))?;
        resumed.context("resuming cron after the swap")?;

        let _ = trash.join();
        protocol::send(
            self.out,
            &Event::Done {
                release: name,
                ms: self.started.elapsed().as_millis() as u64,
            },
        )?;
        Ok(())
    }

    /// Deployer's numbering: one past `.dep/latest_release`, and past any
    /// release directory that already exists.
    fn next_release_name(&self) -> Result<String> {
        let latest = fs::read_to_string(self.root.join(".dep/latest_release"))
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let highest_dir = fs::read_dir(self.root.join("releases"))?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        Ok((latest.max(highest_dir) + 1).to_string())
    }

    fn record_release(&self, name: &str) -> Result<()> {
        fs::write(self.root.join(".dep/latest_release"), format!("{name}\n"))?;
        let entry = serde_json::json!({
            "created_at": iso8601_now(),
            "release_name": name,
            "user": self.plan.user,
            "target": self.plan.target,
        });
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join(".dep/releases_log"))?;
        writeln!(log, "{entry}")?;
        Ok(())
    }

    /// Deployer's deploy:shared: link each shared dir/file from `shared/` with
    /// a relative symlink, seeding `shared/` from the release the first time.
    fn link_shared(&self, release: &Path) -> Result<()> {
        let shared = self.root.join("shared");
        for d in &self.d().shared_dirs {
            let (sd, rd) = (shared.join(d), release.join(d));
            if !sd.exists() {
                fs::create_dir_all(&sd)?;
                if rd.is_dir() && !rd.is_symlink() {
                    copy_dir(&rd, &sd)?;
                }
            }
            remove_any(&rd)?;
            link_relative(&sd, &rd)?;
        }
        for f in &self.d().shared_files {
            let (sf, rf) = (shared.join(f), release.join(f));
            if !sf.exists() {
                fs::create_dir_all(sf.parent().unwrap())?;
                if rf.is_file() {
                    fs::copy(&rf, &sf)?;
                } else {
                    File::create(&sf)?;
                }
            }
            remove_any(&rf)?;
            link_relative(&sf, &rf)?;
        }
        for (rel, target) in &self.d().links {
            let link = release.join(rel);
            remove_any(&link)?;
            fs::create_dir_all(link.parent().unwrap())?;
            symlink(self.root.join(target), &link)?;
        }
        Ok(())
    }

    fn make_writable(&self, release: &Path) -> Result<()> {
        let mode = self.d().writable_mode_bits();
        for d in &self.d().writable_dirs {
            let p = release.join(d);
            fs::create_dir_all(&p)?;
            fs::set_permissions(&p, fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    /// `bin/magento app:config:status`: exit 2 means an import is needed.
    /// Still a PHP bootstrap; a native port is step 2 of the plan.
    fn config_import_needed(&self, release: &Path) -> Result<bool> {
        let status = Command::new(&self.d().php)
            .arg(release.join("bin/magento"))
            .arg("app:config:status")
            .current_dir(release)
            .stdout(stderr_stdio())
            .status()
            .with_context(|| format!("running {} bin/magento app:config:status", self.d().php))?;
        match status.code() {
            Some(0) => Ok(false),
            Some(2) => Ok(true),
            _ => bail!("bin/magento app:config:status exited with {status}"),
        }
    }

    fn magento_current(&self, args: &[&str]) -> Result<()> {
        magento(&self.d().php, &self.root.join("current"), args)
    }

    /// Keep the newest `keep_releases` (the new one and the live one among
    /// them); rename the rest to `.trash-<name>` for the next deploy to delete.
    fn prune(&self, new: &str) -> Result<usize> {
        let releases = self.root.join("releases");
        let mut names: Vec<u64> = fs::read_dir(&releases)?
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u64>().ok())
            .collect();
        names.sort_unstable_by(|a, b| b.cmp(a));
        let mut pruned = 0;
        for n in names.into_iter().skip(self.d().keep_releases) {
            let name = n.to_string();
            if name == new {
                continue;
            }
            fs::rename(
                releases.join(&name),
                releases.join(format!(".trash-{name}")),
            )?;
            pruned += 1;
        }
        Ok(pruned)
    }
}

/// Hold `.dep/magebuild.lock` for the life of the process; the kernel releases
/// it however the agent exits, so a killed deploy never leaves a stale lock.
fn lock(root: &Path) -> Result<File> {
    if root.join(".dep/deploy.lock").exists() {
        bail!(
            "{} exists: a Deployer deploy is running, or one was killed (remove it once nothing is deploying)",
            root.join(".dep/deploy.lock").display()
        );
    }
    let f = File::create(root.join(".dep/magebuild.lock"))?;
    let rc = unsafe {
        libc::flock(
            std::os::fd::AsRawFd::as_raw_fd(&f),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    };
    if rc != 0 {
        bail!(
            "another magebuild deploy holds {}",
            root.join(".dep/magebuild.lock").display()
        );
    }
    Ok(f)
}

fn spawn_trash_cleanup(releases: PathBuf) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok(entries) = fs::read_dir(&releases) else {
            return;
        };
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(".trash-")
                && let Err(err) = fs::remove_dir_all(e.path())
            {
                eprintln!("agent: could not delete {}: {err}", e.path().display());
            }
        }
    })
}

fn extract(input: &mut impl Read, len: u64, format: ArtifactFormat, dest: &Path) -> Result<()> {
    let mut limited = input.take(len);
    {
        let reader: Box<dyn Read + '_> = match format {
            ArtifactFormat::Tar => Box::new(&mut limited),
            ArtifactFormat::TarGz => Box::new(flate2::read::MultiGzDecoder::new(&mut limited)),
            ArtifactFormat::TarZst => Box::new(zstd::Decoder::new(&mut limited)?),
        };
        let mut archive = tar::Archive::new(reader);
        archive.set_preserve_permissions(true);
        archive.set_preserve_mtime(true);
        archive.set_overwrite(true);
        archive.unpack(dest).context("unpacking the artifact")?;
    }
    // Consume whatever the decoder left (tar padding), so the session stays in step.
    std::io::copy(&mut limited, &mut std::io::sink())?;
    if limited.limit() != 0 {
        bail!(
            "the artifact stream ended early ({} bytes missing)",
            limited.limit()
        );
    }
    Ok(())
}

fn db_upgrade_needed(release: &Path) -> Result<bool> {
    let mage = magequery_core::Magento::open(release).map_err(|e| anyhow!(e))?;
    let status = mage.db_status().map_err(|e| anyhow!(e))?;
    for e in &status.errors {
        eprintln!("agent: db-status: {e}");
    }
    Ok(!status.is_up_to_date())
}

fn magento(php: &str, root: &Path, args: &[&str]) -> Result<()> {
    eprintln!("agent: bin/magento {}", args.join(" "));
    let status = Command::new(php)
        .arg(root.join("bin/magento"))
        .args(args)
        .current_dir(root)
        .stdout(stderr_stdio())
        .status()
        .with_context(|| format!("running {php} bin/magento"))?;
    if !status.success() {
        bail!("bin/magento {} exited with {status}", args.join(" "));
    }
    Ok(())
}

/// Atomically point `current` at `releases/<name>` (relative, like Deployer).
fn swap(root: &Path, name: &str) -> Result<()> {
    let tmp = root.join(".current.tmp");
    let _ = fs::remove_file(&tmp);
    symlink(format!("releases/{name}"), &tmp)?;
    fs::rename(&tmp, root.join("current")).context("swapping the current symlink")?;
    Ok(())
}

/// Delete the previous release's cache keys, once no live frontend uses its
/// prefix any more.
fn retire_cache_prefixes(previous: &Path, release: &Path) -> Result<u64> {
    let old = envphp::redis_caches(&previous.join("app/etc/env.php"))?;
    let new = envphp::redis_caches(&release.join("app/etc/env.php"))?;
    let mut done = Vec::new();
    let mut deleted = 0;
    for c in old {
        let key = (c.server.clone(), c.port, c.database, c.prefix.clone());
        let still_live = new.iter().any(|n| {
            n.prefix == c.prefix
                && n.server == c.server
                && n.port == c.port
                && n.database == c.database
        });
        if still_live || done.contains(&key) {
            continue;
        }
        deleted += redis::delete_prefix(&c)?;
        done.push(key);
    }
    Ok(deleted)
}

fn remove_any(p: &Path) -> Result<()> {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(p)?,
        Ok(_) => fs::remove_file(p)?,
        Err(_) => {}
    }
    Ok(())
}

/// `ln -s --relative target link`.
fn link_relative(target: &Path, link: &Path) -> Result<()> {
    let dir = link.parent().unwrap();
    fs::create_dir_all(dir)?;
    symlink(relative(dir, target), link).with_context(|| format!("linking {}", link.display()))
}

fn relative(from_dir: &Path, to: &Path) -> PathBuf {
    let from: Vec<Component> = from_dir.components().collect();
    let to_c: Vec<Component> = to.components().collect();
    let common = from.iter().zip(&to_c).take_while(|(a, b)| a == b).count();
    let mut out = PathBuf::new();
    for _ in common..from.len() {
        out.push("..");
    }
    for c in &to_c[common..] {
        out.push(c.as_os_str());
    }
    out
}

fn copy_dir(from: &Path, to: &Path) -> Result<()> {
    fs::create_dir_all(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let (src, dst) = (e.path(), to.join(e.file_name()));
        let ty = e.file_type()?;
        if ty.is_symlink() {
            symlink(fs::read_link(&src)?, &dst)?;
        } else if ty.is_dir() {
            copy_dir(&src, &dst)?;
        } else {
            fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

/// `2026-11-13T13:36:44+0000`, the format Deployer writes to releases_log.
fn iso8601_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}+0000",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_links_climb_out_of_the_release() {
        assert_eq!(
            relative(
                Path::new("/srv/shop/releases/36/var"),
                Path::new("/srv/shop/shared/var/log")
            ),
            PathBuf::from("../../../shared/var/log")
        );
        assert_eq!(
            relative(
                Path::new("/srv/shop/releases/36/pub/static"),
                Path::new("/srv/shop/shared/pub/static/_cache")
            ),
            PathBuf::from("../../../../shared/pub/static/_cache")
        );
    }

    #[test]
    fn timestamps_match_deployers_format() {
        let t = iso8601_now();
        assert_eq!(t.len(), "2026-11-13T13:36:44+0000".len());
        assert!(t.ends_with("+0000") && t.as_bytes()[10] == b'T');
    }
}
