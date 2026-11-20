//! What a release contains, entry by entry, so a deploy sends only the files
//! the host doesn't already have.
//!
//! The build's package step hashes each file as it archives it and writes the
//! manifest beside the artifact (see [`sidecar_path`]). The runner
//! uses that file when it matches the artifact, and otherwise reads the
//! manifest out of the artifact (one pass, hashing each file). It sends the
//! manifest to the agent. The agent matches each file against the
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

/// The first line of a manifest file beside an artifact. The rest of the line
/// is the artifact's length and sha256, the rest of the file [`encode`]'s bytes.
const SIDECAR_MAGIC: &str = "magebuild-manifest 1";

/// Where the package step writes the manifest of `artifact`:
/// `artifact.tar.zst` → `artifact.tar.zst.manifest`.
pub fn sidecar_path(artifact: &Path) -> PathBuf {
    let mut s = artifact.as_os_str().to_owned();
    s.push(".manifest");
    PathBuf::from(s)
}

/// The length and sha256 of a file, read in 1 MB blocks.
pub fn hash_file(path: &Path) -> Result<(u64, String)> {
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let len = std::io::copy(&mut f, &mut HashWriter(&mut h))
        .with_context(|| format!("reading {}", path.display()))?;
    Ok((len, super::runner::hex(&h.finalize())))
}

/// Write `entries` beside `artifact`, tied to its bytes (their length and
/// sha256, which the package step computes as it writes them) so a manifest
/// left over from another build is never used for this one.
pub fn write_sidecar(artifact: &Path, len: u64, sha: &str, entries: &[Entry]) -> Result<()> {
    let mut out = format!("{SIDECAR_MAGIC} {len} {sha}\n").into_bytes();
    out.extend(encode(entries)?);
    let path = sidecar_path(artifact);
    let tmp = sidecar_path(&path);
    fs::write(&tmp, &out).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))
}

/// What [`read_sidecar`] found.
pub enum Sidecar {
    /// The manifest, and its [`encode`]d bytes to send as they are.
    Found(Vec<Entry>, Vec<u8>),
    Missing,
    /// There is a file, but it isn't this artifact's manifest.
    Unusable(String),
}

/// The manifest the package step wrote beside `artifact`, if it describes
/// exactly these bytes.
pub fn read_sidecar(artifact: &Path) -> Result<Sidecar> {
    let path = sidecar_path(artifact);
    let bytes = match fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Sidecar::Missing),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let Some(nl) = bytes.iter().position(|&b| b == b'\n') else {
        return Ok(Sidecar::Unusable("it has no header".into()));
    };
    let header = String::from_utf8_lossy(&bytes[..nl]);
    let Some(rest) = header
        .strip_prefix(SIDECAR_MAGIC)
        .and_then(|r| r.strip_prefix(' '))
    else {
        return Ok(Sidecar::Unusable(format!("unknown header {header:?}")));
    };
    let (len, sha) = rest.split_once(' ').unwrap_or((rest, ""));
    let (actual_len, actual_sha) = hash_file(artifact)?;
    if len != actual_len.to_string() || sha != actual_sha {
        return Ok(Sidecar::Unusable(
            "it was written for a different artifact".into(),
        ));
    }
    let encoded = bytes[nl + 1..].to_vec();
    match decode(&encoded) {
        Ok(entries) => Ok(Sidecar::Found(entries, encoded)),
        Err(e) => Ok(Sidecar::Unusable(format!("{e:#}"))),
    }
}

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

/// A zstd tar of only the files at `wanted`, indices into `entries`, the
/// artifact's manifest. Each file's path must match its manifest entry.
pub fn filtered(
    path: &Path,
    format: ArtifactFormat,
    entries: &[Entry],
    wanted: &[u32],
) -> Result<Vec<u8>> {
    let wanted: HashSet<u32> = wanted.iter().copied().collect();
    let mut archive = open_tar(path, format)?;
    let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3)?;
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get() as u32);
    if threads > 1 {
        enc.multithread(threads)?;
    }
    let mut builder = tar::Builder::new(enc);
    let last = wanted.iter().copied().max();
    let mut index = 0u32;
    let mut written = 0usize;
    for e in archive.entries()? {
        // The archive is read in order, so stop after the last file wanted.
        if last.is_none_or(|l| index > l) {
            break;
        }
        let e = e?;
        let Some(rel) = entry_path(&e)? else { continue };
        if wanted.contains(&index) {
            let expected = entries.get(index as usize).map(|e| e.path.as_str());
            if expected != Some(rel.as_str()) {
                bail!(
                    "entry {index} of the artifact is {rel}, but the manifest says {}",
                    expected.unwrap_or("nothing")
                );
            }
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
    plan_reuse_except(new, live_root, live, None)
}

/// [`plan_reuse`] for the entries `skip` doesn't mark; marked ones get `None`.
pub fn plan_reuse_except(
    new: &[Entry],
    live_root: &Path,
    live: &[Entry],
    skip: Option<&[bool]>,
) -> Vec<Option<String>> {
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
        .enumerate()
        .map(|(i, e)| {
            if skip.is_some_and(|s| s[i]) {
                return None;
            }
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
/// [`finish`], so nothing is ever written through one. The agent runs this
/// while the sent files are unpacked into the same release: the two never
/// touch the same file, and both create directories only with
/// `create_dir_all`, which tolerates the other creating one first.
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
/// each file's on-disk size and mtime. `in_place` is what [`reconcile`] found
/// already in place in a staged copy: those entries were just checked, so
/// they are taken as they are instead of being looked at again.
pub fn finish(
    release: &Path,
    new: &[Entry],
    in_place: Option<&[Option<&Entry>]>,
) -> Result<Vec<Entry>> {
    let placed = |i: usize| in_place.and_then(|p| p[i]);
    for (i, e) in new.iter().enumerate() {
        if placed(i).is_some() {
            continue;
        }
        let p = release.join(&e.path);
        match &e.kind {
            Kind::Symlink { target } => {
                // A staged release already has most of its symlinks.
                if fs::read_link(&p).is_ok_and(|t| t == Path::new(target)) {
                    continue;
                }
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
        .enumerate()
        .map(|(i, e)| {
            if let Some(p) = placed(i) {
                return Ok(p.clone());
            }
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

/// The name of the copy of release `live` that is prepared for the next
/// deploy, beside the releases.
pub fn staged_name(live: &str) -> String {
    format!(".next-{live}")
}

/// Build a copy of the live release in `dir`: its files hard-linked (or
/// copied) and its symlinks, everything the artifact had and nothing the
/// deploy added (shared links, the derived env.php). Returns the manifest of
/// what is in `dir`, with each file's size and mtime on disk. A file that was
/// edited since its deploy is left out, so the next deploy links or sends it.
pub fn stage(dir: &Path, live_root: &Path, live: &[Entry], how: Reuse) -> Result<Vec<Entry>> {
    let reuse = plan_reuse(live, live_root, live);
    let (present, reuse): (Vec<Entry>, Vec<Option<String>>) = live
        .iter()
        .zip(reuse)
        .filter(|(e, r)| !matches!(e.kind, Kind::File { .. }) || r.is_some())
        .map(|(e, r)| (e.clone(), r))
        .unzip();
    materialize(dir, &present, &reuse, live_root, how)?;
    finish(dir, &present, None)
}

/// Turn `release`, a staged copy whose contents `present` describes, into
/// `new`'s starting point: remove every entry `new` doesn't have exactly as it
/// is, and every directory `new` has no use for. Returns, for each of `new`'s
/// entries, the entry as it is on disk when it is already in place. A file
/// only counts when it still has the size, mtime and mode `present` recorded,
/// the same check the live release's files get before they are linked; a
/// directory when it has the mode `new` wants.
pub fn reconcile<'a>(
    release: &Path,
    new: &[Entry],
    present: &'a [Entry],
) -> Result<Vec<Option<&'a Entry>>> {
    let by_path: HashMap<&str, &Entry> = present.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut required: HashSet<&str> = HashSet::new();
    for e in new {
        if matches!(e.kind, Kind::Dir { .. }) {
            required.insert(&e.path);
        }
        let mut p = e.path.as_str();
        while let Some((parent, _)) = p.rsplit_once('/') {
            required.insert(parent);
            p = parent;
        }
    }
    let keep: Vec<Option<&Entry>> = new
        .par_iter()
        .map(|e| {
            let old: &Entry = by_path.get(e.path.as_str())?;
            let in_place = match (&e.kind, &old.kind) {
                (
                    Kind::File { mode, sha256, .. },
                    Kind::File {
                        mode: m,
                        size,
                        sha256: h,
                        mtime,
                    },
                ) => {
                    sha256 == h
                        && mode == m
                        && fs::symlink_metadata(release.join(&e.path)).is_ok_and(|md| {
                            md.file_type().is_file()
                                && md.len() == *size
                                && Some(md.mtime()) == *mtime
                                && md.mode() & 0o7777 == *mode
                        })
                }
                (Kind::Symlink { target }, Kind::Symlink { target: t }) => target == t,
                (Kind::Dir { mode }, Kind::Dir { mode: m }) => mode == m,
                _ => false,
            };
            // `old` has the on-disk size and mtime that were just checked.
            in_place.then_some(old)
        })
        .collect();
    let kept: HashSet<&str> = new
        .iter()
        .zip(&keep)
        .filter(|(_, k)| k.is_some())
        .map(|(e, _)| e.path.as_str())
        .collect();
    // Files and symlinks first, then directories nothing needs (with whatever
    // is left in them). A directory `new` requires never holds a removed entry
    // that matters: what is still in it is either kept or replaced later.
    present
        .par_iter()
        .filter(|e| !matches!(e.kind, Kind::Dir { .. }) && !kept.contains(e.path.as_str()))
        .try_for_each(|e| match fs::remove_file(release.join(&e.path)) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                Err(err).with_context(|| format!("removing {} from the staged release", e.path))
            }
            _ => Ok(()),
        })?;
    // Every directory the staged copy has, ancestors included.
    let mut dirs: HashSet<&str> = HashSet::new();
    for e in present {
        if matches!(e.kind, Kind::Dir { .. }) {
            dirs.insert(&e.path);
        }
        let mut p = e.path.as_str();
        while let Some((parent, _)) = p.rsplit_once('/') {
            dirs.insert(parent);
            p = parent;
        }
    }
    // Sorted, so a directory goes before anything beneath it.
    let mut dirs: Vec<&str> = dirs.into_iter().filter(|d| !required.contains(d)).collect();
    dirs.sort_unstable();
    for d in dirs {
        match fs::remove_dir_all(release.join(d)) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => {
                return Err(err).with_context(|| format!("removing {d}/ from the staged release"));
            }
            _ => {}
        }
    }
    Ok(keep)
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

    /// One deploy as the agent does it: plan, then materialize while the sent
    /// files unpack, then finish.
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
        let sent = filtered(art, ArtifactFormat::Tar, &new, &need).unwrap();
        // Linking runs while the sent files unpack, as in the agent.
        std::thread::scope(|s| {
            let linking = s.spawn(|| materialize(release, &new, &reuse, root, Reuse::Hardlink));
            let mut a = tar::Archive::new(zstd::Decoder::new(&sent[..]).unwrap());
            a.set_preserve_permissions(true);
            a.set_preserve_mtime(true);
            a.unpack(release).unwrap();
            linking.join().unwrap().unwrap();
        });
        (need, finish(release, &new, None).unwrap())
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
    fn a_manifest_that_does_not_match_the_artifact_is_refused() {
        let d = tempdir("mismatch");
        let art = artifact(&d, "1.tar", |b| {
            file(b, "a.php", 0o644, b"a");
            file(b, "b.php", 0o644, b"b");
        });
        let mut wrong = from_artifact(&art, ArtifactFormat::Tar).unwrap().unwrap();
        wrong.swap(0, 1);
        let err = filtered(&art, ArtifactFormat::Tar, &wrong, &[0]).unwrap_err();
        assert!(err.to_string().contains("the manifest says b.php"), "{err}");
        fs::remove_dir_all(&d).unwrap();
    }

    /// Every path under `dir`, with what it is, not following symlinks.
    fn tree(dir: &Path) -> std::collections::BTreeMap<String, String> {
        fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, String>) {
            for e in fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                let rel = p.strip_prefix(root).unwrap().to_str().unwrap().to_string();
                let m = fs::symlink_metadata(&p).unwrap();
                if m.file_type().is_symlink() {
                    out.insert(rel, format!("-> {}", fs::read_link(&p).unwrap().display()));
                } else if m.is_dir() {
                    out.insert(rel, "dir".into());
                    walk(root, &p, out);
                } else {
                    let body = String::from_utf8_lossy(&fs::read(&p).unwrap()).into_owned();
                    out.insert(rel, format!("{:o} {body}", m.mode() & 0o777));
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    #[test]
    fn a_staged_copy_becomes_the_next_release_with_only_the_changes() {
        let d = tempdir("staged");
        let a1 = artifact(&d, "1.tar", |b| {
            file(b, "a.php", 0o644, b"one");
            file(b, "b.php", 0o644, b"same");
            file(b, "keep/x.php", 0o644, b"x");
            file(b, "gone/deep/old.php", 0o644, b"old");
            file(b, "c.sh", 0o644, b"mode changes");
            file(b, "d", 0o644, b"becomes a link");
            link(b, "e", "a.php");
            file(b, "g", 0o644, b"becomes a dir");
            file(b, "app/etc/config.php", 0o644, b"<?php return [];");
        });
        let a2 = artifact(&d, "2.tar", |b| {
            file(b, "a.php", 0o644, b"two");
            file(b, "b.php", 0o644, b"same");
            file(b, "keep/x.php", 0o644, b"x");
            file(b, "c.sh", 0o755, b"mode changes");
            link(b, "d", "b.php");
            file(b, "e", 0o644, b"now a file");
            file(b, "g/inner.php", 0o644, b"inner");
            file(b, "new/n.php", 0o644, b"new");
            file(b, "app/etc/config.php", 0o644, b"<?php return [];");
        });
        let r1 = d.join("r1");
        let (_, stored1) = deploy(&a1, &r1, None);

        // What the cleanup does after release 1, then an edit on the host.
        let staged = d.join(".next-1");
        fs::create_dir(&staged).unwrap();
        let present = stage(&staged, &r1, &stored1, Reuse::Hardlink).unwrap();
        assert_eq!(tree(&staged), tree(&r1));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(r1.join("keep/x.php"), b"edited!").unwrap();

        // The next deploy, as the agent does it.
        let r2 = d.join("r2");
        fs::rename(&staged, &r2).unwrap();
        let new = from_artifact(&a2, ArtifactFormat::Tar).unwrap().unwrap();
        let placed = reconcile(&r2, &new, &present).unwrap();
        let keep: Vec<bool> = placed.iter().map(Option::is_some).collect();
        let reuse = plan_reuse_except(&new, &r1, &stored1, Some(&keep));
        let mut need = needed(&new, &reuse);
        need.retain(|&i| !keep[i as usize]);
        let sent: Vec<&str> = need
            .iter()
            .map(|&i| new[i as usize].path.as_str())
            .collect();
        assert_eq!(
            sent,
            [
                "a.php",
                "keep/x.php",
                "c.sh",
                "e",
                "g/inner.php",
                "new/n.php"
            ]
        );
        let tar = filtered(&a2, ArtifactFormat::Tar, &new, &need).unwrap();
        std::thread::scope(|s| {
            let linking = s.spawn(|| materialize(&r2, &new, &reuse, &r1, Reuse::Hardlink));
            let mut a = tar::Archive::new(zstd::Decoder::new(&tar[..]).unwrap());
            a.set_preserve_permissions(true);
            a.set_overwrite(true);
            a.unpack(&r2).unwrap();
            linking.join().unwrap().unwrap();
        });
        let stored2 = finish(&r2, &new, Some(&placed)).unwrap();
        // What it records without looking again is what is on disk.
        assert_eq!(stored2, finish(&r2, &new, None).unwrap());

        // Exactly what a fresh deploy of the artifact gives, nothing left over.
        let fresh = d.join("fresh");
        deploy(&a2, &fresh, None);
        assert_eq!(tree(&r2), tree(&fresh));
        let ino = |r: &Path, p: &str| fs::metadata(r.join(p)).unwrap().ino();
        assert_eq!(
            ino(&r1, "b.php"),
            ino(&r2, "b.php"),
            "unchanged files stay linked"
        );
        assert_ne!(
            ino(&r1, "app/etc/config.php"),
            ino(&r2, "app/etc/config.php")
        );
        assert_eq!(fs::read(r1.join("a.php")).unwrap(), b"one", "r1 untouched");
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
