//! Pausing the store's cron and queue consumers around the deploy's database
//! work. Backends are pluggable because hosts differ: the demo runs cron as
//! systemd units, while a Hypernode runs it from the user's crontab (not yet
//! supported here).

use std::process::Command;

use anyhow::{Context, Result, bail};

use super::config::Cron;

pub fn stop(cron: &Cron) -> Result<()> {
    match cron {
        Cron::None => Ok(()),
        Cron::Systemd {
            stop,
            kill_stragglers,
            ..
        } => {
            for unit in stop {
                systemctl("stop", unit)?;
            }
            if *kill_stragglers {
                kill_magento_workers()?;
            }
            Ok(())
        }
    }
}

pub fn start(cron: &Cron) -> Result<()> {
    match cron {
        Cron::None => Ok(()),
        Cron::Systemd { start, .. } => {
            // Start every unit even if one fails, then report the first error:
            // leaving the rest stopped is worse than a partial start.
            let mut first_err = None;
            for unit in start {
                if let Err(e) = systemctl("start", unit) {
                    first_err.get_or_insert(e);
                }
            }
            first_err.map_or(Ok(()), Err)
        }
    }
}

/// One unit per call: since systemd 262 a multi-unit `systemctl` sends one
/// EnqueueUnitJobMany() whose polkit check names no unit, so a rule scoped to
/// specific units cannot allow it.
fn systemctl(verb: &str, unit: &str) -> Result<()> {
    let status = Command::new("systemctl")
        .args([verb, unit])
        .stdout(super::agent::stderr_stdio())
        .status()
        .context("running systemctl")?;
    if !status.success() {
        bail!("systemctl {verb} {unit} exited with {status}");
    }
    Ok(())
}

/// SIGTERM this user's `bin/magento cron:run` and `queue:consumers:start`
/// processes: Magento's consumers_runner spawns detached consumers that a unit
/// stop does not reach, and they would keep running the old release's code.
fn kill_magento_workers() -> Result<()> {
    let me = unsafe { libc::getuid() };
    let own_pid = std::process::id();
    for entry in std::fs::read_dir("/proc")? {
        let Ok(entry) = entry else { continue };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        if pid as u32 == own_pid {
            continue;
        }
        let Ok(status) = std::fs::read_to_string(entry.path().join("status")) else {
            continue;
        };
        let uid = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|l| l.split_whitespace().next())
            .and_then(|u| u.parse::<u32>().ok());
        if uid != Some(me) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if is_magento_worker(&cmdline) {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
    }
    Ok(())
}

fn is_magento_worker(cmdline: &[u8]) -> bool {
    let args: Vec<&[u8]> = cmdline
        .split(|&b| b == 0)
        .filter(|a| !a.is_empty())
        .collect();
    args.windows(2).any(|w| {
        w[0].ends_with(b"bin/magento") && (w[1] == b"cron:run" || w[1] == b"queue:consumers:start")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_cron_and_consumer_processes_only() {
        assert!(is_magento_worker(
            b"php\0/var/lib/magento/releases/35/bin/magento\0cron:run\0--group=index\0"
        ));
        assert!(is_magento_worker(
            b"php\0bin/magento\0queue:consumers:start\0async.operations.all\0"
        ));
        assert!(!is_magento_worker(b"php\0bin/magento\0cache:flush\0"));
        assert!(!is_magento_worker(b"vim\0bin/magento.cron:run\0"));
    }
}
