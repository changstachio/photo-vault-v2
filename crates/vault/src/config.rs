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

/// What `archive` does to a project folder.
#[derive(Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ArchiveMode {
    /// Leave the folder alone.
    #[default]
    None,
    /// ARW to JPG, delete the ARW; no zip.
    Convert,
    /// ARW to JPG, then zip the folder (any file types; no ARW may remain).
    ConvertZip,
    /// Task 5 only: re-encode the JPGs already in the folder or `.zip` at the configured quality.
    Shrink,
}

impl ArchiveMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Convert => "convert",
            Self::ConvertZip => "convert_zip",
            Self::Shrink => "shrink",
        }
    }
}

/// Applies to the folder and everything under it; the deepest matching rule wins.
#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub path: PathBuf,
    pub mode: ArchiveMode,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct Archive {
    /// Mode for projects no rule matches. `none` means nothing is touched until you choose folders.
    pub default: ArchiveMode,
    /// Task 5 replaces a zip only if the JPGs inside shrink by at least this many percent.
    pub shrink_min_saving_percent: f32,
    #[serde(rename = "rule")]
    pub rules: Vec<Rule>,
    pub existing_jpg: ExistingJpg,
    pub threads: usize,
    pub demosaic: i32,
    pub colour: ColourMode,
    pub render: Render,
}

impl Default for Archive {
    fn default() -> Self {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        Self { default: ArchiveMode::None, shrink_min_saving_percent: 5.0, rules: vec![], existing_jpg: ExistingJpg::Rename, threads: (n / 2).max(1), demosaic: 3, colour: ColourMode::Preserve, render: Render::default() }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(deny_unknown_fields, default)]
pub struct Web {
    pub enabled: bool,
    pub bind: String,
    /// Folders the file picker may show and select from. Empty = the ingest and local backup folders.
    pub browse_roots: Vec<PathBuf>,
    /// Extra host names/IPs the UI may be reached by (besides localhost and the bind address).
    pub allowed_hosts: Vec<String>,
}

impl Default for Web {
    fn default() -> Self {
        Self { enabled: true, bind: "127.0.0.1:8787".into(), browse_roots: vec![], allowed_hosts: vec![] }
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

pub const TASKS: [&str; 6] = ["sync", "ingest", "archive", "dates", "shrink", "repair"];

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
    /// Mode for a project folder: the rule on the deepest folder containing it, else `archive.default`.
    pub fn archive_mode(&self, project: &Path) -> ArchiveMode {
        let proj = project.canonicalize().unwrap_or_else(|_| project.to_path_buf());
        self.archive
            .rules
            .iter()
            .filter(|r| proj.starts_with(r.path.canonicalize().unwrap_or_else(|_| r.path.clone())))
            .max_by_key(|r| r.path.components().count())
            .map(|r| r.mode)
            .unwrap_or(self.archive.default)
    }

    /// Why each configured picker folder is or is not usable (None = usable).
    pub fn browse_root_status(&self) -> Vec<(PathBuf, Option<String>)> {
        let list: Vec<&PathBuf> = if self.web.browse_roots.is_empty() { vec![&self.paths.ingest, &self.paths.local_backup] } else { self.web.browse_roots.iter().collect() };
        list.into_iter()
            .map(|p| {
                let why = match std::fs::metadata(p) {
                    Ok(m) if m.is_dir() => std::fs::read_dir(p).err().map(|e| format!("cannot be read by the service user ({e})")),
                    Ok(_) => Some("is not a folder".to_string()),
                    Err(e) => Some(format!("not accessible to the service user: {e}")),
                };
                (p.clone(), why)
            })
            .collect()
    }

    /// Existing folders the web file picker may show, canonicalised.
    pub fn browse_roots(&self) -> Vec<PathBuf> {
        let list: Vec<&PathBuf> = if self.web.browse_roots.is_empty() { vec![&self.paths.ingest, &self.paths.local_backup] } else { self.web.browse_roots.iter().collect() };
        list.into_iter().filter_map(|p| p.canonicalize().ok()).filter(|p| p.is_dir()).collect()
    }

    /// Canonical path of `p` if it exists and lies inside a browse root (blocks `..` and symlink escapes).
    pub fn pick_path(&self, p: &Path) -> Result<PathBuf> {
        let c = p.canonicalize().with_context(|| format!("{} does not exist", p.display()))?;
        if self.browse_roots().iter().any(|r| c.starts_with(r)) {
            Ok(c)
        } else {
            bail!("{} is outside the allowed folders (web.browse_roots)", p.display())
        }
    }

    pub fn load(p: &Path) -> Result<Config> {
        let s = std::fs::read_to_string(p).with_context(|| format!("reading {}", p.display()))?;
        let c: Config = toml::from_str(&s).with_context(|| format!("parsing {}", p.display()))?;
        c.validate()?;
        Ok(c)
    }

    pub fn validate(&self) -> Result<()> {
        let abs = |what: &str, p: &Path| -> Result<()> {
            let t = p.to_string_lossy();
            if !p.is_absolute() {
                bail!("{what} = {t:?} must be an absolute path to a mounted folder (not a URL like smb://...; mount the share first, e.g. with /etc/fstab or `gio mount`, and use the mount path)");
            }
            Ok(())
        };
        abs("paths.ingest", &self.paths.ingest)?;
        abs("paths.local_backup", &self.paths.local_backup)?;
        abs("paths.network_backup", &self.paths.network_backup)?;
        if let Some(d) = &self.sync.dest {
            abs("sync.dest", d)?;
            if d.components().count() <= 1 {
                bail!("sync.dest = {:?} is the filesystem root; point it at the folder on the share", d.to_string_lossy());
            }
        }
        // Folders that must not overlap: ingest deletes what it has backed up, so a backup folder
        // inside the ingest folder (or the other way round) would be copied into itself and deleted.
        let pairs = [
            ("paths.ingest", &self.paths.ingest, "paths.local_backup", &self.paths.local_backup),
            ("paths.ingest", &self.paths.ingest, "paths.network_backup", &self.paths.network_backup),
        ];
        for (an, a, bn, b) in pairs {
            if overlaps(a, b) {
                bail!("{an} ({}) and {bn} ({}) overlap; they must be separate folders", a.display(), b.display());
            }
        }
        if let Some(d) = &self.sync.dest {
            for src in &self.sync.sources {
                if overlaps(&src.path, d) {
                    bail!("sync source {} and sync.dest {} overlap; the share would be copied into itself", src.path.display(), d.display());
                }
            }
        }
        for r in &self.web.browse_roots {
            abs("web.browse_roots", r)?;
        }
        for r in &self.archive.rules {
            abs("archive.rule path", &r.path)?;
        }
        for src in &self.sync.sources {
            abs("sync.source path", &src.path)?;
        }
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
        if !(30..=100).contains(&self.archive.render.quality) {
            bail!("archive.render.quality must be 30..=100");
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

/// True if one path is the other or lies inside it (compared after resolving symlinks where the
/// paths exist, and lexically otherwise).
pub fn overlaps(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| -> PathBuf {
        if let Ok(c) = p.canonicalize() {
            return c;
        }
        let mut out = PathBuf::new();
        for comp in p.components() {
            match comp {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                c => out.push(c),
            }
        }
        out
    };
    let (a, b) = (norm(a), norm(b));
    a.starts_with(&b) || b.starts_with(&a)
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

[archive]                                   # part 3: ARW -> JPG, then zip projects with no ARW left
default = "none"                            # projects without a rule: none | convert | convert_zip | shrink. Choose folders in the web UI.
shrink_min_saving_percent = 5               # task 5 keeps a zip as is unless its JPGs shrink by at least this much
threads = 4
existing_jpg = "rename"                   # ARW with a same-name JPG: rename | use_existing (delete the ARW, no conversion) | overwrite
demosaic = 3
colour = "preserve"                         # preserve | srgb | adobergb | prophoto
[archive.render]
quality = 90                                # JPEG quality 30-100; lower = smaller files
chroma = "420"                              # 420 (smallest) | 422 | 444 (largest)

# [[archive.rule]]                          # normally written by the web UI
# path = "/mnt/backup/Sony Backup/2015"     # applies to everything under this folder; the deepest rule wins
# mode = "convert_zip"

[web]
enabled = true
bind = "127.0.0.1:8787"                     # no login; keep on localhost or behind a reverse proxy
allowed_hosts = []                          # other names/IPs used to open the UI, e.g. ["rm-ubt.local"] when bind = "0.0.0.0:8787"
browse_roots = []                           # folders the picker may show; empty = ingest + local_backup. e.g. ["/mnt/ssd", "/home/me/Pictures"]

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
