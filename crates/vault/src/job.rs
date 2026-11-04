use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Default)]
pub struct Progress {
    pub files_total: AtomicU64,
    pub files_done: AtomicU64,
    pub bytes_total: AtomicU64,
    pub bytes_done: AtomicU64,
}

/// Shared state for one running job: log, progress, cancel flag.
pub struct Ctx {
    pub dry: bool,
    pub apply: bool,
    pub cancel: AtomicBool,
    pub progress: Progress,
    log: Mutex<VecDeque<String>>,
    pub started: Mutex<Instant>,
    pub echo: bool,
    /// Overrides `archive.existing_jpg` for this run.
    pub existing: Mutex<Option<crate::config::ExistingJpg>>,
}

impl Ctx {
    pub fn new(dry: bool, apply: bool, echo: bool) -> Arc<Ctx> {
        Arc::new(Ctx {
            dry,
            apply,
            cancel: AtomicBool::new(false),
            progress: Progress::default(),
            log: Mutex::new(VecDeque::new()),
            started: Mutex::new(Instant::now()),
            echo,
            existing: Mutex::new(None),
        })
    }

    pub fn log(&self, msg: impl AsRef<str>) {
        let line = format!("{} {}", chrono::Local::now().format("%H:%M:%S"), msg.as_ref());
        if self.echo {
            eprintln!("{line}");
        }
        let mut l = self.log.lock().unwrap();
        if l.len() >= 500 {
            l.pop_front();
        }
        l.push_back(line);
    }

    pub fn lines(&self) -> Vec<String> {
        self.log.lock().unwrap().iter().cloned().collect()
    }

    pub fn cancelled(&self) -> bool {
        self.cancel.load(Relaxed)
    }

    pub fn reset_progress(&self, files: u64, bytes: u64) {
        let p = &self.progress;
        p.files_total.store(files, Relaxed);
        p.bytes_total.store(bytes, Relaxed);
        p.files_done.store(0, Relaxed);
        p.bytes_done.store(0, Relaxed);
        *self.started.lock().unwrap() = Instant::now();
    }

    pub fn add_bytes(&self, n: u64) {
        self.progress.bytes_done.fetch_add(n, Relaxed);
    }

    pub fn add_file(&self) {
        self.progress.files_done.fetch_add(1, Relaxed);
    }
}
