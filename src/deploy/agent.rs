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
use super::manifest::{self, Entry};
use super::protocol::{self, ArtifactFormat, Data, Event, Need, PROTOCOL, Plan};
use super::{cron, envphp, fcgi, redis, session};

/// A child process's stdout, pointed at our stderr: stdout is the protocol.
pub fn stderr_stdio() -> Stdio {
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .map(Stdio::from)
        .unwrap_or_else(|_| Stdio::null())
}

pub fn run() -> ExitCode {
    session::detach();
    let stdin = std::io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let mut out = session::ToRunner(std::io::stdout().lock());
    let code = match serve(&mut input, &mut out) {
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
    };
    session::finish();
    code
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
    let manifest = match plan.manifest_len {
        Some(len) => {
            let mut bytes = vec![0; len as usize];
            input
                .read_exact(&mut bytes)
                .context("reading the manifest")?;
            Some(manifest::decode(&bytes)?)
        }
        None => None,
    };
    let mut deploy = Deploy::new(plan, out)?;
    deploy.manifest = manifest;
    deploy.run(input)
}

/// Where a release's manifest is kept once it is live.
fn manifest_path(root: &Path, release: &str) -> PathBuf {
    root.join(".dep/manifests")
        .join(format!("{release}.json.zst"))
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
    /// The live release was in maintenance before the deploy (someone put it
    /// there): the new release inherits it and the deploy never ends it.
    kept_maintenance: bool,
    /// An import or upgrade command has run. DDL commits as it goes, so after a
    /// failure from here the database may be half upgraded: the store stays in
    /// maintenance with cron stopped instead of reopening on the old code.
    upgrade_started: bool,
    /// An earlier deploy's upgrade failed partway (`UPGRADE_FAILED` exists): the
    /// maintenance flag is ours, not someone's, and stays on until a deploy
    /// gets through.
    failed_upgrade_pending: bool,
    /// The incoming release's AMQP topology fingerprint, recorded at the swap.
    amqp_fingerprint: Option<String>,
    /// The incoming release's manifest, from the runner.
    manifest: Option<Vec<Entry>>,
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
            kept_maintenance: false,
            upgrade_started: false,
            failed_upgrade_pending: false,
            amqp_fingerprint: None,
            manifest: None,
            _lock: None,
        })
    }

    fn d(&self) -> &DeploySection {
        &self.plan.deploy
    }

    fn step(&mut self, name: &str, since: Instant, detail: Option<String>) -> Result<()> {
        session::log_line(&format!(
            "step {name} {}ms {}",
            since.elapsed().as_millis(),
            detail.as_deref().unwrap_or("")
        ));
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
                if self.upgrade_started {
                    if let Err(m) = fs::write(self.root.join(UPGRADE_FAILED), "") {
                        eprintln!("agent: could not record the failed upgrade: {m}");
                    }
                    return Err(e.context(
                        "the upgrade failed partway, so the database may not match \
                         either release: maintenance stays on and cron stays stopped. \
                         Fix the cause and deploy again; or, once the database matches \
                         the live code, run `bin/magento maintenance:disable` in \
                         current and start cron",
                    ));
                }
                if self.failed_upgrade_pending {
                    return Err(e.context(
                        "an earlier upgrade failed partway and this deploy did not get \
                         through either: maintenance stays on",
                    ));
                }
                Err(e)
            }
        }
    }

    /// Put the store back the way it was: the swap has not happened yet, so the
    /// live release is untouched; only the deploy's side effects need undoing.
    /// Once the upgrade has started there is no way back, so maintenance and
    /// cron are left as they are.
    fn undo(&mut self) {
        let reopen = !self.upgrade_started && !self.failed_upgrade_pending;
        if reopen
            && self.maintenance_on
            && !self.kept_maintenance
            && let Err(e) = clear_maintenance_flag(&self.root.join("current"))
        {
            eprintln!("agent: could not disable maintenance mode: {e:#}");
        }
        if reopen
            && self.cron_paused
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
        session::open_log(&self.root);
        // Pruned releases whose cleanup did not finish (it was killed, or the
        // host has no detached processes) go while this one streams.
        let releases = self.root.join("releases");
        std::thread::spawn(move || delete_trash(&releases));

        let name = self.next_release_name()?;
        let release = self.root.join("releases").join(&name);
        self.record_release(&name)?;
        fs::create_dir(&release).with_context(|| format!("creating {}", release.display()))?;
        self.release = Some((name.clone(), release.clone()));
        let previous = fs::read_link(self.root.join("current"))
            .ok()
            .map(|l| self.root.join(l));
        // Read before this deploy sets any flag. `exists()` follows links,
        // like Magento's `file_exists()`.
        self.failed_upgrade_pending = self.root.join(UPGRADE_FAILED).exists();
        self.maintenance_on = self.failed_upgrade_pending;
        self.kept_maintenance = !self.failed_upgrade_pending
            && previous
                .as_ref()
                .is_some_and(|p| p.join(MAINTENANCE_FLAG).exists());
        self.step("prepare", t, None)?;

        // Link (or copy) whatever the live release already has, and ask for the rest.
        let t = Instant::now();
        let new = self.manifest.take();
        let live = previous.as_ref().and_then(|p| {
            let live_name = p.file_name()?.to_str()?;
            let bytes = fs::read(manifest_path(&self.root, live_name)).ok()?;
            match manifest::decode(&bytes) {
                Ok(m) => Some((p.clone(), live_name.to_string(), m)),
                Err(e) => {
                    eprintln!("agent: ignoring release {live_name}'s manifest: {e:#}");
                    None
                }
            }
        });
        let need = match (&new, &live) {
            (Some(new), Some((live_root, live_name, live))) => {
                let reuse = manifest::plan_reuse(new, live_root, live);
                manifest::materialize(&release, new, &reuse, live_root, self.d().reuse)?;
                let reused = reuse.iter().filter(|r| r.is_some()).count();
                let indices = manifest::needed(new, &reuse);
                self.step(
                    "reuse",
                    t,
                    Some(format!(
                        "{reused} unchanged files from release {live_name}, {} to send",
                        indices.len()
                    )),
                )?;
                Need::Files { indices }
            }
            _ => Need::All,
        };
        let sending = match &need {
            Need::All => None,
            Need::Files { indices } => Some(indices.len()),
        };
        protocol::send(
            self.out,
            &Event::Ready {
                release: name.clone(),
                need,
            },
        )?;

        let t = Instant::now();
        let data: Data =
            protocol::recv(input)?.context("the runner closed the session before sending data")?;
        extract(input, data.len, data.format, &release)?;
        let stored = new
            .as_deref()
            .map(|m| manifest::finish(&release, m))
            .transpose()?;
        let mb = data.len as f64 / 1e6;
        let detail = match sending {
            Some(n) => format!("{n} files, {mb:.1} MB"),
            None => format!("{mb:.1} MB"),
        };
        self.step("extract", t, Some(detail))?;

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
        let mage = magequery_core::Magento::open(&release).map_err(|e| anyhow!(e))?;
        let schema_or_patches = db_upgrade_needed(&mage)?;
        let config_import = self.config_import_needed(&mage, &release)?;
        let registration = registration_needed(&mage);
        self.amqp_fingerprint = registration.amqp_fingerprint.clone();
        let amqp_changed = registration.amqp_fingerprint.as_ref().is_some_and(|fp| {
            fs::read_to_string(self.root.join(AMQP_FINGERPRINT))
                .ok()
                .as_deref()
                .map(str::trim)
                != Some(fp.as_str())
        });
        drop(mage);
        // setup:db-schema:upgrade runs MysqlMq's and Amqp's recurring setup (queues),
        // setup:db-data:upgrade the Theme module's (theme registration).
        let db_upgrade = schema_or_patches || !registration.reasons.is_empty() || amqp_changed;
        let mut parts: Vec<&str> = Vec::new();
        if schema_or_patches {
            parts.push("database upgrade");
        }
        if config_import {
            parts.push("config import");
        }
        parts.extend(registration.reasons.iter().map(String::as_str));
        if amqp_changed {
            parts.push("AMQP topology setup");
        }
        let needs = if parts.is_empty() {
            "database, config, themes and queues up to date".to_string()
        } else {
            format!("{} needed", parts.join(", "))
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
            // Both releases: the live one for the upgrade, the new one so it
            // comes up in maintenance at the swap and opens only once opcache
            // is reset. Through a shared var/ these are the same flag.
            if self.root.join("current").exists() {
                set_maintenance_flag(&self.root.join("current"))?;
                self.maintenance_on = true;
                set_maintenance_flag(&release)?;
            }
            // Past here stopping halfway is worse than finishing, so a lost
            // runner no longer stops the deploy.
            session::commit();
            self.upgrade_started = true;
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
            self.step("upgrade", t, None)?;
        }

        let t = Instant::now();
        // Maintenance someone switched on stays on: the new release comes up
        // in it too, even when this deploy had nothing to upgrade.
        session::commit();
        if self.kept_maintenance || self.maintenance_on {
            set_maintenance_flag(&release)?;
        }
        swap(&self.root, &name)?;
        // Past the swap there is nothing to roll back: the new release is live.
        self.release = None;
        // The live release's topology is now what the broker has (the upgrade ran
        // when it changed), so the next deploy compares against it.
        let fp_path = self.root.join(AMQP_FINGERPRINT);
        match &self.amqp_fingerprint {
            Some(fp) => fs::write(&fp_path, format!("{fp}\n"))?,
            None => {
                let _ = fs::remove_file(&fp_path);
            }
        }
        // The next deploy reuses files by this manifest.
        if let Some(stored) = &stored
            && let Err(e) = store_manifest(&self.root, &name, stored)
        {
            eprintln!("agent: warning: could not store the manifest: {e:#}");
        }
        self.step("swap", t, None)?;

        let t = Instant::now();
        let mut notes = Vec::new();
        if let Some(op) = &self.d().opcache
            && let Err(e) = fcgi::opcache_reset(&op.fastcgi, Path::new(&op.tmp_dir))
        {
            notes.push(format!("opcache reset failed: {e:#}"));
        }
        // Maintenance stayed on through the swap, so the old code never served
        // the upgraded database; this ends it. Only the new release's flag:
        // the old release keeps its own, so a rollback to code that may not
        // match the upgraded database comes up in maintenance until someone
        // runs `maintenance:disable` there.
        if self.kept_maintenance {
            notes.push("maintenance was on before the deploy and stays on".into());
        } else if self.maintenance_on
            && let Err(e) = clear_maintenance_flag(&release)
        {
            notes.push(format!("clearing maintenance mode failed: {e:#}"));
        }
        self.maintenance_on = false;
        if self.failed_upgrade_pending
            && let Err(e) = fs::remove_file(self.root.join(UPGRADE_FAILED))
        {
            notes.push(format!("clearing the failed-upgrade marker failed: {e}"));
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

        // Deleting a release takes seconds, so it happens after the agent
        // exits, in a process of its own.
        spawn_detached_cleanup(&self.root.join("releases"));
        session::log_line(&format!(
            "done: release {name} in {}ms{}",
            self.started.elapsed().as_millis(),
            if session::runner_gone() {
                " (the runner had gone)"
            } else {
                ""
            }
        ));
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

    /// Whether `app:config:import` has work to do: magequery's native
    /// `app:config:status`, or `bin/magento app:config:status` when the config
    /// holds something the native check doesn't reproduce exactly.
    fn config_import_needed(&self, mage: &magequery_core::Magento, release: &Path) -> Result<bool> {
        match mage.config_status() {
            Ok(status) => {
                for s in status.sections.iter().filter(|s| s.changed) {
                    eprintln!(
                        "agent: config-status: {} changed since the last import",
                        s.name
                    );
                }
                return Ok(status.import_required());
            }
            Err(e) => eprintln!("agent: config-status: {e}; asking bin/magento instead"),
        }
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
        // Manifests of releases that are gone.
        if let Ok(entries) = fs::read_dir(self.root.join(".dep/manifests")) {
            for e in entries.flatten() {
                let file = e.file_name();
                let release = file.to_string_lossy();
                let release = release.trim_end_matches(".json.zst");
                if !releases.join(release).is_dir() {
                    let _ = fs::remove_file(e.path());
                }
            }
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

fn store_manifest(root: &Path, release: &str, entries: &[Entry]) -> Result<()> {
    let path = manifest_path(root, release);
    fs::create_dir_all(path.parent().unwrap())?;
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, manifest::encode(entries)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// Delete the releases set aside as `.trash-<name>`. Another cleanup may be
/// deleting the same ones, so a missing file is not an error.
pub fn delete_trash(releases: &Path) {
    let Ok(entries) = fs::read_dir(releases) else {
        return;
    };
    for e in entries.flatten() {
        if e.file_name().to_string_lossy().starts_with(".trash-")
            && let Err(err) = fs::remove_dir_all(e.path())
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("agent: could not delete {}: {err}", e.path().display());
        }
    }
}

/// `magebuild agent-cleanup` in its own process group with no stdio, so the ssh
/// session closes as soon as the agent exits. If the host kills it with the
/// session anyway, the next deploy finishes the job.
fn spawn_detached_cleanup(releases: &Path) {
    use std::os::unix::process::CommandExt;
    let spawned = Command::new("/proc/self/exe")
        .arg("agent-cleanup")
        .arg(releases)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn();
    if let Err(e) = spawned {
        eprintln!("agent: could not start the cleanup of old releases: {e}");
    }
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

/// Where the live release's AMQP topology fingerprint is kept.
const AMQP_FINGERPRINT: &str = ".dep/magebuild-amqp-topology";

struct Registration {
    /// What `setup:upgrade` would register: unregistered themes, missing
    /// database queues, or a check that couldn't run.
    reasons: Vec<String>,
    amqp_fingerprint: Option<String>,
}

/// The themes and queues `setup:upgrade` registers, which `setup:db:status`
/// does not check. A check that can't run counts as needing the upgrade.
fn registration_needed(mage: &magequery_core::Magento) -> Registration {
    let mut reasons = Vec::new();
    match mage.unregistered_themes() {
        Ok(missing) if missing.is_empty() => {}
        Ok(missing) => {
            eprintln!(
                "agent: theme-status: not registered: {}",
                missing.join(", ")
            );
            reasons.push("theme registration".to_string());
        }
        Err(e) => {
            eprintln!("agent: theme-status: {e}; upgrading to be safe");
            reasons.push("theme registration (unchecked)".to_string());
        }
    }
    match mage.missing_db_queues() {
        Ok(missing) if missing.is_empty() => {}
        Ok(missing) => {
            eprintln!(
                "agent: queue-status: not in the queue table: {}",
                missing.join(", ")
            );
            reasons.push("queue setup".to_string());
        }
        Err(e) => {
            eprintln!("agent: queue-status: {e}; upgrading to be safe");
            reasons.push("queue setup (unchecked)".to_string());
        }
    }
    let amqp_fingerprint = match mage.amqp_topology_fingerprint() {
        Ok(fp) => fp,
        Err(e) => {
            eprintln!("agent: AMQP topology: {e}; upgrading to be safe");
            reasons.push("AMQP topology setup (unchecked)".to_string());
            None
        }
    };
    Registration {
        reasons,
        amqp_fingerprint,
    }
}

fn db_upgrade_needed(mage: &magequery_core::Magento) -> Result<bool> {
    let status = mage.db_status().map_err(|e| anyhow!(e))?;
    for e in &status.errors {
        eprintln!("agent: db-status: {e}");
    }
    Ok(!status.is_up_to_date())
}

/// Magento's maintenance switch: `MaintenanceMode::FLAG_FILENAME` in `VAR_DIR`.
/// Through a shared `var/` it is the one flag every release reads.
const MAINTENANCE_FLAG: &str = "var/.maintenance.flag";

/// Written when an upgrade fails partway and the store is left in maintenance,
/// so the next deploy knows that flag is its own to clear once it gets through.
const UPGRADE_FAILED: &str = ".dep/upgrade-failed";

/// What `bin/magento maintenance:enable` does, without booting PHP:
/// `MaintenanceMode::set(true)` touches the flag, creating `var/` if missing
/// (PHP's `touch()`: an empty file, mode 0666 less the umask, or a fresh mtime
/// on one that exists). The `maintenance_mode_changed` event it also dispatches
/// has no observer in Magento itself, so nothing is skipped there; neither
/// command form touches `.maintenance.ip` without `--ip`.
fn set_maintenance_flag(release: &Path) -> Result<()> {
    let flag = release.join(MAINTENANCE_FLAG);
    let var = flag.parent().expect("flag has a parent");
    fs::create_dir_all(var).with_context(|| format!("creating {}", var.display()))?;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&flag)
        .and_then(|f| f.set_modified(std::time::SystemTime::now()))
        .with_context(|| format!("touching {}", flag.display()))
}

/// What `bin/magento maintenance:disable` does: delete the flag if it exists.
fn clear_maintenance_flag(release: &Path) -> Result<()> {
    let flag = release.join(MAINTENANCE_FLAG);
    match fs::remove_file(&flag) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("deleting {}", flag.display()))
        }
        _ => Ok(()),
    }
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
    fn maintenance_flag_matches_magento_in_shared_and_own_var() {
        let root = std::env::temp_dir().join(format!(
            "magebuild-maint-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // Own var/: each release has its flag, and clearing one leaves the other.
        let (a, b) = (root.join("own/1"), root.join("own/2"));
        for r in [&a, &b] {
            fs::create_dir_all(r.join("var")).unwrap();
            fs::write(r.join("var/.maintenance.flag"), "").unwrap();
        }
        clear_maintenance_flag(&a).unwrap();
        assert!(!a.join("var/.maintenance.flag").exists());
        assert!(b.join("var/.maintenance.flag").exists());
        // Shared var/: one flag, cleared through either release; the second
        // clear finds nothing and is fine.
        let shared = root.join("shared/var");
        fs::create_dir_all(&shared).unwrap();
        fs::write(shared.join(".maintenance.flag"), "").unwrap();
        let (c, d) = (root.join("shared-rel/1"), root.join("shared-rel/2"));
        for r in [&c, &d] {
            fs::create_dir_all(r).unwrap();
            std::os::unix::fs::symlink(&shared, r.join("var")).unwrap();
        }
        clear_maintenance_flag(&c).unwrap();
        clear_maintenance_flag(&d).unwrap();
        assert!(!shared.join(".maintenance.flag").exists());
        assert!(d.join("var").is_dir());
        // No var/ at all.
        clear_maintenance_flag(&root.join("missing")).unwrap();
        // Enabling creates var/ and an empty flag, through a shared var/ too,
        // and keeps an existing flag's contents.
        set_maintenance_flag(&root.join("fresh")).unwrap();
        assert_eq!(
            fs::read(root.join("fresh/var/.maintenance.flag")).unwrap(),
            b""
        );
        set_maintenance_flag(&c).unwrap();
        assert!(shared.join(".maintenance.flag").is_file());
        fs::write(shared.join(".maintenance.flag"), "x").unwrap();
        set_maintenance_flag(&d).unwrap();
        assert_eq!(fs::read(shared.join(".maintenance.flag")).unwrap(), b"x");
        fs::remove_dir_all(&root).unwrap();
    }

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
