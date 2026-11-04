use crate::config::Config;
use crate::job::Ctx;
use crate::util::*;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};

fn cfg(root: &Path) -> Config {
    let t = format!(
        r#"[paths]
ingest = "{r}/ingest"
local_backup = "{r}/local"
network_backup = "{r}/net"
[sync]
dest = "{r}/share"
threads = 3
verify = true
[[sync.source]]
path = "{r}/ingest"
label = "ing"
[ingest]
min_idle_minutes = 0
threads = 2
"#,
        r = root.display()
    );
    let c: Config = toml::from_str(&t).unwrap();
    c.validate().unwrap();
    for d in ["ingest", "local", "net", "share"] {
        fs::create_dir_all(root.join(d)).unwrap();
    }
    c
}

/// Write a file and stamp it with a date well inside the plausible range.
fn put(p: &Path, data: &[u8], year_secs_ago: u64) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, data).unwrap();
    let f = fs::File::options().write(true).open(p).unwrap();
    f.set_modified(SystemTime::now() - Duration::from_secs(year_secs_ago)).unwrap();
}

const YEAR: u64 = 365 * 86400;

#[test]
fn safe_join_rejects_traversal() {
    let r = Path::new("/r");
    assert!(safe_join(r, Path::new("a/b")).is_ok());
    for bad in ["../x", "/abs", "a/../b"] {
        assert!(safe_join(r, Path::new(bad)).is_err(), "{bad}");
    }
}

#[test]
fn sync_copies_then_skips_then_updates() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    put(&t.path().join("ingest/P/a.arw"), &vec![7u8; 3_000_000], YEAR);
    put(&t.path().join("ingest/P/sub/b.jpg"), b"bee", YEAR);
    let ctx = Ctx::new(false, false, false);
    crate::sync::run(&c, &ctx).unwrap();
    assert_eq!(fs::read(t.path().join("share/ing/P/a.arw")).unwrap().len(), 3_000_000);
    assert_eq!(fs::read(t.path().join("share/ing/P/sub/b.jpg")).unwrap(), b"bee");
    // unchanged: nothing to copy
    let ctx = Ctx::new(false, false, false);
    crate::sync::run(&c, &ctx).unwrap();
    assert!(ctx.lines().iter().any(|l| l.contains("0 file(s) to copy")));
    // changed file is replaced
    put(&t.path().join("ingest/P/sub/b.jpg"), b"bumblebee", YEAR / 2);
    crate::sync::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert_eq!(fs::read(t.path().join("share/ing/P/sub/b.jpg")).unwrap(), b"bumblebee");
    // nothing is deleted on the share when a source file disappears
    fs::remove_file(t.path().join("ingest/P/a.arw")).unwrap();
    crate::sync::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert!(t.path().join("share/ing/P/a.arw").exists());
}

#[test]
fn ingest_copies_to_year_and_frees_cache() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    // oldest plausible date wins: file times 3 and 1 years ago -> 3 years ago decides the year
    put(&t.path().join("ingest/Wedding/a.arw"), b"raw-a", 3 * YEAR);
    put(&t.path().join("ingest/Wedding/b.jpg"), b"jpg-b", YEAR);
    let year = crate::dates::year_of((SystemTime::now() - Duration::from_secs(3 * YEAR)).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64);
    // dry run changes nothing
    crate::ingest::run(&c, &Ctx::new(true, false, false)).unwrap();
    assert!(t.path().join("ingest/Wedding/a.arw").exists());
    assert!(!t.path().join("local").join(year.to_string()).exists());

    crate::ingest::run(&c, &Ctx::new(false, false, false)).unwrap();
    let dest = t.path().join("local").join(year.to_string()).join("Wedding");
    assert_eq!(fs::read(dest.join("a.arw")).unwrap(), b"raw-a");
    assert_eq!(fs::read(dest.join("b.jpg")).unwrap(), b"jpg-b");
    assert!(!t.path().join("ingest/Wedding").exists(), "cache freed");
}

#[test]
fn ingest_keeps_cache_on_conflict_and_when_busy() {
    let t = tempfile::tempdir().unwrap();
    let mut c = cfg(t.path());
    put(&t.path().join("ingest/Trip/a.arw"), b"new", 2 * YEAR);
    // an existing different file in the local backup must never be overwritten
    let y = crate::dates::year_of((SystemTime::now() - Duration::from_secs(2 * YEAR)).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64);
    put(&t.path().join(format!("local/{y}/Trip/a.arw")), b"DIFFERENT", 2 * YEAR);
    assert!(crate::ingest::run(&c, &Ctx::new(false, false, false)).is_err());
    assert_eq!(fs::read(t.path().join(format!("local/{y}/Trip/a.arw"))).unwrap(), b"DIFFERENT");
    assert!(t.path().join("ingest/Trip/a.arw").exists());
    // recently modified projects are skipped
    c.ingest.min_idle_minutes = 60;
    fs::remove_dir_all(t.path().join(format!("local/{y}"))).unwrap();
    put(&t.path().join("ingest/Fresh/a.arw"), b"x", 0);
    crate::ingest::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert!(t.path().join("ingest/Fresh/a.arw").exists());
}

#[test]
fn ingest_with_existing_identical_local_copy_deletes_cache_only() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    put(&t.path().join("ingest/Same/a.arw"), b"same", 2 * YEAR);
    put(&t.path().join("local/2020/Same/a.arw"), b"same", 2 * YEAR);
    crate::ingest::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert!(!t.path().join("ingest/Same").exists());
    assert!(!t.path().join("local/2020/Same/a.arw.vtmp").exists());
    assert_eq!(fs::read(t.path().join("local/2020/Same/a.arw")).unwrap(), b"same");
}

#[test]
fn archive_zips_jpg_only_projects_and_keeps_mixed_ones() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    put(&t.path().join("local/2015/Done/1.jpg"), &vec![1u8; 100_000], 5 * YEAR);
    put(&t.path().join("local/2015/Done/s/2.JPG"), &vec![2u8; 50_000], 5 * YEAR);
    put(&t.path().join("local/2016/Mixed/1.jpg"), b"a", 5 * YEAR);
    put(&t.path().join("local/2016/Mixed/notes.txt"), b"keep", 5 * YEAR);
    crate::archive::run(&c, &Ctx::new(true, false, false), None).unwrap();
    assert!(t.path().join("local/2015/Done").is_dir(), "dry run changes nothing");
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    assert!(!t.path().join("local/2015/Done").exists());
    let z = zip::ZipArchive::new(fs::File::open(t.path().join("local/2015/Done.zip")).unwrap()).unwrap();
    assert_eq!(z.len(), 2);
    assert!(z.file_names().any(|n| n.map(|s| s == "Done/s/2.JPG").unwrap_or(false)));
    assert!(t.path().join("local/2016/Mixed/1.jpg").exists(), "mixed project untouched");
    assert!(!t.path().join("local/2016/Mixed.zip").exists());
}

#[test]
fn dates_reports_and_moves_misfiled_projects() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    let y = |n: u64| crate::dates::year_of((SystemTime::now() - Duration::from_secs(n * YEAR)).duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() as i64);
    // 1980 timestamps are ignored, the 4-year-old file decides
    put(&t.path().join("net/Unsorted/a.jpg"), b"a", 4 * YEAR);
    let f = fs::File::options().write(true).open(t.path().join("net/Unsorted/a.jpg")).unwrap();
    drop(f);
    put(&t.path().join("local/1999/WrongYear/b.jpg"), b"b", 4 * YEAR);
    put(&t.path().join(format!("local/{}/Right/c.jpg", y(4))), b"c", 4 * YEAR);
    crate::dates::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert!(t.path().join("net/Unsorted").exists(), "report only");
    crate::dates::run(&c, &Ctx::new(false, true, false)).unwrap();
    assert!(t.path().join(format!("net/{}/Unsorted/a.jpg", y(4))).exists());
    assert!(t.path().join(format!("local/{}/WrongYear/b.jpg", y(4))).exists());
    assert!(t.path().join(format!("local/{}/Right/c.jpg", y(4))).exists());
}

#[test]
fn pre_2010_dates_are_not_plausible() {
    assert!(!crate::dates::plausible(0));
    assert!(!crate::dates::plausible(946684800));
    assert!(crate::dates::plausible(1_400_000_000));
}

#[test]
fn archive_converts_arw_checks_jpg_then_deletes_arw_and_zips() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    let d = t.path().join("local/2014/Raws");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("a.ARW"), vault_raw::testdng::make(256, 192)).unwrap();
    fs::write(d.join("b.arw"), vault_raw::testdng::make(128, 96)).unwrap();
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    assert!(!d.exists());
    let mut z = zip::ZipArchive::new(fs::File::open(t.path().join("local/2014/Raws.zip")).unwrap()).unwrap();
    assert_eq!(z.len(), 2);
    let mut jpg = Vec::new();
    std::io::Read::read_to_end(&mut z.by_name("Raws/a.jpg").unwrap(), &mut jpg).unwrap();
    assert_eq!(vault_raw::probe_decode_bytes(&jpg).unwrap().width, 256);
}

#[test]
fn archive_never_overwrites_an_existing_jpg() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    let d = t.path().join("local/2014/Clash");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("a.arw"), vault_raw::testdng::make(64, 48)).unwrap();
    fs::write(d.join("a.jpg"), b"users own edit").unwrap();
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    let z = zip::ZipArchive::new(fs::File::open(t.path().join("local/2014/Clash.zip")).unwrap()).unwrap();
    assert_eq!(z.len(), 2, "user's a.jpg and a_arw.jpg both kept");
}

fn clash(t: &Path, mode: crate::config::ExistingJpg) -> (Config, std::path::PathBuf) {
    let mut c = cfg(t);
    c.archive.existing_jpg = mode;
    let d = t.join("local/2014/Clash");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("a.arw"), vault_raw::testdng::make(64, 48)).unwrap();
    // a real, decodable JPG with the same name
    let (_, jpg) = vault_raw::render_jpg(&d.join("a.arw"), vault_raw::ColourMode::Preserve, 3, &vault_raw::Render::default()).unwrap();
    fs::write(d.join("a.jpg"), &jpg).unwrap();
    (c, d)
}

#[test]
fn existing_jpg_use_existing_deletes_arw_without_converting() {
    let t = tempfile::tempdir().unwrap();
    let (mut c, d) = clash(t.path(), crate::config::ExistingJpg::UseExisting);
    fs::write(d.join("keep.txt"), b"x").unwrap(); // stop the zip step so files can be inspected
    c.archive.threads = 1;
    let before = fs::read(d.join("a.jpg")).unwrap();
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    assert!(!d.join("a.arw").exists());
    assert!(!d.join("a_arw.jpg").exists());
    assert_eq!(fs::read(d.join("a.jpg")).unwrap(), before);
}

#[test]
fn existing_jpg_use_existing_keeps_arw_if_jpg_is_broken() {
    let t = tempfile::tempdir().unwrap();
    let (c, d) = clash(t.path(), crate::config::ExistingJpg::UseExisting);
    fs::write(d.join("a.jpg"), b"garbage").unwrap();
    assert!(crate::archive::run(&c, &Ctx::new(false, false, false), None).is_err());
    assert!(d.join("a.arw").exists());
}

#[test]
fn existing_jpg_overwrite_replaces_it() {
    let t = tempfile::tempdir().unwrap();
    let (c, d) = clash(t.path(), crate::config::ExistingJpg::Overwrite);
    fs::write(d.join("a.jpg"), b"old edit").unwrap();
    fs::write(d.join("keep.txt"), b"x").unwrap();
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    assert!(!d.join("a.arw").exists());
    assert!(!d.join("a_arw.jpg").exists());
    assert!(vault_raw::probe_decode(&d.join("a.jpg")).is_ok());
}

#[test]
fn existing_jpg_override_from_ctx_beats_config() {
    let t = tempfile::tempdir().unwrap();
    let (c, d) = clash(t.path(), crate::config::ExistingJpg::Rename);
    fs::write(d.join("keep.txt"), b"x").unwrap();
    let ctx = Ctx::new(false, false, false);
    *ctx.existing.lock().unwrap() = Some(crate::config::ExistingJpg::UseExisting);
    crate::archive::run(&c, &ctx, None).unwrap();
    assert!(!d.join("a.arw").exists() && !d.join("a_arw.jpg").exists());
}
