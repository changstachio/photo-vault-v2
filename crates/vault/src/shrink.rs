use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::Path;
use vault_raw::recompress::recompress;
use vault_raw::Chroma;

use crate::config::{ArchiveMode, Config};
use crate::dates;
use crate::job::Ctx;
use crate::util::*;

/// JPGs larger than this are not loaded into memory for re-encoding (copied unchanged instead).
pub(crate) const MAX_JPG: u64 = 64 << 20;

struct Item {
    name: String,
    mt: Option<zip::DateTime>,
    data: Vec<u8>,
}

struct Done {
    name: String,
    hash: blake3::Hash,
    len: u64,
    dims: Option<(u16, u16)>,
}

pub(crate) fn is_jpg(name: &str) -> bool {
    matches!(name.rsplit('.').next().map(|e| e.to_lowercase()).as_deref(), Some("jpg" | "jpeg"))
}

fn opts(method: zip::CompressionMethod, mt: Option<zip::DateTime>) -> zip::write::SimpleFileOptions {
    let o = zip::write::SimpleFileOptions::default().compression_method(method).large_file(true);
    match mt {
        Some(d) => o.last_modified_time(d),
        None => o,
    }
}

/// Replacement for one JPG: `Some((new bytes, pixel size))`, or `None` to keep the original.
pub(crate) type Transform<'a> = &'a (dyn Fn(&[u8]) -> Option<(Vec<u8>, (u16, u16))> + Sync);

pub(crate) struct Rewrite {
    pub orig_len: u64,
    pub new_len: u64,
    pub changed: usize,
}

fn flush(pool: &rayon::ThreadPool, batch: &mut Vec<Item>, zw: &mut zip::ZipWriter<File>, f: Transform, done: &mut Vec<Done>, changed: &mut usize) -> Result<()> {
    if batch.is_empty() {
        return Ok(());
    }
    let res: Vec<Option<(Vec<u8>, (u16, u16))>> = pool.install(|| batch.par_iter().map(|it| f(&it.data)).collect());
    for (it, r) in batch.drain(..).zip(res) {
        let (bytes, dims) = match r {
            Some((b, d)) => {
                *changed += 1;
                (b, Some(d))
            }
            None => (it.data, None),
        };
        zw.start_file(it.name.clone(), opts(zip::CompressionMethod::Stored, it.mt))?;
        zw.write_all(&bytes)?;
        done.push(Done { name: it.name, hash: blake3::hash(&bytes), len: bytes.len() as u64, dims });
    }
    Ok(())
}

/// Rebuild `path` next to itself with JPG entries passed through `f` (everything else copied
/// unchanged, names, order and times kept), read the new zip back and check every entry, then
/// call `accept` to decide whether it replaces the original. The original is never modified
/// before the new zip has been verified.
pub(crate) fn rewrite_zip(ctx: &Ctx, pool: &rayon::ThreadPool, chunk: usize, path: &Path, f: Transform, accept: &dyn Fn(&Rewrite) -> bool) -> Result<Rewrite> {
    let fname = path.file_name().unwrap().to_string_lossy().into_owned();
    let orig_len = fs::metadata(path)?.len();
    let mut za = zip::ZipArchive::new(BufReader::new(File::open(path)?)).with_context(|| format!("opening {fname}"))?;
    let n = za.len();
    let tmp = path.with_file_name(format!(".{fname}.vshrink"));
    let r = (|| -> Result<Rewrite> {
        let mut zw = zip::ZipWriter::new(File::create(&tmp)?);
        let mut batch: Vec<Item> = Vec::new();
        let mut done: Vec<Done> = Vec::new();
        let mut changed = 0usize;
        for i in 0..n {
            if ctx.cancelled() {
                bail!("cancelled");
            }
            let mut e = za.by_index(i)?;
            let name = e.name()?.to_string();
            let mt = e.last_modified();
            if e.is_dir() {
                flush(pool, &mut batch, &mut zw, f, &mut done, &mut changed)?;
                zw.add_directory(name, opts(zip::CompressionMethod::Stored, mt))?;
                continue;
            }
            ctx.add_bytes(e.compressed_size());
            if is_jpg(&name) && e.size() <= MAX_JPG {
                let mut data = Vec::with_capacity(e.size() as usize);
                e.read_to_end(&mut data)?;
                batch.push(Item { name, mt, data });
                if batch.len() >= chunk.max(1) {
                    flush(pool, &mut batch, &mut zw, f, &mut done, &mut changed)?;
                }
            } else {
                // Not a JPG (or too big to hold in memory): copied through unchanged, order kept.
                flush(pool, &mut batch, &mut zw, f, &mut done, &mut changed)?;
                let method = if e.compression() == zip::CompressionMethod::Stored { zip::CompressionMethod::Stored } else { zip::CompressionMethod::Deflated };
                zw.start_file(name.clone(), opts(method, mt))?;
                let (mut h, mut len, mut buf) = (blake3::Hasher::new(), 0u64, vec![0u8; 1 << 20]);
                loop {
                    let k = e.read(&mut buf)?;
                    if k == 0 {
                        break;
                    }
                    h.update(&buf[..k]);
                    zw.write_all(&buf[..k])?;
                    len += k as u64;
                }
                done.push(Done { name, hash: h.finalize(), len, dims: None });
            }
        }
        flush(pool, &mut batch, &mut zw, f, &mut done, &mut changed)?;
        zw.finish()?.sync_all()?;

        let mut zv = zip::ZipArchive::new(BufReader::new(File::open(&tmp)?))?;
        if zv.len() != n {
            bail!("new zip has {} entries, expected {n}", zv.len());
        }
        for d in &done {
            let mut e = zv.by_name(&d.name).with_context(|| format!("{} missing from new zip", d.name))?;
            let mut bytes = Vec::new();
            e.read_to_end(&mut bytes)?; // the zip reader checks the CRC
            if bytes.len() as u64 != d.len || blake3::hash(&bytes) != d.hash {
                bail!("{} differs in new zip", d.name);
            }
            if let Some((w, h)) = d.dims {
                let info = vault_raw::probe_decode_bytes(&bytes).with_context(|| format!("{} does not decode", d.name))?;
                if info.width != w as u32 || info.height != h as u32 {
                    bail!("{} changed size", d.name);
                }
            }
        }
        let rw = Rewrite { orig_len, new_len: fs::metadata(&tmp)?.len(), changed };
        if rw.changed > 0 && accept(&rw) {
            fs::rename(&tmp, path)?;
        } else {
            let _ = fs::remove_file(&tmp);
        }
        Ok(rw)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// Task 5: re-encode the JPGs inside the folders and zips chosen as `shrink`, at the configured
/// quality/chroma. A JPG is replaced only if the new one is at least `shrink_min_saving_percent`
/// smaller; EXIF, XMP, ICC and file times are kept. A zip is rebuilt next to the original, read back
/// and checked, and only then renamed over the original.
pub fn run(cfg: &Config, ctx: &Ctx) -> Result<()> {
    let root = &cfg.paths.local_backup;
    if !root.is_dir() {
        bail!("local backup folder {} not found", root.display());
    }
    let (units, unit_errs) = dates::units_lossy(root)?;
    for e in &unit_errs {
        ctx.log(e);
    }
    let units: Vec<_> = units.into_iter().filter(|u| cfg.archive_mode(&u.path) == ArchiveMode::Shrink).collect();
    let size_of = |u: &dates::Unit| if u.is_zip { fs::metadata(&u.path).map(|m| m.len()).unwrap_or(0) } else { list_files(&u.path).map(|f| f.iter().map(|x| x.size).sum()).unwrap_or(0) };
    let total: u64 = units.iter().map(size_of).sum();
    ctx.log(format!("{} project(s) selected for shrinking, {} (quality {}, chroma {})", units.len(), fmt_bytes(total), cfg.archive.render.quality, cfg.archive.render.chroma.as_str()));
    if units.is_empty() {
        ctx.log("nothing selected: choose \"shrink JPGs\" for folders or zips under \"Convert / zip selection\"");
    }
    ctx.reset_progress(units.len() as u64, total);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cfg.archive.threads).build()?;
    let (mut problems, mut saved) = (unit_errs.len() as u32, 0u64);
    for u in units {
        if ctx.cancelled() {
            ctx.log("cancelled");
            break;
        }
        if ctx.dry {
            ctx.log(format!("{}: would shrink ({})", u.path.display(), fmt_bytes(size_of(&u))));
            ctx.add_file();
            continue;
        }
        let r = if u.is_zip { shrink_zip(cfg, ctx, &pool, &u.path) } else { shrink_dir(cfg, ctx, &pool, &u.path) };
        match r {
            Ok(s) => saved += s,
            Err(e) => {
                problems += 1;
                ctx.log(format!("{}: FAILED: {e:#}", u.name));
            }
        }
        ctx.add_file();
    }
    ctx.log(format!("shrink done: {} saved, {problems} problem(s)", fmt_bytes(saved)));
    if problems > 0 {
        bail!("{problems} problem(s), see log");
    }
    Ok(())
}

/// Re-encoded JPG to use instead of `data`, if any: when it is at least `min` % smaller, or always
/// when `data` itself is broken (decoders disagree). Returns (bytes, pixel size) and whether
/// `data` was broken.
pub(crate) fn shrink_bytes(data: &[u8], q: u8, ch: Chroma, min: f32) -> Option<((Vec<u8>, (u16, u16)), bool)> {
    let broken = vault_raw::subsampled_baseline(data) && vault_raw::decoders_disagree(data).unwrap_or(false);
    let limit = data.len() as f64 * (1.0 - min as f64 / 100.0);
    match recompress(data, q, ch) {
        Ok((b, w, h)) if broken || (b.len() as f64) <= limit => Some(((b, (w, h)), broken)),
        Ok(_) => None,
        // A broken file that cannot be re-encoded is repaired losslessly if jpegtran is installed.
        Err(_) if broken => crate::repair::fix(data).ok().flatten().and_then(|b| vault_raw::probe_decode_bytes(&b).ok().map(|i| ((b, (i.width as u16, i.height as u16)), true))),
        Err(_) => None,
    }
}

fn shrink_zip(cfg: &Config, ctx: &Ctx, pool: &rayon::ThreadPool, path: &Path) -> Result<u64> {
    let (q, ch, min) = (cfg.archive.render.quality, cfg.archive.render.chroma, cfg.archive.shrink_min_saving_percent);
    let fname = path.file_name().unwrap().to_string_lossy().into_owned();
    let fixed = std::sync::atomic::AtomicUsize::new(0);
    let f = |data: &[u8]| -> Option<(Vec<u8>, (u16, u16))> {
        let (new, was_broken) = shrink_bytes(data, q, ch, min)?;
        if was_broken {
            fixed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Some(new)
    };
    let pct = |r: &Rewrite| 100.0 * r.orig_len.saturating_sub(r.new_len) as f64 / r.orig_len.max(1) as f64;
    // Broken JPGs (shown as green stripes by some viewers) are always replaced, even if bigger.
    let accept = |r: &Rewrite| fixed.load(std::sync::atomic::Ordering::Relaxed) > 0 || (r.new_len < r.orig_len && pct(r) >= min as f64);
    let r = rewrite_zip(ctx, pool, cfg.archive.threads * 2, path, &f, &accept)?;
    let nfixed = fixed.load(std::sync::atomic::Ordering::Relaxed);
    if r.changed > 0 && accept(&r) {
        let extra = if nfixed > 0 { format!(", {nfixed} broken JPG(s) replaced") } else { String::new() };
        ctx.log(format!("{fname}: {} -> {} ({:.0}% smaller{extra})", fmt_bytes(r.orig_len), fmt_bytes(r.new_len), pct(&r)));
        Ok(r.orig_len.saturating_sub(r.new_len))
    } else {
        ctx.log(format!("{fname}: {} -> {} ({:.1}% smaller), below {min}%: left unchanged", fmt_bytes(r.orig_len), fmt_bytes(r.new_len), pct(&r)));
        Ok(0)
    }
}

fn shrink_dir(cfg: &Config, ctx: &Ctx, pool: &rayon::ThreadPool, dir: &Path) -> Result<u64> {
    let (q, ch, min) = (cfg.archive.render.quality, cfg.archive.render.chroma, cfg.archive.shrink_min_saving_percent);
    let files: Vec<FileInfo> = list_files(dir)?.into_iter().filter(|f| is_jpg(&f.rel.to_string_lossy()) && f.size <= MAX_JPG).collect();
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let (mut before, mut after) = (0u64, 0u64);
    for chunk in files.chunks(cfg.archive.threads * 2) {
        if ctx.cancelled() {
            bail!("cancelled");
        }
        let res: Vec<Result<(u64, u64)>> = pool.install(|| chunk.par_iter().map(|f| shrink_file(f, q, ch, min)).collect());
        for (f, r) in chunk.iter().zip(res) {
            ctx.add_bytes(f.size);
            match r {
                Ok((b, a)) => {
                    before += b;
                    after += a;
                }
                Err(e) => ctx.log(format!("{name}/{}: {e:#} (kept as is)", f.rel.display())),
            }
        }
    }
    ctx.log(format!("{name}: {} -> {}", fmt_bytes(before), fmt_bytes(after)));
    Ok(before.saturating_sub(after))
}

/// Replace one JPG in place if the re-encoded one is small enough; returns (old, new) sizes.
fn shrink_file(f: &FileInfo, q: u8, ch: Chroma, min: f32) -> Result<(u64, u64)> {
    let data = fs::read(&f.abs)?;
    let old = data.len() as u64;
    let Some(((b, (w, h)), _)) = shrink_bytes(&data, q, ch, min) else {
        return Ok((old, old));
    };
    let info = vault_raw::probe_decode_bytes(&b)?;
    if info.width != w as u32 || info.height != h as u32 {
        bail!("re-encoded JPG has the wrong size");
    }
    // The file must be unchanged since it was read.
    let md = fs::metadata(&f.abs)?;
    if md.len() != f.size || md.modified()? != f.mtime {
        bail!("changed while shrinking");
    }
    let n = f.abs.file_name().unwrap().to_string_lossy().into_owned();
    let tmp = f.abs.with_file_name(format!(".{n}.vshrink"));
    let r = (|| -> Result<()> {
        let mut t = File::create(&tmp)?;
        t.write_all(&b)?;
        t.set_modified(f.mtime)?;
        t.sync_all()?;
        if hash_file(&tmp)? != blake3::hash(&b) {
            bail!("write verification failed");
        }
        fs::rename(&tmp, &f.abs)?;
        Ok(())
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r?;
    Ok((old, b.len() as u64))
}
