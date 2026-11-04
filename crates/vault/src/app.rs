use anyhow::{bail, Result};
use chrono::{Datelike, Local, Timelike};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::config::{parse_day, parse_hhmm, Config};
use crate::job::Ctx;

pub struct Running {
    pub task: String,
    pub ctx: Arc<Ctx>,
    pub done: bool,
    pub result: Option<String>,
}

pub struct App {
    pub cfg: Config,
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
    pub fn new(cfg: Config) -> Arc<App> {
        Arc::new(App { cfg, job: Mutex::new(None), events: Ctx::new(false, false, true) })
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
            let r = run_task(&app.cfg, &task, &ctx, None);
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
            "schedule": self.cfg.schedule.iter().map(|s| format!("{} at {} {}", s.task, s.at, if s.days.is_empty() { "daily".into() } else { s.days.join(",") })).collect::<Vec<_>>(),
        })
    }
}

/// Fires configured schedule entries (local time, minute resolution) and `run_on_start` tasks.
pub fn scheduler(app: Arc<App>) {
    for t in app.cfg.daemon.run_on_start.clone() {
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
        for (i, s) in app.cfg.schedule.iter().enumerate() {
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
