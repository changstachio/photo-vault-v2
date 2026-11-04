use anyhow::{bail, Context, Result};
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::config::Config;
use crate::job::Ctx;
use crate::util::*;

struct Item {
    f: FileInfo,
    dst: PathBuf,
}

/// Part 1: mirror the chosen source folders onto the network share. Nothing is ever deleted;
/// a changed source file replaces its old copy atomically (temp file + rename).
pub fn run(cfg: &Config, ctx: &Ctx) -> Result<()> {
    let s = &cfg.sync;
    let dest = s.dest.clone().context("sync.dest is not set")?;
    if s.sources.is_empty() {
        bail!("no [[sync.source]] entries configured");
    }
    if let Some(m) = &s.require_mount {
        if !is_mount(m)? {
            bail!("{} is not a mounted filesystem; refusing to copy", m.display());
        }
    }
    if !dest.is_dir() {
        bail!("sync destination {} does not exist", dest.display());
    }

    let mut items = Vec::new();
    let mut seen = 0u64;
    let mut labels: std::collections::HashMap<String, PathBuf> = std::collections::HashMap::new();
    let mut errors = 0u64;
    // A source inside another selected folder is already covered by it.
    let folders: Vec<PathBuf> = s.sources.iter().filter(|x| x.path.is_dir()).map(|x| x.path.canonicalize().unwrap_or_else(|_| x.path.clone())).collect();
    for src in &s.sources {
        let canon = src.path.canonicalize().unwrap_or_else(|_| src.path.clone());
        if folders.iter().any(|f| *f != canon && canon.starts_with(f)) {
            continue;
        }
        if crate::config::overlaps(&src.path, &dest) {
            ctx.log(format!("skip {}: overlaps the destination {}", src.path.display(), dest.display()));
            errors += 1;
            continue;
        }
        let is_file = src.path.is_file();
        if !src.path.is_dir() && !is_file {
            ctx.log(format!("skip {}: not found", src.path.display()));
            errors += 1;
            continue;
        }
        // A single selected file goes under its folder's name; a folder under its own name.
        let parent_dir = if is_file { src.path.parent().map(|p| p.to_path_buf()) } else { Some(src.path.clone()) };
        let default_label = parent_dir.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        let mut label = src.label.clone().or(default_label).unwrap_or_default();
        if !is_plain_name(&label) {
            ctx.log(format!("skip {}: cannot derive a folder name, set label", src.path.display()));
            errors += 1;
            continue;
        }
        // Single files from the same folder share its label. Two different folders with the same
        // name get the parent folder's name prefixed, e.g. "b - Photos", instead of mixing.
        let owner = parent_dir.clone().unwrap_or_default();
        if let Some(prev) = labels.get(&label) {
            if *prev != owner {
                let pre = owner.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let alt = format!("{pre} - {label}");
                if src.label.is_some() || !is_plain_name(&alt) || labels.get(&alt).is_some_and(|p| *p != owner) {
                    ctx.log(format!("skip {}: folder name {label} is already used by {}; set a label", src.path.display(), prev.display()));
                    errors += 1;
                    continue;
                }
                ctx.log(format!("{}: {label} is taken, copying to {alt}", src.path.display()));
                label = alt;
            }
        }
        labels.insert(label.clone(), owner);
        let files = if is_file {
            match single_file(&src.path) {
                Ok(f) => f,
                Err(e) => {
                    ctx.log(format!("scan failed {}: {e:#}", src.path.display()));
                    errors += 1;
                    continue;
                }
            }
        } else {
            let (f, errs) = list_files_lossy(&src.path);
            for e in errs.iter().take(20) {
                ctx.log(format!("FAILED {e}"));
            }
            errors += errs.len() as u64;
            f
        };
        let root = dest.join(&label);
        for f in files {
            let dst = safe_join(&root, &f.rel)?;
            items.push(Item { f, dst });
        }
        seen += 1;
    }
    ctx.log(format!("scanned {seen} source folder(s), {} files", items.len()));

    let scan_pool = rayon::ThreadPoolBuilder::new().num_threads(16).build()?;
    let todo: Vec<Item> = scan_pool.install(|| items.into_par_iter().filter(|i| !same_stamp(&i.f, &i.dst)).collect());
    let bytes: u64 = todo.iter().map(|i| i.f.size).sum();
    ctx.log(format!("{} file(s) to copy, {}", todo.len(), fmt_bytes(bytes)));
    ctx.reset_progress(todo.len() as u64, bytes);
    if ctx.dry {
        for i in todo.iter().take(200) {
            ctx.log(format!("would copy {}", i.f.rel.display()));
        }
        return Ok(());
    }

    let buf = s.buffer_mib << 20;
    let failures = Mutex::new(Vec::<String>::new());
    let pool = rayon::ThreadPoolBuilder::new().num_threads(s.threads).build()?;
    pool.install(|| {
        todo.par_iter().for_each(|i| {
            if ctx.cancelled() {
                return;
            }
            let r = (|| -> Result<()> {
                // A changed file replaces the old copy atomically: copy to a sibling name first.
                if i.dst.exists() {
                    let old = i.dst.with_file_name(format!(".{}.vold", i.dst.file_name().unwrap().to_string_lossy()));
                    let _ = std::fs::remove_file(&old);
                    std::fs::rename(&i.dst, &old)?;
                    let res = copy_hashed(&i.f.abs, &i.dst, buf, ctx).and_then(|(h, _)| {
                        if s.verify && hash_file(&i.dst)? != h {
                            let _ = std::fs::remove_file(&i.dst);
                            bail!("verify failed for {}", i.dst.display());
                        }
                        Ok(())
                    });
                    match res {
                        Ok(_) => {
                            let _ = std::fs::remove_file(&old);
                        }
                        Err(e) => {
                            let _ = std::fs::rename(&old, &i.dst);
                            return Err(e);
                        }
                    }
                    return Ok(());
                }
                let (h, _) = copy_hashed(&i.f.abs, &i.dst, buf, ctx)?;
                if s.verify {
                    let d = hash_file(&i.dst)?;
                    if d != h {
                        let _ = std::fs::remove_file(&i.dst);
                        bail!("verify failed for {}", i.dst.display());
                    }
                }
                Ok(())
            })();
            match r {
                Ok(()) => ctx.add_file(),
                Err(e) => {
                    let m = format!("FAILED {}: {e:#}", i.f.rel.display());
                    ctx.log(&m);
                    failures.lock().unwrap().push(m);
                }
            }
        });
    });
    let failed = failures.lock().unwrap().len() as u64 + errors;
    let secs = ctx.started.lock().unwrap().elapsed().as_secs_f64().max(0.001);
    let done = ctx.progress.bytes_done.load(std::sync::atomic::Ordering::Relaxed);
    ctx.log(format!("copied {} in {:.0}s ({:.0} MB/s), {failed} problem(s){}", fmt_bytes(done), secs, done as f64 / secs / 1e6, if ctx.cancelled() { ", cancelled" } else { "" }));
    if failed > 0 {
        bail!("{failed} problem(s), see log");
    }
    Ok(())
}
