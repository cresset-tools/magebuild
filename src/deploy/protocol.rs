//! The runner ↔ agent protocol: JSON lines both ways over the ssh session's
//! stdin and stdout, plus one raw byte stream (the artifact) whose length the
//! plan announces. The agent's stderr is not part of it and goes straight to
//! the runner's terminal.
//!
//! ```text
//! runner                              agent
//!   ── (agent binary, only if missing) ─▶
//!   ◀──────────── Hello ────────────────
//!   ───────────── Plan ────────────────▶
//!   ◀──────────── Ready ────────────────   lock taken, releases/N created
//!   ── artifact bytes (plan.artifact_len) ▶
//!   ◀──────────── Step … ───────────────   one per finished step
//!   ◀──────────── Done | Failed ────────
//! ```

use std::io::{BufRead, Write};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::config::DeploySection;

/// Bumped on any incompatible change; both sides refuse a mismatch.
pub const PROTOCOL: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactFormat {
    TarGz,
    TarZst,
    Tar,
}

impl ArtifactFormat {
    pub fn from_path(p: &std::path::Path) -> Result<Self> {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        Ok(if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
            Self::TarGz
        } else if name.ends_with(".tar.zst") {
            Self::TarZst
        } else if name.ends_with(".tar") {
            Self::Tar
        } else {
            bail!("unsupported artifact `{name}`: expected .tar, .tar.gz or .tar.zst")
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Plan {
    pub protocol: u32,
    pub deploy: DeploySection,
    pub artifact_len: u64,
    pub artifact_format: ArtifactFormat,
    /// Recorded in `.dep/releases_log`, as Deployer does.
    pub user: String,
    pub target: String,
}

/// Agent → runner.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Event {
    Hello {
        protocol: u32,
        version: String,
        /// sha256 of the agent's own executable, so the runner can confirm an
        /// upload arrived intact.
        sha256: String,
    },
    Ready {
        release: String,
    },
    Step {
        name: String,
        ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Done {
        release: String,
        ms: u64,
    },
    Failed {
        message: String,
    },
}

pub fn send<W: Write, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    serde_json::to_writer(&mut *w, msg)?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

/// Read one JSON line. `Ok(None)` on a clean end of stream.
pub fn recv<R: BufRead, T: for<'de> Deserialize<'de>>(r: &mut R) -> Result<Option<T>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    serde_json::from_str(line.trim_end())
        .with_context(|| {
            format!(
                "unexpected message from the other side: {}",
                line.trim_end()
            )
        })
        .map(Some)
}
