use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::mpsc::sync_channel;
use std::time::{Duration, SystemTime};
use walkdir::WalkDir;

use crate::job::Ctx;

#[derive(Clone, Debug)]
pub struct FileInfo {
    pub abs: PathBuf,
    pub rel: PathBuf,
    pub size: u64,
    pub mtime: SystemTime,
}

/// All regular files under `root` (symlinks skipped, never followed), sorted by path.
pub fn list_files(root: &Path) -> Result<Vec<FileInfo>> {
    let mut v = Vec::new();
    for e in WalkDir::new(root).follow_links(false).sort_by_file_name() {
        let e = e.with_context(|| format!("walking {}", root.display()))?;
        if !e.file_type().is_file() {
            continue;
        }
        let md = e.metadata()?;
        v.push(FileInfo {
            rel: e.path().strip_prefix(root)?.to_path_buf(),
            abs: e.path().to_path_buf(),
            size: md.len(),
            mtime: md.modified()?,
        });
    }
    Ok(v)
}

pub fn is_plain_name(s: &str) -> bool {
    !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\', '\0'])
}

/// Join `rel` to `root`, refusing anything that could leave `root`.
pub fn safe_join(root: &Path, rel: &Path) -> Result<PathBuf> {
    for c in rel.components() {
        if !matches!(c, Component::Normal(_)) {
            bail!("unsafe relative path {}", rel.display());
        }
    }
    Ok(root.join(rel))
}

pub fn ext_lower(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

pub fn hash_file(p: &Path) -> Result<blake3::Hash> {
    let mut f = File::open(p).with_context(|| format!("open {}", p.display()))?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 4 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize())
}

fn tmp_name(dst: &Path) -> PathBuf {
    let n = dst.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    dst.with_file_name(format!(".{n}.vtmp"))
}

/// Copy `src` to `dst` (never overwrites): a reader thread reads and hashes while this thread
/// writes, into a temp file that is renamed on success. Fails if the source changed while it was read.
pub fn copy_hashed(src: &Path, dst: &Path, buf_bytes: usize, ctx: &Ctx) -> Result<(blake3::Hash, u64)> {
    if dst.exists() {
        bail!("{} already exists", dst.display());
    }
    if let Some(d) = dst.parent() {
        fs::create_dir_all(d)?;
    }
    let tmp = tmp_name(dst);
    let r = copy_inner(src, dst, &tmp, buf_bytes, ctx);
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

fn copy_inner(src: &Path, dst: &Path, tmp: &Path, buf_bytes: usize, ctx: &Ctx) -> Result<(blake3::Hash, u64)> {
    let mut input = File::open(src).with_context(|| format!("open {}", src.display()))?;
    let md0 = input.metadata()?;
    let mut out = File::create(tmp).with_context(|| format!("create {}", tmp.display()))?;
    let (tx, rx) = sync_channel::<Vec<u8>>(4);
    let mut total = 0u64;
    let hash = std::thread::scope(|s| -> Result<blake3::Hash> {
        let reader = s.spawn(move || -> Result<blake3::Hash> {
            let mut h = blake3::Hasher::new();
            loop {
                let mut buf = vec![0u8; buf_bytes];
                let mut n = 0;
                while n < buf.len() {
                    let r = input.read(&mut buf[n..])?;
                    if r == 0 {
                        break;
                    }
                    n += r;
                }
                if n == 0 {
                    break;
                }
                buf.truncate(n);
                h.update(&buf);
                if tx.send(buf).is_err() {
                    break;
                }
            }
            Ok(h.finalize())
        });
        let mut werr = None;
        for chunk in rx.iter() {
            if let Err(e) = out.write_all(&chunk) {
                werr = Some(e);
                break;
            }
            total += chunk.len() as u64;
            ctx.add_bytes(chunk.len() as u64);
            if ctx.cancelled() {
                werr = Some(std::io::Error::other("cancelled"));
                break;
            }
        }
        drop(rx);
        let h = reader.join().map_err(|_| anyhow::anyhow!("reader thread panicked"))??;
        match werr {
            Some(e) => Err(e.into()),
            None => Ok(h),
        }
    })?;
    if total != md0.len() {
        bail!("{} changed while copying ({} of {} bytes)", src.display(), total, md0.len());
    }
    let md1 = fs::metadata(src)?;
    if md1.len() != md0.len() || md1.modified()? != md0.modified()? {
        bail!("{} changed while copying", src.display());
    }
    out.set_modified(md0.modified()?)?;
    out.sync_all()?;
    drop(out);
    fs::rename(tmp, dst)?;
    Ok((hash, total))
}

/// Same size and modified time (2 s tolerance for FAT/SMB timestamp granularity).
pub fn same_stamp(a: &FileInfo, dst: &Path) -> bool {
    match fs::metadata(dst) {
        Ok(m) => {
            m.len() == a.size
                && m.modified().map(|t| diff(t, a.mtime) <= Duration::from_secs(2)).unwrap_or(false)
        }
        Err(_) => false,
    }
}

fn diff(a: SystemTime, b: SystemTime) -> Duration {
    a.duration_since(b).unwrap_or_else(|e| e.duration())
}

/// Remove exactly the listed files, then any directories left empty (non-empty ones stay).
pub fn remove_files_and_empty_dirs(root: &Path, files: &[FileInfo]) -> Result<()> {
    for f in files {
        match fs::remove_file(&f.abs) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).with_context(|| format!("delete {}", f.abs.display())),
        }
    }
    let mut dirs: Vec<PathBuf> = WalkDir::new(root).contents_first(true).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_dir()).map(|e| e.into_path()).collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    for d in dirs {
        let _ = fs::remove_dir(&d);
    }
    Ok(())
}

pub fn fmt_bytes(n: u64) -> String {
    let n = n as f64;
    if n >= 1e12 {
        format!("{:.2} TB", n / 1e12)
    } else if n >= 1e9 {
        format!("{:.2} GB", n / 1e9)
    } else if n >= 1e6 {
        format!("{:.1} MB", n / 1e6)
    } else {
        format!("{n:.0} B")
    }
}

pub fn is_year(s: &str) -> bool {
    s.len() == 4 && s.starts_with(['1', '2']) && s.chars().all(|c| c.is_ascii_digit())
}

/// True if `p` is a mount point (its device differs from its parent's).
pub fn is_mount(p: &Path) -> Result<bool> {
    use std::os::unix::fs::MetadataExt;
    let a = fs::metadata(p).with_context(|| format!("stat {}", p.display()))?;
    let parent = p.parent().unwrap_or(Path::new("/"));
    let b = fs::metadata(if parent.as_os_str().is_empty() { Path::new(".") } else { parent })?;
    Ok(a.dev() != b.dev() || a.ino() == b.ino())
}

/// A single file as a one-element list (rel = its file name).
pub fn single_file(p: &Path) -> Result<Vec<FileInfo>> {
    let md = fs::symlink_metadata(p)?;
    if !md.file_type().is_file() {
        bail!("{} is not a regular file", p.display());
    }
    Ok(vec![FileInfo { abs: p.to_path_buf(), rel: PathBuf::from(p.file_name().context("no file name")?), size: md.len(), mtime: md.modified()? }])
}

/// Decode `%XX` and `+` in a URL query value.
pub fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) => {
                    out.push(v);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
