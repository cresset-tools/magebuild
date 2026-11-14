//! What a release contains, entry by entry, so a deploy sends only the files
//! the host doesn't already have.
//!
//! The runner reads the manifest out of the artifact (one pass, hashing each
//! file) and sends it to the agent. The agent matches each file against the
//! live release's stored manifest by content hash and mode, hard-links (or
//! copies) the matches from the live release, and asks for the rest. After the
//! swap it stores the new release's manifest under `.dep/manifests/`, with each
//! file's size and mtime as they are on disk, so the next deploy can tell a
//! file that was edited in place from one it can reuse.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::config::Reuse;
use super::protocol::ArtifactFormat;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// Relative to the release root, `/`-separated, no `./` prefix.
    pub path: String,
    #[serde(flatten)]
    pub kind: Kind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Kind {
    File {
        mode: u32,
        size: u64,
        sha256: String,
        /// Seconds since the epoch, as on the host's disk. Only in a stored
        /// manifest; the runner's has none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mtime: Option<i64>,
    },
    Symlink {
        target: String,
    },
    Dir {
        mode: u32,
    },
}

/// Files under these prefixes are copied from the live release, never linked:
/// `bin/magento` commands may rewrite `config.php` in place.
const ALWAYS_COPY: &[&str] = &["app/etc/"];

pub fn encode(entries: &[Entry]) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(entries)?;
    Ok(zstd::encode_all(&json[..], 3)?)
}

pub fn decode(bytes: &[u8]) -> Result<Vec<Entry>> {
    let json = zstd::decode_all(bytes).context("decompressing the manifest")?;
    serde_json::from_slice(&json).context("parsing the manifest")
}

fn open_tar(path: &Path, format: ArtifactFormat) -> Result<tar::Archive<Box<dyn Read>>> {
    let f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let f = std::io::BufReader::with_capacity(1 << 20, f);
    let reader: Box<dyn Read> = match format {
        ArtifactFormat::Tar => Box::new(f),
        ArtifactFormat::TarGz => Box::new(flate2::read::MultiGzDecoder::new(f)),
        ArtifactFormat::TarZst => Box::new(zstd::Decoder::with_buffer(f)?),
    };
    Ok(tar::Archive::new(reader))
}

/// An entry's path relative to the release, or `None` for the root itself
/// (`tar -C . .` writes a `./` entry). Errors on anything that could leave it.
fn entry_path(e: &tar::Entry<'_, impl Read>) -> Result<Option<String>> {
    let raw = e.path()?;
    let mut parts = Vec::new();
    for c in raw.components() {
        match c {
            Component::CurDir => {}
            Component::Normal(p) => parts.push(
                p.to_str()
                    .with_context(|| format!("{} is not UTF-8", raw.display()))?,
            ),
            _ => bail!("{} leaves the release", raw.display()),
        }
    }
    Ok((!parts.is_empty()).then(|| parts.join("/")))
}

/// Read the manifest out of an artifact. `Ok(None)` when the artifact holds
/// something the manifest can't describe (hard links, devices, a path twice, a
/// path that isn't UTF-8), in which case the deploy sends the whole artifact.
pub fn from_artifact(path: &Path, format: ArtifactFormat) -> Result<Option<Vec<Entry>>> {
    let mut archive = open_tar(path, format)?;
    let mut entries = Vec::new();
    let mut seen = HashSet::new();
    for e in archive.entries()? {
        let mut e = e?;
        let Ok(rel) = entry_path(&e) else {
            return Ok(None);
        };
        let Some(rel) = rel else { continue };
        if !seen.insert(rel.clone()) {
            return Ok(None);
        }
        // Symlinks' modes mean nothing, and some writers leave the field blank.
        let mode = || e.header().mode().map(|m| m & 0o7777);
        let kind = match e.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                let mode = mode()?;
                let mut h = Sha256::new();
                let size = std::io::copy(&mut e, &mut HashWriter(&mut h))?;
                Kind::File {
                    mode,
                    size,
                    sha256: super::runner::hex(&h.finalize()),
                    mtime: None,
                }
            }
            tar::EntryType::Symlink => {
                let Some(target) = e.link_name()? else {
                    return Ok(None);
                };
                let Some(target) = target.to_str() else {
                    return Ok(None);
                };
                Kind::Symlink {
                    target: target.to_string(),
                }
            }
            tar::EntryType::Directory => Kind::Dir { mode: mode()? },
            _ => return Ok(None),
        };
        entries.push(Entry { path: rel, kind });
    }
    Ok(Some(entries))
}

struct HashWriter<'a>(&'a mut Sha256);

impl Write for HashWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A zstd tar of only the files at `wanted` (indices into the manifest
/// [`from_artifact`] read from the same artifact).
pub fn filtered(path: &Path, format: ArtifactFormat, wanted: &[u32]) -> Result<Vec<u8>> {
    let wanted: HashSet<u32> = wanted.iter().copied().collect();
    let mut archive = open_tar(path, format)?;
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    if threads > 1 {
        enc.multithread(threads)?;
    }
    let mut builder = tar::Builder::new(enc);
    let mut index = 0u32;
    let mut written = 0usize;
    for e in archive.entries()? {
        let e = e?;
        let Some(rel) = entry_path(&e)? else { continue };
        if wanted.contains(&index) {
            let mut header = e.header().clone();
            builder
                .append_data(&mut header, &rel, e)
                .with_context(|| format!("repacking {rel}"))?;
            written += 1;
        }
        index += 1;
    }
    if written != wanted.len() {
        bail!(
            "the agent asked for {} files, the artifact has {written} of them",
            wanted.len()
        );
    }
    Ok(builder.into_inner()?.finish()?)
}

/// For each entry of `new`, the live release's file it can reuse: same hash and
/// mode, and still the size and mtime its stored manifest recorded. The file at
/// the same path comes first; any other with the same content will do.
pub fn plan_reuse(new: &[Entry], live_root: &Path, live: &[Entry]) -> Vec<Option<String>> {
    type Stored<'a> = (&'a str, u64, Option<i64>);
    let mut by_path: HashMap<&str, (&str, u32, Stored)> = HashMap::new();
    let mut by_content: HashMap<(&str, u32), Vec<Stored>> = HashMap::new();
    for e in live {
        if let Kind::File {
            mode,
            size,
            sha256,
            mtime,
        } = &e.kind
        {
            let stored = (e.path.as_str(), *size, *mtime);
            by_path.insert(&e.path, (sha256, *mode, stored));
            by_content.entry((sha256, *mode)).or_default().push(stored);
        }
    }
    let usable = |(path, size, mtime): &Stored, mode: u32| {
        let m = fs::symlink_metadata(live_root.join(path)).ok()?;
        let unchanged = m.file_type().is_file()
            && m.len() == *size
            && Some(m.mtime()) == *mtime
            && m.mode() & 0o7777 == mode;
        unchanged.then(|| path.to_string())
    };
    new.par_iter()
        .map(|e| {
            let Kind::File { mode, sha256, .. } = &e.kind else {
                return None;
            };
            let same_path = by_path
                .get(e.path.as_str())
                .filter(|(h, m, _)| *h == sha256 && m == mode)
                .and_then(|(_, _, stored)| usable(stored, *mode));
            same_path.or_else(|| {
                by_content
                    .get(&(sha256.as_str(), *mode))?
                    .iter()
                    .find_map(|stored| usable(stored, *mode))
            })
        })
        .collect()
}

/// The indices of the files `reuse` found no source for.
pub fn needed(new: &[Entry], reuse: &[Option<String>]) -> Vec<u32> {
    new.iter()
        .zip(reuse)
        .enumerate()
        .filter(|(_, (e, r))| matches!(e.kind, Kind::File { .. }) && r.is_none())
        .map(|(i, _)| i as u32)
        .collect()
}

/// Everything in `release` except the files being sent: directories, then the
/// reused files linked or copied from `live_root`. Symlinks come later, in
/// [`finish`], so nothing is ever written through one.
pub fn materialize(
    release: &Path,
    new: &[Entry],
    reuse: &[Option<String>],
    live_root: &Path,
    how: Reuse,
) -> Result<()> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen = HashSet::new();
    for e in new {
        let p = Path::new(&e.path);
        let dir = match e.kind {
            Kind::Dir { .. } => Some(p),
            _ => p.parent(),
        };
        if let Some(d) = dir
            && !d.as_os_str().is_empty()
            && seen.insert(d)
        {
            dirs.push(release.join(d));
        }
    }
    for d in &dirs {
        fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
    }
    new.par_iter()
        .zip(reuse.par_iter())
        .try_for_each(|(e, src)| -> Result<()> {
            let Some(src) = src else { return Ok(()) };
            let (from, to) = (live_root.join(src), release.join(&e.path));
            let copy = how == Reuse::Copy || ALWAYS_COPY.iter().any(|p| e.path.starts_with(p));
            if copy || fs::hard_link(&from, &to).is_err() {
                fs::copy(&from, &to)
                    .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
            }
            Ok(())
        })
}

/// After the sent files are unpacked: create the symlinks, set directory
/// modes, check every file is there, and return the manifest to store, with
/// each file's on-disk size and mtime.
pub fn finish(release: &Path, new: &[Entry]) -> Result<Vec<Entry>> {
    for e in new {
        let p = release.join(&e.path);
        match &e.kind {
            Kind::Symlink { target } => {
                if let Ok(m) = fs::symlink_metadata(&p) {
                    if m.is_dir() {
                        fs::remove_dir_all(&p)?;
                    } else {
                        fs::remove_file(&p)?;
                    }
                }
                symlink(target, &p).with_context(|| format!("linking {}", p.display()))?;
            }
            Kind::Dir { mode } => fs::set_permissions(&p, fs::Permissions::from_mode(*mode))?,
            Kind::File { .. } => {}
        }
    }
    new.par_iter()
        .map(|e| {
            let Kind::File { sha256, .. } = &e.kind else {
                return Ok(e.clone());
            };
            let p = release.join(&e.path);
            let m = fs::symlink_metadata(&p)
                .with_context(|| format!("{} is missing after the upload", e.path))?;
            if !m.file_type().is_file() {
                bail!("{} is not a file after the upload", e.path);
            }
            Ok(Entry {
                path: e.path.clone(),
                kind: Kind::File {
                    mode: m.mode() & 0o7777,
                    size: m.len(),
                    sha256: sha256.clone(),
                    mtime: Some(m.mtime()),
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "magebuild-manifest-{name}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn file(builder: &mut tar::Builder<Vec<u8>>, path: &str, mode: u32, body: &[u8]) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_mode(mode);
        h.set_size(body.len() as u64);
        h.set_mtime(1_700_000_000);
        builder.append_data(&mut h, path, body).unwrap();
    }

    fn link(builder: &mut tar::Builder<Vec<u8>>, path: &str, target: &str) {
        let mut h = tar::Header::new_gnu();
        h.set_entry_type(tar::EntryType::Symlink);
        h.set_size(0);
        builder.append_link(&mut h, path, target).unwrap();
    }

    fn artifact(dir: &Path, name: &str, f: impl FnOnce(&mut tar::Builder<Vec<u8>>)) -> PathBuf {
        let mut b = tar::Builder::new(Vec::new());
        f(&mut b);
        let p = dir.join(name);
        fs::write(&p, b.into_inner().unwrap()).unwrap();
        p
    }

    /// One deploy as the agent does it: plan, materialize, unpack, finish.
    fn deploy(
        art: &Path,
        release: &Path,
        live: Option<(&Path, &[Entry])>,
    ) -> (Vec<u32>, Vec<Entry>) {
        let new = from_artifact(art, ArtifactFormat::Tar).unwrap().unwrap();
        let (root, live) = live.unwrap_or((Path::new("/nonexistent"), &[]));
        let reuse = plan_reuse(&new, root, live);
        let need = needed(&new, &reuse);
        fs::create_dir_all(release).unwrap();
        materialize(release, &new, &reuse, root, Reuse::Hardlink).unwrap();
        let sent = filtered(art, ArtifactFormat::Tar, &need).unwrap();
        let mut a = tar::Archive::new(zstd::Decoder::new(&sent[..]).unwrap());
        a.set_preserve_permissions(true);
        a.set_preserve_mtime(true);
        a.unpack(release).unwrap();
        (need, finish(release, &new).unwrap())
    }

    #[test]
    fn an_unchanged_rebuild_sends_nothing_and_links_everything() {
        let d = tempdir("unchanged");
        let build = |name| {
            artifact(&d, name, |b| {
                file(b, "./vendor/a/x.php", 0o644, b"x");
                file(b, "./vendor/b/x.php", 0o644, b"x");
                file(b, "./app/etc/config.php", 0o644, b"<?php return [];");
                file(b, "./bin/magento", 0o755, b"#!php");
                link(b, "./pub/static/img.png", "../../vendor/a/img.png");
            })
        };
        let (a1, a2) = (build("1.tar"), build("2.tar"));
        let (need1, stored1) = deploy(&a1, &d.join("r1"), None);
        assert_eq!(need1.len(), 4, "a first deploy sends every file");

        let (need2, _) = deploy(&a2, &d.join("r2"), Some((&d.join("r1"), &stored1)));
        assert!(need2.is_empty(), "sent {need2:?}");
        let ino = |r: &str, p: &str| fs::metadata(d.join(r).join(p)).unwrap().ino();
        assert_eq!(ino("r1", "vendor/a/x.php"), ino("r2", "vendor/a/x.php"));
        assert_eq!(ino("r1", "vendor/b/x.php"), ino("r2", "vendor/b/x.php"));
        assert_ne!(
            ino("r1", "app/etc/config.php"),
            ino("r2", "app/etc/config.php"),
            "app/etc is copied, never shared"
        );
        let mode = fs::metadata(d.join("r2/bin/magento")).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o755);
        assert_eq!(
            fs::read_link(d.join("r2/pub/static/img.png")).unwrap(),
            Path::new("../../vendor/a/img.png")
        );
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn only_changed_files_are_sent() {
        let d = tempdir("changed");
        let a1 = artifact(&d, "1.tar", |b| {
            file(b, "a.php", 0o644, b"one");
            file(b, "b.php", 0o644, b"same");
            file(b, "c.sh", 0o644, b"mode changes");
            link(b, "d", "a.php");
        });
        let a2 = artifact(&d, "2.tar", |b| {
            file(b, "a.php", 0o644, b"two");
            file(b, "moved/b.php", 0o644, b"same");
            file(b, "c.sh", 0o755, b"mode changes");
            // A symlink that becomes a file, and the reverse.
            file(b, "d", 0o644, b"now a file");
            link(b, "e", "moved/b.php");
        });
        let (_, stored1) = deploy(&a1, &d.join("r1"), None);
        let (need, stored2) = deploy(&a2, &d.join("r2"), Some((&d.join("r1"), &stored1)));
        let new = from_artifact(&a2, ArtifactFormat::Tar).unwrap().unwrap();
        let sent: Vec<&str> = need
            .iter()
            .map(|&i| new[i as usize].path.as_str())
            .collect();
        assert_eq!(sent, ["a.php", "c.sh", "d"]);
        assert_eq!(fs::read(d.join("r2/moved/b.php")).unwrap(), b"same");
        assert_eq!(fs::read(d.join("r2/d")).unwrap(), b"now a file");
        assert_eq!(
            fs::read_link(d.join("r2/e")).unwrap(),
            Path::new("moved/b.php")
        );
        assert_eq!(
            fs::read(d.join("r1/a.php")).unwrap(),
            b"one",
            "r1 untouched"
        );
        assert_eq!(
            fs::metadata(d.join("r1/c.sh")).unwrap().mode() & 0o777,
            0o644
        );
        assert_eq!(stored2.len(), new.len());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn a_file_edited_on_the_host_is_sent_again() {
        let d = tempdir("edited");
        let build = |name| artifact(&d, name, |b| file(b, "a.php", 0o644, b"original"));
        let (a1, a2) = (build("1.tar"), build("2.tar"));
        let (_, stored1) = deploy(&a1, &d.join("r1"), None);
        fs::write(d.join("r1/a.php"), b"edited!!").unwrap();
        let (need, _) = deploy(&a2, &d.join("r2"), Some((&d.join("r1"), &stored1)));
        assert_eq!(need, [0]);
        assert_eq!(fs::read(d.join("r2/a.php")).unwrap(), b"original");
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn artifacts_the_manifest_cannot_describe() {
        let d = tempdir("odd");
        let twice = artifact(&d, "twice.tar", |b| {
            file(b, "a", 0o644, b"1");
            file(b, "./a", 0o644, b"2");
        });
        assert!(
            from_artifact(&twice, ArtifactFormat::Tar)
                .unwrap()
                .is_none()
        );
        let hard = artifact(&d, "hard.tar", |b| {
            file(b, "a", 0o644, b"1");
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(tar::EntryType::Link);
            h.set_size(0);
            b.append_link(&mut h, "b", "a").unwrap();
        });
        assert!(from_artifact(&hard, ArtifactFormat::Tar).unwrap().is_none());
        fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn the_manifest_round_trips() {
        let e = vec![
            Entry {
                path: "a".into(),
                kind: Kind::File {
                    mode: 0o644,
                    size: 1,
                    sha256: "ab".into(),
                    mtime: Some(5),
                },
            },
            Entry {
                path: "b".into(),
                kind: Kind::Symlink { target: "a".into() },
            },
        ];
        assert_eq!(decode(&encode(&e).unwrap()).unwrap(), e);
    }
}
