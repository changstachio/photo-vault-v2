use anyhow::{bail, Result};
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::dates;
use crate::job::Ctx;
use crate::util::*;

enum Found {
    Dir(PathBuf),
    Zip,
}

/// Look for `<root>/<name>` or `<root>/<year>/<name>` (folder or `.zip`).
fn find(root: &Path, name: &str) -> Result<Vec<Found>> {
    let mut v = Vec::new();
    if !root.is_dir() {
        return Ok(v);
    }
    let mut parents = vec![root.to_path_buf()];
    for e in fs::read_dir(root)? {
        let e = e?;
        if e.file_type()?.is_dir() && is_year(&e.file_name().to_string_lossy()) {
            parents.push(e.path());
        }
    }
    parents.sort();
    for p in parents {
        let d = p.join(name);
        if d.is_dir() && !fs::symlink_metadata(&d)?.file_type().is_symlink() {
            v.push(Found::Dir(d));
        }
        let z = p.join(format!("{name}.zip"));
        if z.is_file() {
            v.push(Found::Zip);
        }
    }
    Ok(v)
}

/// Files of `src` that are not present with identical content under `dir`.
fn unmatched<'a>(dir: &Path, files: &'a [FileInfo], hashes: &[blake3::Hash]) -> Vec<usize> {
    (0..files.len())
        .into_par_iter()
        .filter(|&i| {
            let p = dir.join(&files[i].rel);
            match fs::metadata(&p) {
                Ok(m) if m.is_file() && m.len() == files[i].size => hash_file(&p).map(|h| h != hashes[i]).unwrap_or(true),
                _ => true,
            }
        })
        .collect()
}

/// Part 2: every project folder in the ingest cache must be confirmed (BLAKE3, byte for byte) in the
/// local backup before it is deleted from the cache. A missing project is copied to
/// `<local_backup>/<year of oldest plausible date>/<project>/` first.
pub fn run(cfg: &Config, ctx: &Ctx) -> Result<()> {
    let p = &cfg.paths;
    if !p.ingest.is_dir() {
        bail!("ingest folder {} not found", p.ingest.display());
    }
    if !p.local_backup.is_dir() {
        bail!("local backup folder {} not found (is the drive mounted?)", p.local_backup.display());
    }
    let pool = rayon::ThreadPoolBuilder::new().num_threads(cfg.ingest.threads).build()?;
    let idle = Duration::from_secs(cfg.ingest.min_idle_minutes * 60);
    let mut projects: Vec<_> = fs::read_dir(&p.ingest)?.filter_map(|e| e.ok()).filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).collect();
    projects.sort_by_key(|e| e.file_name());
    let (mut cleared, mut kept, mut problems) = (0u32, 0u32, 0u32);

    for e in projects {
        if ctx.cancelled() {
            ctx.log("cancelled");
            break;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !is_plain_name(&name) {
            continue;
        }
        match pool.install(|| project(cfg, ctx, &name, &e.path(), idle)) {
            Ok(Outcome::Cleared) => cleared += 1,
            Ok(Outcome::Kept) => kept += 1,
            Err(err) => {
                problems += 1;
                ctx.log(format!("{name}: FAILED: {err:#}"));
            }
        }
    }
    ctx.log(format!("ingest done: {cleared} cleared from cache, {kept} kept, {problems} problem(s)"));
    if problems > 0 {
        bail!("{problems} problem(s), see log");
    }
    Ok(())
}

enum Outcome {
    Cleared,
    Kept,
}

fn project(cfg: &Config, ctx: &Ctx, name: &str, path: &Path, idle: Duration) -> Result<Outcome> {
    let p = &cfg.paths;
    let files = list_files(path)?;
    if files.is_empty() {
        ctx.log(format!("{name}: empty, left alone"));
        return Ok(Outcome::Kept);
    }
    if let Some(newest) = files.iter().map(|f| f.mtime).max() {
        if SystemTime::now().duration_since(newest).unwrap_or_default() < idle {
            ctx.log(format!("{name}: modified in the last {} min, skipped", idle.as_secs() / 60));
            return Ok(Outcome::Kept);
        }
    }
    let total: u64 = files.iter().map(|f| f.size).sum();
    ctx.reset_progress(files.len() as u64 * 2, total * 2);
    let hashes: Vec<blake3::Hash> = files.par_iter().map(|f| hash_file(&f.abs)).collect::<Result<_>>()?;
    ctx.add_bytes(total);

    let local = find(&p.local_backup, name)?;
    if local.iter().any(|f| matches!(f, Found::Zip)) && !local.iter().any(|f| matches!(f, Found::Dir(_))) {
        ctx.log(format!("{name}: already archived as a zip in local backup; not copied or deleted (resolve by hand)"));
        return Ok(Outcome::Kept);
    }
    let net = find(&p.network_backup, name)?;
    let net_ok = net.iter().any(|f| matches!(f, Found::Dir(d) if unmatched(d, &files, &hashes).is_empty()));

    // Best existing local copy = the one missing the fewest files.
    let best = local
        .iter()
        .filter_map(|f| match f {
            Found::Dir(d) => Some((unmatched(d, &files, &hashes), d.clone())),
            _ => None,
        })
        .min_by_key(|(m, _)| m.len());
    let (missing, dest_dir) = match best {
        Some((m, d)) => (m, d),
        None => {
            let Some(ts) = dates::dir_date(path)? else {
                ctx.log(format!("{name}: no usable date (all before 2010 or missing); skipped"));
                return Ok(Outcome::Kept);
            };
            (( 0..files.len()).collect(), p.local_backup.join(dates::year_of(ts).to_string()).join(name))
        }
    };
    ctx.log(format!(
        "{name}: local {} ({} of {} file(s) missing), network {}",
        if missing.len() == files.len() { "absent" } else { "present" },
        missing.len(),
        files.len(),
        if net_ok { "present" } else { "absent/different" }
    ));
    if ctx.dry {
        if !missing.is_empty() {
            ctx.log(format!("{name}: would copy {} file(s) to {}", missing.len(), dest_dir.display()));
        }
        ctx.log(format!("{name}: would delete from cache once verified"));
        return Ok(Outcome::Kept);
    }

    let errs: Vec<String> = missing
        .par_iter()
        .filter_map(|&i| {
            if ctx.cancelled() {
                return Some("cancelled".into());
            }
            let f = &files[i];
            let r = (|| -> Result<()> {
                let dst = safe_join(&dest_dir, &f.rel)?;
                if dst.exists() {
                    bail!("{} exists with different content; refusing to overwrite", dst.display());
                }
                let (h, _) = copy_hashed(&f.abs, &dst, 8 << 20, ctx)?;
                if h != hashes[i] || hash_file(&dst)? != hashes[i] {
                    let _ = fs::remove_file(&dst);
                    bail!("copy of {} failed verification", f.rel.display());
                }
                ctx.add_bytes(f.size);
                Ok(())
            })();
            r.err().map(|e| format!("{e:#}"))
        })
        .collect();
    if !errs.is_empty() {
        for e in errs.iter().take(10) {
            ctx.log(format!("{name}: {e}"));
        }
        bail!("{} file(s) not backed up; cache left intact", errs.len());
    }

    // Final proof from disk, not from the copy step, before anything is deleted.
    let still = unmatched(&dest_dir, &files, &hashes);
    if !still.is_empty() {
        bail!("{} file(s) not confirmed in local backup; cache left intact", still.len());
    }
    // Source files must be unchanged since hashing.
    for (f, h) in files.iter().zip(&hashes) {
        let m = fs::metadata(&f.abs)?;
        if m.len() != f.size || m.modified()? != f.mtime {
            bail!("{} changed during ingest; cache left intact", f.rel.display());
        }
        let _ = h;
    }
    remove_files_and_empty_dirs(path, &files)?;
    ctx.log(format!("{name}: confirmed in {}, removed from cache", dest_dir.display()));
    Ok(Outcome::Cleared)
}
