use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use crate::config::{ArchiveMode, Config, ExistingJpg};
use crate::job::Ctx;
use crate::util::*;

/// Part 3: oldest first, convert ARW to JPG (ARW deleted only after the JPG is read back and
/// checked), then zip folders that contain no ARW (stored; folder deleted only after the zip is
/// read back and every file matches).
pub fn run(cfg: &Config, ctx: &Ctx, limit: Option<usize>) -> Result<()> {
    let root = &cfg.paths.local_backup;
    if !root.is_dir() {
        bail!("local backup folder {} not found", root.display());
    }
    let mut years: Vec<(i32, PathBuf)> = fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            is_year(&n).then(|| (n.parse().unwrap(), e.path()))
        })
        .collect();
    years.sort();
    let mut projects: Vec<PathBuf> = Vec::new();
    for (_, y) in years {
        let mut ps: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(&y)?
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
            .collect();
        ps.sort();
        projects.extend(ps.into_iter().map(|(_, p)| p));
    }
    let total = projects.len();
    let mut projects: Vec<(PathBuf, ArchiveMode)> = projects.into_iter().map(|p| { let m = cfg.archive_mode(&p); (p, m) }).filter(|(_, m)| *m != ArchiveMode::None).collect();
    if let Some(n) = limit {
        projects.truncate(n);
    }
    ctx.log(format!("{} of {total} project folder(s) selected for archiving, oldest first", projects.len()));
    if projects.is_empty() {
        ctx.log("nothing selected: choose folders under \"Convert / zip selection\" or set archive.default");
    }
    ctx.reset_progress(projects.len() as u64, 0);
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cfg.archive.threads).build()?;
    let problems = AtomicU32::new(0);
    for (p, mode) in projects {
        if ctx.cancelled() {
            ctx.log("cancelled");
            break;
        }
        if let Err(e) = pool.install(|| project(cfg, ctx, &p, mode)) {
            problems.fetch_add(1, Relaxed);
            ctx.log(format!("{}: FAILED: {e:#}", p.display()));
        }
        ctx.add_file();
    }
    let n = problems.load(Relaxed);
    ctx.log(format!("archive done, {n} problem(s)"));
    if n > 0 {
        bail!("{n} problem(s), see log");
    }
    Ok(())
}

fn project(cfg: &Config, ctx: &Ctx, dir: &Path, mode: ArchiveMode) -> Result<()> {
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let files = list_files(dir)?;
    let arws: Vec<&FileInfo> = files.iter().filter(|f| ext_lower(&f.abs) == "arw").collect();
    if !arws.is_empty() {
        ctx.log(format!("{name}: {} ARW file(s) to convert", arws.len()));
        if ctx.dry {
            return finish_dry(ctx, &name, mode);
        }
        let errs: Vec<String> = arws.par_iter().filter_map(|f| convert_one(cfg, ctx, f).err().map(|e| format!("{}: {e:#}", f.rel.display()))).collect();
        if !errs.is_empty() {
            for e in errs.iter().take(10) {
                ctx.log(format!("{name}: {e}"));
            }
            bail!("{} conversion(s) failed; folder left as is", errs.len());
        }
    }
    if mode == ArchiveMode::Convert {
        return Ok(());
    }
    // Anything except RAW (.arw) files may be zipped: JPGs, XMP/PNG/video sidecars, subfolders.
    let files = list_files(dir)?;
    if files.is_empty() {
        ctx.log(format!("{name}: empty, not zipped"));
        return Ok(());
    }
    if let Some(a) = files.iter().find(|f| ext_lower(&f.abs) == "arw") {
        bail!("{} is still an ARW; not zipped", a.rel.display());
    }
    if let Some(sp) = special_entry(dir) {
        ctx.log(format!("{name}: contains {} (symlink or special file), not zipped", sp.display()));
        return Ok(());
    }
    if ctx.dry {
        ctx.log(format!("{name}: would zip {} file(s) and remove the folder", files.len()));
        return Ok(());
    }
    zip_folder(ctx, dir, &name, &files)
}

/// First entry that is neither a regular file nor a folder (zipping would silently drop it).
fn special_entry(dir: &Path) -> Option<PathBuf> {
    walkdir::WalkDir::new(dir).follow_links(false).into_iter().filter_map(|e| e.ok()).find(|e| !e.file_type().is_file() && !e.file_type().is_dir()).map(|e| e.into_path())
}

fn finish_dry(ctx: &Ctx, name: &str, mode: ArchiveMode) -> Result<()> {
    if mode == ArchiveMode::ConvertZip {
        ctx.log(format!("{name}: would then zip and remove the folder"));
    }
    Ok(())
}

fn existing_jpg(f: &FileInfo) -> Option<PathBuf> {
    let stem = f.abs.file_stem()?.to_string_lossy().to_lowercase();
    fs::read_dir(f.abs.parent()?).ok()?.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
        p.is_file() && matches!(ext_lower(p).as_str(), "jpg" | "jpeg") && p.file_stem().map(|s| s.to_string_lossy().to_lowercase()) == Some(stem.clone())
    })
}

fn convert_one(cfg: &Config, ctx: &Ctx, f: &FileInfo) -> Result<()> {
    let a = &cfg.archive;
    let mode = ctx.existing.lock().unwrap().unwrap_or(a.existing_jpg);
    let existing = existing_jpg(f);
    if let (ExistingJpg::UseExisting, Some(j)) = (mode, &existing) {
        let info = vault_raw::probe_decode(j).with_context(|| format!("{} does not decode; ARW kept", j.display()))?;
        if info.width == 0 || info.height == 0 {
            bail!("{} has no pixels; ARW kept", j.display());
        }
        fs::remove_file(&f.abs).with_context(|| format!("deleting {}", f.abs.display()))?;
        return Ok(());
    }
    let (_, bytes) = vault_raw::render_jpg(&f.abs, a.colour, a.demosaic, &a.render)?;
    let want = blake3::hash(&bytes);
    let stem = f.abs.file_stem().unwrap().to_string_lossy().into_owned();
    let parent = f.abs.parent().unwrap();
    // An existing JPG with identical bytes is our own earlier output (interrupted run); a different
    // one is the user's, so a second name is used rather than overwriting it.
    let mut out = None;
    if let (ExistingJpg::Overwrite, Some(j)) = (mode, &existing) {
        let n = j.file_name().unwrap().to_string_lossy().into_owned();
        let tmp = j.with_file_name(format!(".{n}.vtmp"));
        let r = (|| -> Result<()> {
            let mut t = File::create(&tmp)?;
            t.write_all(&bytes)?;
            t.sync_all()?;
            fs::rename(&tmp, j)?;
            Ok(())
        })();
        if r.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        r?;
        out = Some(j.clone());
    }
    for cand in [format!("{stem}.jpg"), format!("{stem}_arw.jpg")].into_iter().filter(|_| out.is_none()) {
        let p = parent.join(cand);
        if !p.exists() {
            vault_raw::write_atomic(&p, &bytes)?;
            out = Some(p);
            break;
        }
        if hash_file(&p)? == want {
            out = Some(p);
            break;
        }
    }
    let out = out.context("both target JPG names are taken by different files")?;
    if hash_file(&out)? != want {
        let _ = fs::remove_file(&out);
        bail!("JPG did not land intact");
    }
    let info = vault_raw::probe_decode(&out).context("JPG does not decode")?;
    if info.width == 0 || info.height == 0 {
        bail!("JPG has no pixels");
    }
    fs::remove_file(&f.abs).with_context(|| format!("deleting {}", f.abs.display()))?;
    Ok(())
}

fn zip_folder(ctx: &Ctx, dir: &Path, name: &str, files: &[FileInfo]) -> Result<()> {
    let parent = dir.parent().unwrap();
    let zpath = parent.join(format!("{name}.zip"));
    if zpath.exists() {
        bail!("{} already exists; folder not zipped", zpath.display());
    }
    let tmp = parent.join(format!(".{name}.zip.vtmp"));
    // Subfolders are stored as entries too, so empty ones survive.
    let dirs: Vec<PathBuf> = walkdir::WalkDir::new(dir).min_depth(1).sort_by_file_name().into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_dir()).filter_map(|e| e.path().strip_prefix(dir).ok().map(|p| p.to_path_buf())).collect();
    let r = (|| -> Result<()> {
        write_zip(&tmp, name, files, &dirs)?;
        verify_zip(&tmp, name, files, &dirs)?;
        File::open(&tmp)?.sync_all()?;
        fs::rename(&tmp, &zpath)?;
        Ok(())
    })();
    if let Err(e) = r {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    // Last look: nothing changed while zipping.
    for f in files {
        let m = fs::metadata(&f.abs)?;
        if m.len() != f.size || m.modified()? != f.mtime {
            let _ = fs::remove_file(&zpath);
            bail!("{} changed while zipping; zip discarded", f.rel.display());
        }
    }
    remove_files_and_empty_dirs(dir, files)?;
    ctx.log(format!("{name}: zipped {} file(s) -> {}", files.len(), zpath.display()));
    Ok(())
}

fn entry_name(name: &str, rel: &Path) -> String {
    let rel: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    format!("{name}/{}", rel.join("/"))
}

fn write_zip(tmp: &Path, name: &str, files: &[FileInfo], dirs: &[PathBuf]) -> Result<()> {
    use chrono::{Datelike, Timelike};
    let mut z = zip::ZipWriter::new(File::create(tmp)?);
    for d in dirs {
        z.add_directory(entry_name(name, d), zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored))?;
    }
    for f in files {
        let t: chrono::DateTime<chrono::Local> = f.mtime.into();
        let mut opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored).large_file(true);
        if let Ok(d) = zip::DateTime::from_date_and_time(t.year().clamp(1980, 2107) as u16, t.month() as u8, t.day() as u8, t.hour() as u8, t.minute() as u8, t.second() as u8) {
            opt = opt.last_modified_time(d);
        }
        z.start_file(entry_name(name, &f.rel), opt)?;
        let mut src = File::open(&f.abs)?;
        std::io::copy(&mut src, &mut z)?;
    }
    z.finish()?.flush()?;
    Ok(())
}

fn verify_zip(zp: &Path, name: &str, files: &[FileInfo], dirs: &[PathBuf]) -> Result<()> {
    let mut z = zip::ZipArchive::new(File::open(zp)?)?;
    if z.len() != files.len() + dirs.len() {
        bail!("zip has {} entries, expected {}", z.len(), files.len() + dirs.len());
    }
    for f in files {
        let mut e = z.by_name(&entry_name(name, &f.rel)).with_context(|| format!("{} missing from zip", f.rel.display()))?;
        if e.size() != f.size {
            bail!("{} has wrong size in zip", f.rel.display());
        }
        let mut h = blake3::Hasher::new();
        std::io::copy(&mut e, &mut h)?; // the zip reader also checks the CRC
        if h.finalize() != hash_file(&f.abs)? {
            bail!("{} differs in zip", f.rel.display());
        }
    }
    Ok(())
}
