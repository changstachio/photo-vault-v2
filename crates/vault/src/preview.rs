use anyhow::{bail, Context, Result};
use std::collections::hash_map::RandomState;
use std::fs::{self, File};
use std::hash::{BuildHasher, Hasher};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use vault_raw::Chroma;
use walkdir::WalkDir;

use crate::config::Config;
use crate::util::*;

const QUALITIES: [u8; 8] = [100, 95, 90, 85, 80, 75, 70, 60];
const CHROMAS: [Chroma; 3] = [Chroma::C444, Chroma::C422, Chroma::C420];
const MAX_JPG: u64 = 64 << 20;

fn rand_u64() -> u64 {
    RandomState::new().build_hasher().finish()
}

/// Uniform random pick from a stream (reservoir sampling).
struct Pick<T> {
    seen: u64,
    item: Option<T>,
}

impl<T> Pick<T> {
    fn new() -> Self {
        Pick { seen: 0, item: None }
    }
    fn offer(&mut self, x: T) {
        self.seen += 1;
        if rand_u64() % self.seen == 0 {
            self.item = Some(x);
        }
    }
}

enum Source {
    Arw(PathBuf),
    JpgFile(PathBuf),
    Zip(PathBuf),
}

/// Size test for the web UI: pick a random file under `path` (inside the local backup) and show how
/// big it becomes at each quality / chroma setting. A random ARW is used if there is one, else a
/// random JPG (a loose file or one inside a zip). The same LibRaw render is reused for all settings.
pub fn run(cfg: &Config, path: &Path) -> Result<serde_json::Value> {
    let root = cfg.paths.local_backup.canonicalize().context("local backup folder is not accessible")?;
    let p = path.canonicalize().with_context(|| format!("{} does not exist", path.display()))?;
    if !p.starts_with(&root) {
        bail!("{} is outside the local backup folder", path.display());
    }
    let (mut arw, mut jpg): (Pick<PathBuf>, Pick<Source>) = (Pick::new(), Pick::new());
    if p.is_file() {
        match ext_lower(&p).as_str() {
            "arw" => arw.offer(p.clone()),
            "zip" => jpg.offer(Source::Zip(p.clone())),
            "jpg" | "jpeg" => jpg.offer(Source::JpgFile(p.clone())),
            _ => {}
        }
    } else {
        for e in WalkDir::new(&p).follow_links(false).into_iter().filter_map(|e| e.ok()).filter(|e| e.file_type().is_file()) {
            match ext_lower(e.path()).as_str() {
                "arw" => arw.offer(e.path().to_path_buf()),
                "jpg" | "jpeg" => jpg.offer(Source::JpgFile(e.path().to_path_buf())),
                "zip" => jpg.offer(Source::Zip(e.path().to_path_buf())),
                _ => {}
            }
        }
    }
    let src = match (arw.item, jpg.item) {
        (Some(a), _) => Source::Arw(a),
        (None, Some(j)) => j,
        _ => bail!("no ARW or JPG files found under {}", path.display()),
    };
    let variants: Vec<(u8, Chroma)> = CHROMAS.iter().flat_map(|c| QUALITIES.iter().map(move |q| (*q, *c))).collect();
    let rel = |f: &Path| f.strip_prefix(&root).unwrap_or(f).to_string_lossy().into_owned();
    let (kind, name, source_size, (w, h, sizes)) = match src {
        Source::Arw(a) => {
            let size = fs::metadata(&a)?.len();
            let t = vault_raw::size_table(&a, cfg.archive.colour, cfg.archive.demosaic, &cfg.archive.render, &variants)?;
            ("arw", rel(&a), size, t)
        }
        Source::JpgFile(j) => {
            let data = fs::read(&j)?;
            let size = data.len() as u64;
            ("jpg", rel(&j), size, vault_raw::size_table_jpg(&data, &variants)?)
        }
        Source::Zip(z) => {
            let mut za = zip::ZipArchive::new(BufReader::new(File::open(&z)?))?;
            let mut pick = Pick::new();
            for i in 0..za.len() {
                let e = za.by_index_raw(i)?;
                if e.is_file() && e.name().map(|n| matches!(n.rsplit('.').next().map(|x| x.to_lowercase()).as_deref(), Some("jpg" | "jpeg"))).unwrap_or(false) && e.size() <= MAX_JPG {
                    pick.offer(i);
                }
            }
            let i = pick.item.with_context(|| format!("no JPGs inside {}", z.display()))?;
            let mut e = za.by_index(i)?;
            let ename = e.name()?.to_string();
            let mut data = Vec::with_capacity(e.size() as usize);
            e.read_to_end(&mut data)?;
            let size = data.len() as u64;
            ("jpg", format!("{} > {ename}", rel(&z)), size, vault_raw::size_table_jpg(&data, &variants)?)
        }
    };
    let rows: Vec<_> = variants.iter().zip(&sizes).map(|((q, c), s)| serde_json::json!({"quality": q, "chroma": c.as_str(), "size": s, "pct": (*s as f64) * 100.0 / source_size.max(1) as f64})).collect();
    let r = &cfg.archive.render;
    Ok(serde_json::json!({
        "kind": kind, "file": name, "source_size": source_size, "width": w, "height": h,
        "current": {"quality": r.quality, "chroma": r.chroma.as_str()}, "rows": rows,
        "note": if kind == "jpg" { "Re-encoding an existing JPG: sizes are for task 5 (shrink)." } else { "Sizes are for converting this ARW (task 3)." },
    }))
}
