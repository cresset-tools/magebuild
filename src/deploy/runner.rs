//! `magebuild deploy`: the CI side. Opens one ssh session, makes sure the
//! host has this exact agent build (uploading it only when missing), sends the
//! plan, streams the artifact and prints the agent's progress.

use std::fs::File;
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::config::DeploySection;
use super::protocol::{self, ArtifactFormat, Event, PROTOCOL, Plan};

pub struct Options {
    pub artifact: PathBuf,
    /// The agent executable to run on the host (a static Linux build).
    /// Defaults to this executable.
    pub agent: Option<PathBuf>,
    /// Overrides `[deploy] host`.
    pub host: Option<String>,
    /// The ssh command, split on whitespace (`ssh -o BatchMode=yes`).
    pub ssh: String,
}

pub fn run(mut deploy: DeploySection, opts: Options) -> Result<ExitCode> {
    let started = Instant::now();
    if let Some(h) = opts.host {
        deploy.host = h;
    }
    deploy.validate()?;

    let agent_path = match opts.agent {
        Some(p) => p,
        None => std::env::current_exe().context("locating this executable")?,
    };
    let agent = std::fs::read(&agent_path)
        .with_context(|| format!("reading the agent binary {}", agent_path.display()))?;
    let agent_sha = hex(&Sha256::digest(&agent));

    let format = ArtifactFormat::from_path(&opts.artifact)?;
    let artifact_len = std::fs::metadata(&opts.artifact)
        .with_context(|| format!("reading {}", opts.artifact.display()))?
        .len();

    let mut ssh = opts.ssh.split_whitespace();
    let program = ssh.next().context("--ssh is empty")?;
    let mut child = Command::new(program)
        .args(ssh)
        .arg(&deploy.host)
        .arg("--")
        .arg(bootstrap_script(&agent_sha, agent.len()))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("starting `{}`", opts.ssh))?;
    let mut to_agent = child.stdin.take().expect("piped stdin");
    let mut from_agent = BufReader::new(child.stdout.take().expect("piped stdout"));

    let result = (|| -> Result<String> {
        // The bootstrap prints `need-agent` before reading the binary; otherwise
        // the first line is already the agent's hello.
        let mut first = String::new();
        std::io::BufRead::read_line(&mut from_agent, &mut first)?;
        let hello_line = if first.trim_end() == "need-agent" {
            eprintln!("  uploading agent ({:.1} MB)", agent.len() as f64 / 1e6);
            to_agent.write_all(&agent)?;
            to_agent.flush()?;
            let mut l = String::new();
            std::io::BufRead::read_line(&mut from_agent, &mut l)?;
            l
        } else {
            first
        };
        if hello_line.is_empty() {
            bail!("the ssh session ended before the agent started (see its output above)");
        }
        let hello: Event = serde_json::from_str(hello_line.trim_end()).with_context(|| {
            format!(
                "unexpected first line from the host: {}",
                hello_line.trim_end()
            )
        })?;
        let Event::Hello {
            protocol,
            version,
            sha256,
        } = hello
        else {
            bail!("expected the agent's hello, got {hello:?}");
        };
        if protocol != PROTOCOL {
            bail!("agent {version} speaks protocol {protocol}, this magebuild speaks {PROTOCOL}");
        }
        if sha256 != agent_sha {
            bail!(
                "the agent on the host does not match {} (sha256 {sha256})",
                agent_path.display()
            );
        }

        protocol::send(
            &mut to_agent,
            &Plan {
                protocol: PROTOCOL,
                deploy,
                artifact_len,
                artifact_format: format,
                user: env_or(&["GITHUB_ACTOR", "USER"], "magebuild"),
                target: env_or(&["GITHUB_SHA"], "HEAD"),
            },
        )?;

        let mut release = String::new();
        loop {
            let ev: Event = protocol::recv(&mut from_agent)?
                .context("the agent exited without reporting a result")?;
            match ev {
                Event::Ready { release: r } => {
                    eprintln!(
                        "  release {r}: streaming {:.1} MB",
                        artifact_len as f64 / 1e6
                    );
                    stream(&opts.artifact, artifact_len, &mut to_agent)?;
                    release = r;
                }
                Event::Step { name, ms, detail } => match detail {
                    Some(d) => eprintln!("✓ {name:<16} {:>6.2}s  {d}", ms as f64 / 1000.0),
                    None => eprintln!("✓ {name:<16} {:>6.2}s", ms as f64 / 1000.0),
                },
                Event::Done { release: r, .. } => return Ok(r),
                Event::Failed { message } if release.is_empty() => {
                    bail!("deploy failed on the host: {message}")
                }
                Event::Failed { message } => {
                    bail!("deploy of release {release} failed on the host: {message}")
                }
                Event::Hello { .. } => bail!("unexpected second hello"),
            }
        }
    })();
    drop(to_agent);
    let status = child.wait()?;

    match result {
        Ok(release) => {
            eprintln!(
                "deployed release {release} in {:.1}s",
                started.elapsed().as_secs_f64()
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            if !status.success() {
                eprintln!("ssh exited with {status}");
            }
            Ok(ExitCode::FAILURE)
        }
    }
}

/// Runs under the host's login shell. Keeps the agent at a path named by its
/// hash, so a deploy uploads it only the first time a build is used. `head -c`
/// cannot read past the binary: the runner sends nothing else until the agent
/// says hello.
fn bootstrap_script(sha: &str, len: usize) -> String {
    format!(
        r#"a="$HOME/.cache/magebuild/agent-{sha}"; if [ ! -x "$a" ]; then mkdir -p "${{a%/*}}" && echo need-agent && head -c {len} > "$a.$$" && chmod 755 "$a.$$" && mv -f "$a.$$" "$a" || exit 97; fi; exec "$a" agent"#
    )
}

fn stream(path: &Path, len: u64, to: &mut impl Write) -> Result<()> {
    let mut f = File::open(path)?.take(len);
    let n = std::io::copy(&mut f, to)?;
    to.flush()?;
    if n != len {
        bail!(
            "{} changed size while streaming ({n} of {len} bytes)",
            path.display()
        );
    }
    Ok(())
}

fn env_or(keys: &[&str], default: &str) -> String {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| default.to_string())
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
