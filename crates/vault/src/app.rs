use anyhow::{bail, Result};
use chrono::{Datelike, Local, Timelike};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

use crate::config::{parse_day, parse_hhmm, Config};
use crate::job::Ctx;

pub struct Running {
    pub task: String,
    pub ctx: Arc<Ctx>,
    pub done: bool,
    pub result: Option<String>,
}

pub struct App {
    cfg: RwLock<Arc<Config>>,
    pub cfg_path: PathBuf,
    pub job: Mutex<Option<Running>>,
    pub events: Arc<Ctx>,
}

pub fn run_task(cfg: &Config, task: &str, ctx: &Ctx, limit: Option<usize>) -> Result<()> {
    match task {
        "sync" => crate::sync::run(cfg, ctx),
        "ingest" => crate::ingest::run(cfg, ctx),
        "archive" => crate::archive::run(cfg, ctx, limit),
        "dates" => crate::dates::run(cfg, ctx),
        t => bail!("unknown task {t}"),
    }
}

impl App {
    pub fn new(cfg: Config, cfg_path: PathBuf) -> Arc<App> {
        Arc::new(App { cfg: RwLock::new(Arc::new(cfg)), cfg_path, job: Mutex::new(None), events: Ctx::new(false, false, true) })
    }

    pub fn cfg(&self) -> Arc<Config> {
        self.cfg.read().unwrap().clone()
    }

    pub fn config_text(&self) -> Result<String> {
        Ok(std::fs::read_to_string(&self.cfg_path)?)
    }

    /// Validate `text` as a config, write it to the config file (temp file + rename, so a failed
    /// save never leaves a half-written file) and use it for the next task. Running tasks keep the
    /// settings they started with. Returns a note when a restart is needed.
    pub fn save_config(&self, text: &str) -> Result<Option<String>> {
        let new: Config = toml::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))?;
        new.validate()?;
        let old = self.cfg();
        let n = self.cfg_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let tmp = self.cfg_path.with_file_name(format!(".{n}.vtmp"));
        let r = (|| -> Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(text.as_bytes())?;
            f.sync_all()?;
            std::fs::rename(&tmp, &self.cfg_path)?;
            Ok(())
        })();
        if let Err(e) = r {
            let _ = std::fs::remove_file(&tmp);
            bail!("cannot write {}: {e:#} (the service user needs write access to the file and its folder)", self.cfg_path.display());
        }
        let note = (old.web.bind != new.web.bind || old.web.enabled != new.web.enabled).then(|| "web.bind / web.enabled take effect after restarting the service".to_string());
        *self.cfg.write().unwrap() = Arc::new(new);
        self.events.log("config saved from web UI");
        Ok(note)
    }

    /// Replace the `[[sync.source]]` list in the config file, keeping the rest of the file (comments
    /// included) as it is. Every path must exist inside the allowed browse folders.
    pub fn set_sources(&self, list: Vec<crate::config::Source>) -> Result<Option<String>> {
        use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};
        let cfg = self.cfg();
        let mut arr = ArrayOfTables::new();
        let mut seen = std::collections::HashSet::new();
        for s in &list {
            let p = cfg.pick_path(&s.path)?;
            if !seen.insert(p.clone()) {
                continue;
            }
            let mut t = Table::new();
            t["path"] = value(p.to_string_lossy().into_owned());
            if let Some(l) = &s.label {
                t["label"] = value(l.clone());
            }
            arr.push(t);
        }
        let mut doc: DocumentMut = self.config_text()?.parse().map_err(|e| anyhow::anyhow!("config file does not parse: {e}"))?;
        if !doc.contains_key("sync") {
            doc["sync"] = Item::Table(Table::new());
        }
        let sync = doc["sync"].as_table_mut().ok_or_else(|| anyhow::anyhow!("[sync] is not a table"))?;
        sync.remove("source");
        if !arr.is_empty() {
            sync.insert("source", Item::ArrayOfTables(arr));
        }
        self.save_config(&doc.to_string())
    }

    /// Replace the `[[archive.rule]]` list (folders must be inside the local backup folder).
    pub fn set_archive_rules(&self, list: Vec<crate::config::Rule>) -> Result<Option<String>> {
        use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};
        let cfg = self.cfg();
        let root = cfg.paths.local_backup.canonicalize().map_err(|e| anyhow::anyhow!("local backup folder not accessible: {e}"))?;
        let mut arr = ArrayOfTables::new();
        let mut seen = std::collections::HashSet::new();
        for r in &list {
            let p = r.path.canonicalize().map_err(|e| anyhow::anyhow!("{}: {e}", r.path.display()))?;
            if !p.starts_with(&root) || !p.is_dir() {
                bail!("{} is not a folder inside the local backup folder", r.path.display());
            }
            if !seen.insert(p.clone()) {
                continue;
            }
            let mut t = Table::new();
            t["path"] = value(p.to_string_lossy().into_owned());
            t["mode"] = value(r.mode.as_str());
            arr.push(t);
        }
        let mut doc: DocumentMut = self.config_text()?.parse().map_err(|e| anyhow::anyhow!("config file does not parse: {e}"))?;
        if !doc.contains_key("archive") {
            doc["archive"] = Item::Table(Table::new());
        }
        let t = doc["archive"].as_table_mut().ok_or_else(|| anyhow::anyhow!("[archive] is not a table"))?;
        t.remove("rule");
        if !arr.is_empty() {
            t.insert("rule", Item::ArrayOfTables(arr));
        }
        self.save_config(&doc.to_string())
    }

    /// Set JPEG quality and chroma subsampling in `[archive.render]`, keeping the rest of the file.
    pub fn set_render(&self, quality: u8, chroma: vault_raw::Chroma) -> Result<Option<String>> {
        use toml_edit::{value, DocumentMut, Item, Table};
        let mut doc: DocumentMut = self.config_text()?.parse().map_err(|e| anyhow::anyhow!("config file does not parse: {e}"))?;
        if !doc.contains_key("archive") {
            doc["archive"] = Item::Table(Table::new());
        }
        let ar = doc["archive"].as_table_mut().ok_or_else(|| anyhow::anyhow!("[archive] is not a table"))?;
        if !ar.contains_key("render") {
            ar["render"] = Item::Table(Table::new());
        }
        let r = ar["render"].as_table_mut().ok_or_else(|| anyhow::anyhow!("[archive.render] is not a table"))?;
        r["quality"] = value(quality as i64);
        r["chroma"] = value(chroma.as_str());
        self.save_config(&doc.to_string())
    }

    /// One level of a folder for the picker. Empty `path` lists the allowed roots. With `archive`,
    /// only folders inside the local backup folder are shown (starting at its top).
    pub fn browse(&self, path: &str, archive: bool) -> Result<serde_json::Value> {
        let cfg = self.cfg();
        let roots = if archive { cfg.paths.local_backup.canonicalize().into_iter().collect() } else { cfg.browse_roots() };
        let path = if archive && path.is_empty() { roots.first().map(|r: &std::path::PathBuf| r.to_string_lossy().into_owned()).ok_or_else(|| anyhow::anyhow!("local backup folder {} is not accessible to the service user", cfg.paths.local_backup.display()))? } else { path.to_string() };
        let path = path.as_str();
        if path.is_empty() {
            let skipped: Vec<_> = cfg.browse_root_status().into_iter().filter_map(|(p, why)| why.map(|w| serde_json::json!({"path": p.to_string_lossy(), "reason": w}))).collect();
            return Ok(serde_json::json!({"path": "", "parent": null, "skipped": skipped, "entries": roots.iter().map(|r| serde_json::json!({"name": r.to_string_lossy(), "path": r.to_string_lossy(), "dir": true, "size": 0})).collect::<Vec<_>>()}));
        }
        let dir = if archive {
            let c = std::path::Path::new(path).canonicalize()?;
            if !roots.iter().any(|r| c.starts_with(r)) {
                bail!("{path} is outside the local backup folder");
            }
            c
        } else {
            cfg.pick_path(std::path::Path::new(path))?
        };
        let mut entries = Vec::new();
        for e in std::fs::read_dir(&dir)? {
            let e = e?;
            let ft = e.file_type()?;
            let name = e.file_name().to_string_lossy().into_owned();
            if ft.is_symlink() || name.starts_with('.') || !(ft.is_dir() || (ft.is_file() && !archive)) {
                continue;
            }
            let size = if ft.is_file() { e.metadata().map(|m| m.len()).unwrap_or(0) } else { 0 };
            entries.push(serde_json::json!({"name": name, "path": e.path().to_string_lossy(), "dir": ft.is_dir(), "size": size}));
        }
        entries.sort_by(|a, b| (!a["dir"].as_bool().unwrap(), a["name"].as_str().unwrap().to_lowercase()).cmp(&(!b["dir"].as_bool().unwrap(), b["name"].as_str().unwrap().to_lowercase())));
        entries.truncate(3000);
        let parent = dir.parent().filter(|p| roots.iter().any(|r| p.starts_with(r))).map(|p| p.to_string_lossy().into_owned());
        Ok(serde_json::json!({"path": dir.to_string_lossy(), "parent": parent, "entries": entries}))
    }

    /// Start a task in the background; one task at a time.
    pub fn start(self: &Arc<Self>, task: &str, dry: bool, apply: bool, existing: Option<crate::config::ExistingJpg>) -> Result<()> {
        let mut j = self.job.lock().unwrap();
        if j.as_ref().map(|r| !r.done).unwrap_or(false) {
            bail!("a task is already running");
        }
        if !crate::config::TASKS.contains(&task) {
            bail!("unknown task {task}");
        }
        let ctx = Ctx::new(dry, apply, true);
        *ctx.existing.lock().unwrap() = existing;
        *j = Some(Running { task: task.into(), ctx: ctx.clone(), done: false, result: None });
        drop(j);
        let app = self.clone();
        let task = task.to_string();
        std::thread::spawn(move || {
            ctx.log(format!("start {task}{}", if dry { " (dry run)" } else { "" }));
            let r = run_task(&app.cfg(), &task, &ctx, None);
            let msg = match r {
                Ok(()) => "ok".to_string(),
                Err(e) => format!("failed: {e:#}"),
            };
            ctx.log(format!("end {task}: {msg}"));
            if let Some(j) = app.job.lock().unwrap().as_mut() {
                j.done = true;
                j.result = Some(msg);
            }
        });
        Ok(())
    }

    pub fn cancel(&self) {
        if let Some(j) = self.job.lock().unwrap().as_ref() {
            j.ctx.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub fn state(&self) -> serde_json::Value {
        use std::sync::atomic::Ordering::Relaxed;
        let j = self.job.lock().unwrap();
        let (task, running, result, log, prog) = match j.as_ref() {
            Some(r) => {
                let p = &r.ctx.progress;
                let el = r.ctx.started.lock().unwrap().elapsed().as_secs_f64();
                let (bd, bt) = (p.bytes_done.load(Relaxed), p.bytes_total.load(Relaxed));
                (
                    Some(r.task.clone()),
                    !r.done,
                    r.result.clone(),
                    r.ctx.lines(),
                    serde_json::json!({
                        "files_done": p.files_done.load(Relaxed), "files_total": p.files_total.load(Relaxed),
                        "bytes_done": bd, "bytes_total": bt, "elapsed": el,
                        "mb_s": if el > 0.5 { bd as f64 / el / 1e6 } else { 0.0 },
                    }),
                )
            }
            None => (None, false, None, vec![], serde_json::json!(null)),
        };
        serde_json::json!({
            "task": task, "running": running, "result": result, "log": log, "progress": prog,
            "events": self.events.lines(),
            "schedule": self.cfg().schedule.iter().map(|s| format!("{} at {} {}", s.task, s.at, if s.days.is_empty() { "daily".into() } else { s.days.join(",") })).collect::<Vec<_>>(),
        })
    }
}

/// Fires configured schedule entries (local time, minute resolution) and `run_on_start` tasks.
pub fn scheduler(app: Arc<App>) {
    for t in app.cfg().daemon.run_on_start.clone() {
        // Start-up tasks queue behind each other.
        while app.start(&t, false, false, None).is_err() {
            std::thread::sleep(std::time::Duration::from_secs(5));
        }
        app.events.log(format!("started {t} on daemon start"));
        loop {
            std::thread::sleep(std::time::Duration::from_secs(2));
            if !app.job.lock().unwrap().as_ref().map(|r| !r.done).unwrap_or(false) {
                break;
            }
        }
    }
    let mut fired: HashMap<usize, String> = HashMap::new();
    loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        let now = Local::now();
        let key = now.format("%Y-%m-%d %H:%M").to_string();
        let cfg = app.cfg();
        for (i, s) in cfg.schedule.iter().enumerate() {
            let Ok((h, m)) = parse_hhmm(&s.at) else { continue };
            if now.hour() != h || now.minute() != m || fired.get(&i) == Some(&key) {
                continue;
            }
            if !s.days.is_empty() && !s.days.iter().filter_map(|d| parse_day(d).ok()).any(|d| d == now.weekday()) {
                continue;
            }
            fired.insert(i, key.clone());
            match app.start(&s.task, false, false, None) {
                Ok(()) => app.events.log(format!("scheduled {} started", s.task)),
                Err(e) => app.events.log(format!("scheduled {} not started: {e}", s.task)),
            }
        }
    }
}
