use anyhow::{bail, Context, Result};
use chrono::{Datelike, Local, NaiveDateTime, TimeZone};
use rayon::prelude::*;
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Config;
use crate::job::Ctx;
use crate::util::*;

const IMAGE_EXTS: [&str; 8] = ["arw", "jpg", "jpeg", "png", "tif", "tiff", "dng", "heic"];

fn min_ts() -> i64 {
    Local.with_ymd_and_hms(2010, 1, 1, 0, 0, 0).unwrap().timestamp()
}

/// Dates before 2010 (camera clocks never set, epoch defaults) and in the future are ignored.
pub fn plausible(ts: i64) -> bool {
    ts >= min_ts() && ts <= chrono::Local::now().timestamp() + 86400
}

pub fn year_of(ts: i64) -> i32 {
    Local.timestamp_opt(ts, 0).single().map(|d| d.year()).unwrap_or(0)
}

fn secs(t: SystemTime) -> Option<i64> {
    t.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
}

fn naive_ts(n: NaiveDateTime) -> Option<i64> {
    Local.from_local_datetime(&n).earliest().map(|d| d.timestamp())
}

/// EXIF capture time. Only the first 4 MiB are read (cheap over a network share); files whose
/// EXIF data lies further in fall back to file times.
fn exif_ts(p: &Path) -> Option<i64> {
    let f = File::open(p).ok()?;
    let mut r = BufReader::new(f.take(4 << 20));
    let ex = exif::Reader::new().read_from_container(&mut r).ok()?;
    for tag in [exif::Tag::DateTimeOriginal, exif::Tag::DateTimeDigitized, exif::Tag::DateTime] {
        if let Some(f) = ex.get_field(tag, exif::In::PRIMARY) {
            let s = f.display_value().to_string();
            if let Ok(n) = NaiveDateTime::parse_from_str(s.trim(), "%Y-%m-%d %H:%M:%S") {
                if let Some(t) = naive_ts(n).filter(|t| plausible(*t)) {
                    return Some(t);
                }
            }
        }
    }
    None
}

/// Oldest plausible date among EXIF capture time, creation time and modified time.
pub fn file_date(p: &Path, md: &fs::Metadata) -> Option<i64> {
    let mut c: Vec<i64> = Vec::new();
    if IMAGE_EXTS.contains(&ext_lower(p).as_str()) {
        c.extend(exif_ts(p));
    }
    c.extend(md.created().ok().and_then(secs));
    c.extend(md.modified().ok().and_then(secs));
    c.into_iter().filter(|t| plausible(*t)).min()
}

pub fn dir_date(dir: &Path) -> Result<Option<i64>> {
    let files = list_files(dir)?;
    Ok(files
        .par_iter()
        .filter_map(|f| fs::metadata(&f.abs).ok().and_then(|m| file_date(&f.abs, &m)))
        .min())
}

fn zip_date(p: &Path) -> Option<i64> {
    let md = fs::metadata(p).ok()?;
    let mut c: Vec<i64> = vec![];
    c.extend(md.created().ok().and_then(secs));
    c.extend(md.modified().ok().and_then(secs));
    if let Ok(mut z) = zip::ZipArchive::new(File::open(p).ok()?) {
        for i in 0..z.len() {
            if let Ok(e) = z.by_index_raw(i) {
                if let Some(d) = e.last_modified() {
                    let n = chrono::NaiveDate::from_ymd_opt(d.year() as i32, d.month() as u32, d.day() as u32)
                        .and_then(|x| x.and_hms_opt(d.hour() as u32, d.minute() as u32, d.second() as u32));
                    c.extend(n.and_then(naive_ts));
                }
            }
        }
    }
    c.into_iter().filter(|t| plausible(*t)).min()
}

#[derive(Debug, Clone)]
pub struct Unit {
    pub path: PathBuf,
    pub name: String,
    pub in_year: Option<i32>,
    pub is_zip: bool,
}

fn is_zip_name(n: &str) -> bool {
    n.to_lowercase().ends_with(".zip")
}

/// Projects in a backup root: `<root>/<year>/<project>[.zip]` and unsorted `<root>/<project>[.zip]`.
/// Projects in a backup root (see above). An unreadable year folder is reported in the error list instead of
/// stopping the whole scan.
pub fn units_lossy(root: &Path) -> Result<(Vec<Unit>, Vec<String>)> {
    let mut v = Vec::new();
    let mut errs = Vec::new();
    for e in fs::read_dir(root).with_context(|| format!("reading {}", root.display()))? {
        let Ok(e) = e else { continue };
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if name.starts_with('.') || ft.is_symlink() {
            continue;
        }
        if ft.is_dir() && is_year(&name) {
            let rd = match fs::read_dir(e.path()) {
                Ok(rd) => rd,
                Err(err) => {
                    errs.push(format!("cannot read {}: {err}", e.path().display()));
                    continue;
                }
            };
            for c in rd.filter_map(|c| c.ok()) {
                let cn = c.file_name().to_string_lossy().into_owned();
                let Ok(cft) = c.file_type() else { continue };
                if cn.starts_with('.') || cft.is_symlink() {
                    continue;
                }
                if cft.is_dir() || (cft.is_file() && is_zip_name(&cn)) {
                    v.push(Unit { path: c.path(), name: cn, in_year: name.parse().ok(), is_zip: cft.is_file() });
                }
            }
        } else if ft.is_dir() || (ft.is_file() && is_zip_name(&name)) {
            v.push(Unit { path: e.path(), name, in_year: None, is_zip: ft.is_file() });
        }
    }
    v.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((v, errs))
}

pub fn unit_date(u: &Unit) -> Result<Option<i64>> {
    if u.is_zip {
        Ok(zip_date(&u.path))
    } else {
        dir_date(&u.path)
    }
}

/// Part 4: work out the real capture year of every project in both backup roots and report (or,
/// with `apply`, move) projects that are not in the right `<year>` folder.
pub fn run(cfg: &Config, ctx: &Ctx) -> Result<()> {
    let mut wrong = 0u64;
    let mut problems = 0u64;
    let mut unknown = 0u64;
    for (label, root) in [("network", &cfg.paths.network_backup), ("local", &cfg.paths.local_backup)] {
        if !root.is_dir() {
            ctx.log(format!("{label}: {} not found, skipped", root.display()));
            continue;
        }
        let (us, errs) = units_lossy(root)?;
        for e in &errs {
            problems += 1;
            ctx.log(format!("{label}: {e}"));
        }
        ctx.log(format!("{label}: {} project(s) in {}", us.len(), root.display()));
        ctx.reset_progress(us.len() as u64, 0);
        for u in us {
            if ctx.cancelled() {
                ctx.log("cancelled");
                return Ok(());
            }
            let d = match unit_date(&u) {
                Ok(d) => d,
                Err(e) => {
                    problems += 1;
                    ctx.log(format!("{label}: {} could not be read: {e:#}", u.name));
                    continue;
                }
            };
            ctx.add_file();
            let Some(ts) = d else {
                unknown += 1;
                ctx.log(format!("{label}: {} has no usable date (all before 2010 or missing)", u.name));
                continue;
            };
            let y = year_of(ts);
            if u.in_year == Some(y) {
                continue;
            }
            wrong += 1;
            let target = root.join(y.to_string()).join(&u.name);
            let from = match u.in_year {
                Some(f) => f.to_string(),
                None => "(no year)".into(),
            };
            if !ctx.apply || ctx.dry {
                ctx.log(format!("{label}: {} {from} -> {y}", u.name));
                continue;
            }
            if target.exists() {
                ctx.log(format!("{label}: {} -> {y} skipped, {} exists", u.name, target.display()));
                continue;
            }
            fs::create_dir_all(root.join(y.to_string()))?;
            match fs::rename(&u.path, &target) {
                Ok(()) => ctx.log(format!("{label}: moved {} {from} -> {y}", u.name)),
                Err(e) => ctx.log(format!("{label}: move of {} failed: {e}", u.name)),
            }
        }
    }
    ctx.log(format!("{wrong} project(s) in the wrong year folder, {unknown} without a usable date{}", if ctx.apply && !ctx.dry { "" } else { " (report only; run with apply to move)" }));
    if problems > 0 {
        bail!("{problems} folder(s) could not be read, see log");
    }
    Ok(())
}
