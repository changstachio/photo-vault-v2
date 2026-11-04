use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use vault_raw::{ColourMode, Render};

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub paths: Paths,
    #[serde(default)]
    pub sync: Sync,
    #[serde(default)]
    pub ingest: Ingest,
    #[serde(default)]
    pub archive: Archive,
    #[serde(default)]
    pub web: Web,
    #[serde(default)]
    pub daemon: Daemon,
    #[serde(default)]
    pub schedule: Vec<Schedule>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Paths {
    /// Temporary SSD ingest cache; each subfolder is a project.
    pub ingest: PathBuf,
    /// Local `Sony Backup` folder (`<year>/<project>`).
    pub local_backup: PathBuf,
    /// Network `Sony Backup` folder (mounted share).
    pub network_backup: PathBuf,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct Sync {
    /// Destination on the network share.
    pub dest: Option<PathBuf>,
    pub threads: usize,
    pub buffer_mib: usize,
    /// Re-read and hash every copied file from the destination.
    pub verify: bool,
    /// If set, this path must be a mount point (refuses to copy onto the local disk if the share is missing).
    pub require_mount: Option<PathBuf>,
    #[serde(rename = "source")]
    pub sources: Vec<Source>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub path: PathBuf,
    /// Folder name under `dest`; defaults to the last path component.
    pub label: Option<String>,
}

impl Default for Sync {
    fn default() -> Self {
        Self { dest: None, threads: 6, buffer_mib: 8, verify: false, require_mount: None, sources: vec![] }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct Ingest {
    /// Skip projects with a file modified more recently than this.
    pub min_idle_minutes: u64,
    pub threads: usize,
}

impl Default for Ingest {
    fn default() -> Self {
        Self { min_idle_minutes: 30, threads: 4 }
    }
}

/// What to do when an ARW has a JPG with the same name beside it.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ExistingJpg {
    /// Keep the existing JPG and save the conversion as `name_arw.jpg`.
    #[default]
    Rename,
    /// Treat the existing JPG as the converted copy: delete the ARW without converting.
    UseExisting,
    /// Replace the existing JPG with a fresh conversion.
    Overwrite,
}

impl std::str::FromStr for ExistingJpg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "rename" => Ok(Self::Rename),
            "use_existing" => Ok(Self::UseExisting),
            "overwrite" => Ok(Self::Overwrite),
            _ => Err(format!("{s:?}: expected rename, use_existing or overwrite")),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct Archive {
    pub existing_jpg: ExistingJpg,
    pub threads: usize,
    pub demosaic: i32,
    pub colour: ColourMode,
    pub render: Render,
}

impl Default for Archive {
    fn default() -> Self {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self { existing_jpg: ExistingJpg::Rename, threads: (n / 2).max(1), demosaic: 3, colour: ColourMode::Preserve, render: Render::default() }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct Web {
    pub enabled: bool,
    pub bind: String,
}

impl Default for Web {
    fn default() -> Self {
        Self { enabled: true, bind: "127.0.0.1:8787".into() }
    }
}

#[derive(Deserialize, Clone, Debug, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Daemon {
    /// Tasks to run once when the daemon starts (e.g. at boot).
    pub run_on_start: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub task: String,
    /// Local time, `HH:MM`.
    pub at: String,
    /// `mon`..`sun`; empty = every day.
    #[serde(default)]
    pub days: Vec<String>,
}

pub const TASKS: [&str; 4] = ["sync", "ingest", "archive", "dates"];

pub fn parse_hhmm(s: &str) -> Result<(u32, u32)> {
    let (h, m) = s.split_once(':').with_context(|| format!("bad time {s:?}, expected HH:MM"))?;
    let (h, m): (u32, u32) = (h.parse()?, m.parse()?);
    if h > 23 || m > 59 {
        bail!("bad time {s:?}");
    }
    Ok((h, m))
}

pub fn parse_day(s: &str) -> Result<chrono::Weekday> {
    s.parse::<chrono::Weekday>().map_err(|_| anyhow::anyhow!("bad day {s:?}, expected mon..sun"))
}

impl Config {
    pub fn load(p: &Path) -> Result<Config> {
        let s = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        let c: Config = toml::from_str(&s).with_context(|| format!("parsing {}", p.display()))?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        for s in &self.schedule {
            if !TASKS.contains(&s.task.as_str()) {
                bail!("schedule: unknown task {:?}", s.task);
            }
            parse_hhmm(&s.at)?;
            for d in &s.days {
                parse_day(d)?;
            }
        }
        for t in &self.daemon.run_on_start {
            if !TASKS.contains(&t.as_str()) {
                bail!("daemon.run_on_start: unknown task {t:?}");
            }
        }
        if !(1..=64).contains(&self.sync.threads) || !(1..=64).contains(&self.ingest.threads) || !(1..=64).contains(&self.archive.threads) {
            bail!("threads must be 1..=64");
        }
        if !(1..=256).contains(&self.sync.buffer_mib) {
            bail!("sync.buffer_mib must be 1..=256");
        }
        if !(50..=100).contains(&self.archive.render.quality) {
            bail!("archive.render.quality must be 50..=100");
        }
        for s in &self.sync.sources {
            if let Some(l) = &s.label {
                if l.is_empty() || l.contains(['/', '\\']) || l == "." || l == ".." {
                    bail!("sync.source label {l:?} must be a plain folder name");
                }
            }
        }
        Ok(())
    }
}

pub fn default_path() -> PathBuf {
    if let Ok(p) = std::env::var("VAULT_CONFIG") {
        return p.into();
    }
    let user = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/photovault/config.toml"));
    match user {
        Some(u) if u.exists() => u,
        _ => PathBuf::from("/etc/photovault/config.toml"),
    }
}

pub const EXAMPLE: &str = r#"# photovault config
[paths]
ingest = "/mnt/ssd/ingest"                  # temporary SSD ingest cache; each subfolder is a project
local_backup = "/mnt/backup/Sony Backup"    # layout: <year>/<project title>
network_backup = "/mnt/nas/Sony Backup"     # mounted share (use /etc/fstab or systemd automount)

[sync]                                      # part 1: copy chosen folders to the share (never deletes)
dest = "/mnt/nas/Photos"
threads = 6                                 # parallel files; raise for many small files
buffer_mib = 8
verify = false                              # true re-reads every copy (halves throughput)
require_mount = "/mnt/nas"                  # refuse to run unless this is a mounted filesystem

[[sync.source]]
path = "/mnt/ssd/ingest"
# label = "ingest"                          # folder name under dest (default: last path component)

[ingest]                                    # part 2: ingest cache -> local backup, then free the cache
min_idle_minutes = 30                       # skip projects touched more recently than this
threads = 4

[archive]                                   # part 3: ARW -> JPG, then zip JPG-only projects
threads = 4
existing_jpg = "rename"                   # ARW with a same-name JPG: rename | use_existing (delete the ARW, no conversion) | overwrite
demosaic = 3
colour = "preserve"                         # preserve | srgb | adobergb | prophoto
[archive.render]
quality = 100

[web]
enabled = true
bind = "127.0.0.1:8787"                     # no login; keep on localhost or behind a reverse proxy

[daemon]
run_on_start = []                           # e.g. ["sync"] to run once at boot

[[schedule]]
task = "sync"
at = "02:00"
# days = ["mon", "wed", "fri"]              # omit for every day

[[schedule]]
task = "ingest"
at = "04:00"
"#;
