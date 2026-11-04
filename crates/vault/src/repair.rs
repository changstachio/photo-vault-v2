use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use walkdir::WalkDir;

use crate::config::Config;
use crate::job::Ctx;
use crate::shrink::{is_jpg, rewrite_zip, MAX_JPG};
use crate::util::*;

/// Lossless rewrite of the entropy coding with libjpeg's own tables (`jpegtran -optimize`).
fn jpegtran(data: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("jpegtran")
        .args(["-copy", "all", "-optimize"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run jpegtran (install it: sudo apt install libjpeg-turbo-progs)")?;
    let mut stdin = child.stdin.take().unwrap();
    let input = data.to_vec();
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let out = child.wait_with_output()?;
    writer.join().map_err(|_| anyhow::anyhow!("writer thread panicked"))??;
    if !out.status.success() {
        bail!("jpegtran failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

/// `Some(repaired bytes)` if `data` is a JPG that the decoders disagree on and jpegtran fixes
/// it losslessly (pixels identical in the reference decoder, and both decoders now agree).
pub fn fix(data: &[u8]) -> Result<Option<Vec<u8>>> {
    if !vault_raw::subsampled_baseline(data) || !vault_raw::decoders_disagree(data)? {
        return Ok(None);
    }
    let new = jpegtran(data)?;
    if vault_raw::reference_pixels(&new)? != vault_raw::reference_pixels(data)? {
        bail!("jpegtran output is not pixel-identical; left as is");
    }
    if vault_raw::decoders_disagree(&new)? {
        bail!("still decodes differently after jpegtran; left as is");
    }
    Ok(Some(new))
}

fn broken(data: &[u8]) -> bool {
    vault_raw::subsampled_baseline(data) && vault_raw::decoders_disagree(data).unwrap_or(false)
}

/// Task 6: find JPGs (loose or inside zips) under the local backup that some image viewers show as
/// garbage (written by this tool's 4:2:0 encoder before the fix) and repair them losslessly.
pub fn run(cfg: &Config, ctx: &Ctx) -> Result<()> {
    let root = &cfg.paths.local_backup;
    if !root.is_dir() {
        bail!("local backup folder {} not found", root.display());
    }
    if Command::new("jpegtran").arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_err() {
        bail!("jpegtran is not installed: sudo apt install libjpeg-turbo-progs");
    }
    let (mut files, mut zips): (Vec<FileInfo>, Vec<PathBuf>) = (vec![], vec![]);
    for e in WalkDir::new(root).follow_links(false).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let md = e.metadata()?;
        if is_jpg(&name) && md.len() <= MAX_JPG {
            files.push(FileInfo { abs: e.path().to_path_buf(), rel: e.path().strip_prefix(root)?.to_path_buf(), size: md.len(), mtime: md.modified()? });
        } else if ext_lower(e.path()) == "zip" {
            zips.push(e.path().to_path_buf());
        }
    }
    let total: u64 = files.iter().map(|f| f.size).sum::<u64>() + zips.iter().filter_map(|z| fs::metadata(z).ok()).map(|m| m.len()).sum::<u64>();
    ctx.log(format!("checking {} JPG(s) and {} zip(s), {}", files.len(), zips.len(), fmt_bytes(total)));
    ctx.reset_progress((files.len() + zips.len()) as u64, total);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cfg.archive.threads).build()?;
    let (found, fixed, failed) = (AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0));

    pool.install(|| {
        files.par_iter().for_each(|f| {
            if ctx.cancelled() {
                return;
            }
            let r = (|| -> Result<()> {
                let data = fs::read(&f.abs)?;
                if ctx.dry {
                    if broken(&data) {
                        found.fetch_add(1, Relaxed);
                        ctx.log(format!("would repair {}", f.rel.display()));
                    }
                    return Ok(());
                }
                let Some(new) = fix(&data)? else { return Ok(()) };
                found.fetch_add(1, Relaxed);
                replace_file(f, &new)?;
                fixed.fetch_add(1, Relaxed);
                ctx.log(format!("repaired {}", f.rel.display()));
                Ok(())
            })();
            if let Err(e) = r {
                failed.fetch_add(1, Relaxed);
                ctx.log(format!("{}: {e:#}", f.rel.display()));
            }
            ctx.add_bytes(f.size);
            ctx.add_file();
        })
    });

    for z in &zips {
        if ctx.cancelled() {
            break;
        }
        let rel = z.strip_prefix(root).unwrap_or(z).display().to_string();
        let r = if ctx.dry { scan_zip(&pool, cfg.archive.threads * 2, z) } else { fix_zip(ctx, &pool, cfg.archive.threads * 2, z) };
        match r {
            Ok(0) => {}
            Ok(n) => {
                found.fetch_add(n, Relaxed);
                if ctx.dry {
                    ctx.log(format!("would repair {n} JPG(s) in {rel}"));
                } else {
                    fixed.fetch_add(n, Relaxed);
                    ctx.log(format!("repaired {n} JPG(s) in {rel}"));
                }
            }
            Err(e) => {
                failed.fetch_add(1, Relaxed);
                ctx.log(format!("{rel}: {e:#}"));
            }
        }
        ctx.add_file();
    }
    let (fo, fi, fa) = (found.load(Relaxed), fixed.load(Relaxed), failed.load(Relaxed));
    ctx.log(format!("repair done: {fo} broken JPG(s) found, {fi} repaired, {fa} problem(s){}", if ctx.dry { " (dry run)" } else { "" }));
    if fa > 0 {
        bail!("{fa} problem(s), see log");
    }
    Ok(())
}

fn replace_file(f: &FileInfo, new: &[u8]) -> Result<()> {
    let md = fs::metadata(&f.abs)?;
    if md.len() != f.size || md.modified()? != f.mtime {
        bail!("changed while repairing");
    }
    let n = f.abs.file_name().unwrap().to_string_lossy().into_owned();
    let tmp = f.abs.with_file_name(format!(".{n}.vrepair"));
    let r = (|| -> Result<()> {
        let mut t = File::create(&tmp)?;
        t.write_all(new)?;
        t.set_modified(f.mtime)?;
        t.sync_all()?;
        if hash_file(&tmp)? != blake3::hash(new) {
            bail!("write verification failed");
        }
        fs::rename(&tmp, &f.abs)?;
        Ok(())
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

fn fix_zip(ctx: &Ctx, pool: &rayon::ThreadPool, chunk: usize, z: &Path) -> Result<u64> {
    let f = |data: &[u8]| -> Option<(Vec<u8>, (u16, u16))> {
        match fix(data) {
            Ok(Some(new)) => vault_raw::probe_decode_bytes(&new).ok().map(|i| (new, (i.width as u16, i.height as u16))),
            Ok(None) => None,
            Err(e) => {
                ctx.log(format!("{}: an entry could not be repaired: {e:#}", z.display()));
                None
            }
        }
    };
    let r = rewrite_zip(ctx, pool, chunk, z, &f, &|_| true)?;
    Ok(r.changed as u64)
}

/// Dry run: count broken JPGs in a zip without writing anything.
fn scan_zip(pool: &rayon::ThreadPool, chunk: usize, z: &Path) -> Result<u64> {
    let mut za = zip::ZipArchive::new(BufReader::new(File::open(z)?))?;
    let mut n = 0u64;
    let mut batch: Vec<Vec<u8>> = Vec::new();
    let count = |b: &mut Vec<Vec<u8>>| -> u64 {
        let c = pool.install(|| b.par_iter().filter(|d| broken(d)).count()) as u64;
        b.clear();
        c
    };
    for i in 0..za.len() {
        let mut e = za.by_index(i)?;
        if !e.is_file() || !is_jpg(e.name()?.as_ref()) || e.size() > MAX_JPG {
            continue;
        }
        let mut d = Vec::with_capacity(e.size() as usize);
        e.read_to_end(&mut d)?;
        batch.push(d);
        if batch.len() >= chunk.max(1) {
            n += count(&mut batch);
        }
    }
    n += count(&mut batch);
    Ok(n)
}
