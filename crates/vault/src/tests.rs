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
[archive]
default = "convert_zip"
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
fn archive_zips_any_project_without_arw() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    put(&t.path().join("local/2015/Done/1.jpg"), &vec![1u8; 100_000], 5 * YEAR);
    put(&t.path().join("local/2015/Done/s/2.JPG"), &vec![2u8; 50_000], 5 * YEAR);
    put(&t.path().join("local/2016/Mixed/1.jpg"), b"a", 5 * YEAR);
    put(&t.path().join("local/2016/Mixed/notes.txt"), b"keep", 5 * YEAR);
    put(&t.path().join("local/2016/Mixed/edit.xmp"), b"<x/>", 5 * YEAR);
    fs::create_dir_all(t.path().join("local/2016/Mixed/empty")).unwrap();
    crate::archive::run(&c, &Ctx::new(true, false, false), None).unwrap();
    assert!(t.path().join("local/2015/Done").is_dir(), "dry run changes nothing");
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    assert!(!t.path().join("local/2015/Done").exists());
    let z = zip::ZipArchive::new(fs::File::open(t.path().join("local/2015/Done.zip")).unwrap()).unwrap();
    assert_eq!(z.len(), 3, "two files and the subfolder entry");
    assert!(z.file_names().any(|n| n.map(|s| s == "Done/s/2.JPG").unwrap_or(false)));
    // other file types and empty subfolders are fine: zipped, nothing lost
    assert!(!t.path().join("local/2016/Mixed").exists());
    let z = zip::ZipArchive::new(fs::File::open(t.path().join("local/2016/Mixed.zip")).unwrap()).unwrap();
    let names: Vec<String> = z.file_names().map(|n| n.unwrap().to_string()).collect();
    for want in ["Mixed/1.jpg", "Mixed/notes.txt", "Mixed/edit.xmp", "Mixed/empty/"] {
        assert!(names.iter().any(|n| n == want), "{want} missing from {names:?}");
    }
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
    c.archive.default = crate::config::ArchiveMode::Convert; // convert only, so the files can be inspected
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
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    assert!(!d.join("a.arw").exists());
    assert!(!d.join("a_arw.jpg").exists());
    assert!(vault_raw::probe_decode(&d.join("a.jpg")).is_ok());
}

#[test]
fn existing_jpg_override_from_ctx_beats_config() {
    let t = tempfile::tempdir().unwrap();
    let (c, d) = clash(t.path(), crate::config::ExistingJpg::Rename);
    let ctx = Ctx::new(false, false, false);
    *ctx.existing.lock().unwrap() = Some(crate::config::ExistingJpg::UseExisting);
    crate::archive::run(&c, &ctx, None).unwrap();
    assert!(!d.join("a.arw").exists() && !d.join("a_arw.jpg").exists());
}

#[test]
fn save_config_validates_writes_and_applies() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("config.toml");
    let good = crate::config::EXAMPLE.to_string();
    fs::write(&path, &good).unwrap();
    let app = crate::app::App::new(toml::from_str(&good).unwrap(), path.clone());
    // invalid input is rejected and the file is untouched
    assert!(app.save_config("[paths]\ningest = 1").is_err());
    assert!(app.save_config(&good.replace("threads = 6", "threads = 0")).is_err());
    assert!(app.save_config(&good.replace("task = \"sync\"", "task = \"nope\"")).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), good);
    // valid change is written, applied, and flags a restart when the bind changes
    let new = good.replace("threads = 6", "threads = 9").replace("127.0.0.1:8787", "127.0.0.1:9999");
    let note = app.save_config(&new).unwrap();
    assert!(note.is_some());
    assert_eq!(app.cfg().sync.threads, 9);
    assert_eq!(fs::read_to_string(&path).unwrap(), new);
    assert!(!t.path().join(".config.toml.vtmp").exists());
}

#[test]
fn picker_sets_sources_keeps_comments_and_blocks_outside_paths() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    let mut text = format!(
        "# my notes\n[paths]\ningest = \"{r}/ingest\"\nlocal_backup = \"{r}/local\"\nnetwork_backup = \"{r}/net\"\n[sync]\ndest = \"{r}/share\" # keep\n[[sync.source]]\npath = \"{r}/ingest\"\n[ingest]\nthreads = 2\n",
        r = r.display()
    );
    text.push_str("[web]\nbind = \"127.0.0.1:1\"\n");
    for d in ["ingest/A", "ingest/B", "local", "net", "share", "outside"] {
        fs::create_dir_all(r.join(d)).unwrap();
    }
    fs::write(r.join("ingest/A/x.jpg"), b"x").unwrap();
    let path = r.join("c.toml");
    fs::write(&path, &text).unwrap();
    let app = crate::app::App::new(toml::from_str(&text).unwrap(), path.clone());
    let src = |p: std::path::PathBuf| crate::config::Source { path: p, label: None };
    // folder + single file are accepted
    app.set_sources(vec![src(r.join("ingest/A")), src(r.join("ingest/A/x.jpg")), src(r.join("ingest/A"))]).unwrap();
    assert_eq!(app.cfg().sync.sources.len(), 2);
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# my notes") && saved.contains("# keep") && saved.contains("threads = 2"));
    // outside the allowed folders, `..` escapes and missing paths are refused; file unchanged
    assert!(app.set_sources(vec![src(r.join("outside"))]).is_err());
    assert!(app.set_sources(vec![src(r.join("ingest/../outside"))]).is_err());
    assert!(app.set_sources(vec![src(r.join("ingest/nope"))]).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    // browsing
    let b = app.browse(&r.join("ingest").to_string_lossy(), false).unwrap();
    assert_eq!(b["entries"].as_array().unwrap().len(), 2);
    assert!(app.browse(&r.join("outside").to_string_lossy(), false).is_err());
    assert_eq!(app.browse("", false).unwrap()["entries"].as_array().unwrap().len(), 2);
    // clearing the list removes the entries
    app.set_sources(vec![]).unwrap();
    assert!(app.cfg().sync.sources.is_empty());
}

#[test]
fn sync_accepts_single_file_sources() {
    let t = tempfile::tempdir().unwrap();
    let mut c = cfg(t.path());
    put(&t.path().join("ingest/P/one.jpg"), b"1", YEAR);
    put(&t.path().join("ingest/P/two.jpg"), b"2", YEAR);
    put(&t.path().join("ingest/P/skip.jpg"), b"3", YEAR);
    c.sync.sources = ["one.jpg", "two.jpg"].iter().map(|f| crate::config::Source { path: t.path().join("ingest/P").join(f), label: None }).collect();
    crate::sync::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert!(t.path().join("share/P/one.jpg").exists() && t.path().join("share/P/two.jpg").exists());
    assert!(!t.path().join("share/P/skip.jpg").exists());
}

#[test]
fn url_decode_handles_percent_and_bad_input() {
    assert_eq!(crate::util::url_decode("a%20b%2Fc+d"), "a b/c d");
    assert_eq!(crate::util::url_decode("100%"), "100%");
    assert_eq!(crate::util::url_decode("%zz%é"), "%zz%é");
}

#[test]
fn config_rejects_urls_relative_paths_and_root_dest() {
    let t = tempfile::tempdir().unwrap();
    let good = cfg(t.path());
    let mut c = good.clone();
    c.paths.network_backup = "smb://10.0.0.8/share/x".into();
    assert!(c.validate().unwrap_err().to_string().contains("absolute"));
    let mut c = good.clone();
    c.sync.dest = Some("/".into());
    assert!(c.validate().unwrap_err().to_string().contains("root"));
    let mut c = good.clone();
    c.sync.sources[0].path = "relative/dir".into();
    assert!(c.validate().is_err());
}

#[test]
fn picker_reports_why_roots_are_unusable() {
    let t = tempfile::tempdir().unwrap();
    let mut c = cfg(t.path());
    c.paths.ingest = t.path().join("missing");
    c.paths.local_backup = t.path().join("missing2");
    let app = crate::app::App::new(c, t.path().join("c.toml"));
    let b = app.browse("", false).unwrap();
    assert_eq!(b["entries"].as_array().unwrap().len(), 0);
    assert_eq!(b["skipped"].as_array().unwrap().len(), 2);
}

#[test]
fn archive_rules_choose_convert_only_zip_or_skip_by_folder() {
    let t = tempfile::tempdir().unwrap();
    let mut c = cfg(t.path());
    c.archive.default = crate::config::ArchiveMode::None;
    let mk = |y: &str, n: &str| {
        let d = t.path().join(format!("local/{y}/{n}"));
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("a.arw"), vault_raw::testdng::make(64, 48)).unwrap();
        d
    };
    let (untouched, conv, zipd, skip) = (mk("2014", "Untouched"), mk("2015", "ConvOnly"), mk("2016", "Zipped"), mk("2016", "Skipped"));
    let rule = |p: &Path, m| crate::config::Rule { path: p.to_path_buf(), mode: m };
    use crate::config::ArchiveMode::*;
    c.archive.rules = vec![rule(&t.path().join("local/2015"), Convert), rule(&t.path().join("local/2016"), ConvertZip), rule(&skip, None)];
    crate::archive::run(&c, &Ctx::new(false, false, false), Option::None).unwrap();
    assert!(untouched.join("a.arw").exists(), "no rule: untouched");
    assert!(conv.join("a.jpg").exists() && !conv.join("a.arw").exists() && conv.is_dir(), "converted, not zipped");
    assert!(!zipd.exists() && t.path().join("local/2016/Zipped.zip").exists(), "converted and zipped");
    assert!(skip.join("a.arw").exists(), "deeper rule skips");
}

#[test]
fn archive_rules_save_only_inside_local_backup_and_keep_comments() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    let text = format!("# note\n[paths]\ningest = \"{r}/ingest\"\nlocal_backup = \"{r}/local\"\nnetwork_backup = \"{r}/net\"\n[archive]\nthreads = 2 # keep\n[archive.render]\nquality = 90\n", r = r.display());
    for d in ["ingest", "local/2015/P", "net", "other"] {
        fs::create_dir_all(r.join(d)).unwrap();
    }
    let path = r.join("c.toml");
    fs::write(&path, &text).unwrap();
    let app = crate::app::App::new(toml::from_str(&text).unwrap(), path.clone());
    let rule = |p: std::path::PathBuf, m| crate::config::Rule { path: p, mode: m };
    app.set_archive_rules(vec![rule(r.join("local/2015"), crate::config::ArchiveMode::ConvertZip)]).unwrap();
    assert_eq!(app.cfg().archive.rules.len(), 1);
    assert_eq!(app.cfg().archive_mode(&r.join("local/2015/P")), crate::config::ArchiveMode::ConvertZip);
    assert_eq!(app.cfg().archive_mode(&r.join("local/2016/Q")), crate::config::ArchiveMode::None);
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# note") && saved.contains("# keep") && saved.contains("quality = 90"));
    assert!(app.set_archive_rules(vec![rule(r.join("other"), crate::config::ArchiveMode::Convert)]).is_err());
    assert!(app.set_archive_rules(vec![rule(r.join("local/../other"), crate::config::ArchiveMode::Convert)]).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    let b = app.browse("", true).unwrap();
    assert_eq!(b["entries"].as_array().unwrap().len(), 1);
    assert!(app.browse(&r.join("other").to_string_lossy(), true).is_err());
}

#[test]
fn archive_does_not_zip_when_a_symlink_is_present() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    let d = t.path().join("local/2013/Link");
    put(&d.join("a.jpg"), b"a", 5 * YEAR);
    std::os::unix::fs::symlink("a.jpg", d.join("l.jpg")).unwrap();
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    assert!(d.join("a.jpg").exists() && !t.path().join("local/2013/Link.zip").exists());
}

#[test]
fn render_settings_save_and_smaller_jpgs() {
    let t = tempfile::tempdir().unwrap();
    let r = t.path();
    let text = format!("# note\n[paths]\ningest = \"{r}/i\"\nlocal_backup = \"{r}/l\"\nnetwork_backup = \"{r}/n\"\n[archive]\nthreads = 2\n", r = r.display());
    let path = r.join("c.toml");
    fs::write(&path, &text).unwrap();
    let app = crate::app::App::new(toml::from_str(&text).unwrap(), path.clone());
    assert_eq!(app.cfg().archive.render.quality, 90, "default quality");
    app.set_render(80, vault_raw::Chroma::C422).unwrap();
    assert_eq!(app.cfg().archive.render.quality, 80);
    assert_eq!(app.cfg().archive.render.chroma, vault_raw::Chroma::C422);
    assert!(fs::read_to_string(&path).unwrap().contains("# note"));
    assert!(app.set_render(10, vault_raw::Chroma::C420).is_err());
    assert_eq!(app.cfg().archive.render.quality, 80);
    // the same picture gets smaller with lower quality and 4:2:0
    let dng = r.join("t.dng");
    fs::write(&dng, vault_raw::testdng::make(512, 384)).unwrap();
    let size = |q, c| vault_raw::render_jpg(&dng, vault_raw::ColourMode::Preserve, 3, &vault_raw::Render { quality: q, chroma: c, ..Default::default() }).unwrap().1.len();
    let big = size(100, vault_raw::Chroma::C444);
    assert!(size(90, vault_raw::Chroma::C420) < big && size(75, vault_raw::Chroma::C420) < size(90, vault_raw::Chroma::C420), "q100/444 = {big}");
}

fn big_jpg(dir: &Path, name: &str) -> Vec<u8> {
    let dng = dir.join("tmp.dng");
    fs::write(&dng, vault_raw::testdng::make(512, 384)).unwrap();
    let r = vault_raw::Render { quality: 100, chroma: vault_raw::Chroma::C444, ..Default::default() };
    let (_, j) = vault_raw::render_jpg(&dng, vault_raw::ColourMode::Preserve, 3, &r).unwrap();
    let _ = name;
    j
}

fn shrink_cfg(t: &Path, target: &Path) -> Config {
    use crate::config::{ArchiveMode, Rule};
    let mut c = cfg(t);
    c.archive.rules = vec![Rule { path: target.to_path_buf(), mode: ArchiveMode::Shrink }];
    c.archive.render.quality = 80;
    c.archive.render.chroma = vault_raw::Chroma::C420;
    c
}

fn write_test_zip(path: &Path, jpg: &[u8]) {
    let mut z = zip::ZipWriter::new(fs::File::create(path).unwrap());
    let st = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    let dt = zip::DateTime::from_date_and_time(2016, 5, 4, 3, 2, 0).unwrap();
    z.add_directory("P/sub", st).unwrap();
    z.start_file("P/a.jpg", st.last_modified_time(dt)).unwrap();
    std::io::Write::write_all(&mut z, jpg).unwrap();
    z.start_file("P/sub/b.JPEG", st).unwrap();
    std::io::Write::write_all(&mut z, jpg).unwrap();
    z.start_file("P/notes.txt", zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
    std::io::Write::write_all(&mut z, b"hello hello hello hello hello hello").unwrap();
    z.finish().unwrap();
}

#[test]
fn shrink_rebuilds_zip_smaller_keeping_names_times_metadata() {
    let t = tempfile::tempdir().unwrap();
    let jpg = big_jpg(t.path(), "x");
    let zp = t.path().join("local/2016/P.zip");
    fs::create_dir_all(zp.parent().unwrap()).unwrap();
    write_test_zip(&zp, &jpg);
    let before = fs::metadata(&zp).unwrap().len();
    let c = shrink_cfg(t.path(), &t.path().join("local/2016"));
    crate::shrink::run(&c, &Ctx::new(true, false, false)).unwrap();
    assert_eq!(fs::metadata(&zp).unwrap().len(), before, "dry run changes nothing");
    crate::shrink::run(&c, &Ctx::new(false, false, false)).unwrap();
    let after = fs::metadata(&zp).unwrap().len();
    assert!(after < before * 9 / 10, "{after} vs {before}");
    let mut z = zip::ZipArchive::new(fs::File::open(&zp).unwrap()).unwrap();
    let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().unwrap().to_string()).collect();
    assert_eq!(names.len(), 4);
    for n in ["P/sub/", "P/a.jpg", "P/sub/b.JPEG", "P/notes.txt"] {
        assert!(names.iter().any(|x| x == n), "{n} in {names:?}");
    }
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut z.by_name("P/a.jpg").unwrap(), &mut buf).unwrap();
    let info = vault_raw::probe_decode_bytes(&buf).unwrap();
    assert_eq!((info.width, info.height), (512, 384));
    assert!(info.has_icc, "ICC profile kept");
    assert!(buf.len() < jpg.len());
    let dt = z.by_name("P/a.jpg").unwrap().last_modified().unwrap();
    assert_eq!((dt.year(), dt.month(), dt.day()), (2016, 5, 4), "entry time kept");
    let mut txt = String::new();
    std::io::Read::read_to_string(&mut z.by_name("P/notes.txt").unwrap(), &mut txt).unwrap();
    assert!(txt.starts_with("hello"));
    assert!(fs::read_dir(zp.parent().unwrap()).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".vshrink")));
    // a second run finds nothing worth doing and leaves the file byte for byte alone
    let once = fs::read(&zp).unwrap();
    crate::shrink::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert_eq!(fs::read(&zp).unwrap(), once);
}

#[test]
fn shrink_leaves_zip_alone_below_threshold_and_keeps_broken_jpgs() {
    let t = tempfile::tempdir().unwrap();
    let jpg = big_jpg(t.path(), "x");
    let zp = t.path().join("local/2016/P.zip");
    fs::create_dir_all(zp.parent().unwrap()).unwrap();
    write_test_zip(&zp, &jpg);
    let orig = fs::read(&zp).unwrap();
    let mut c = shrink_cfg(t.path(), &zp);
    c.archive.shrink_min_saving_percent = 99.0;
    crate::shrink::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert_eq!(fs::read(&zp).unwrap(), orig);
    // a "JPG" that does not decode is copied through untouched, never dropped
    let zp2 = t.path().join("local/2016/Q.zip");
    let mut z = zip::ZipWriter::new(fs::File::create(&zp2).unwrap());
    let st = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("Q/good.jpg", st).unwrap();
    std::io::Write::write_all(&mut z, &jpg).unwrap();
    z.start_file("Q/bad.jpg", st).unwrap();
    std::io::Write::write_all(&mut z, b"not a jpeg at all").unwrap();
    z.finish().unwrap();
    let c = shrink_cfg(t.path(), &zp2);
    crate::shrink::run(&c, &Ctx::new(false, false, false)).unwrap();
    let mut z = zip::ZipArchive::new(fs::File::open(&zp2).unwrap()).unwrap();
    let mut buf = Vec::new();
    std::io::Read::read_to_end(&mut z.by_name("Q/bad.jpg").unwrap(), &mut buf).unwrap();
    assert_eq!(buf, b"not a jpeg at all");
}

#[test]
fn shrink_folder_in_place_keeps_mtime() {
    let t = tempfile::tempdir().unwrap();
    let jpg = big_jpg(t.path(), "x");
    let d = t.path().join("local/2016/Dir");
    put(&d.join("a.jpg"), &jpg, 3 * YEAR);
    put(&d.join("s/b.jpg"), &jpg, 3 * YEAR);
    let mtime = fs::metadata(d.join("a.jpg")).unwrap().modified().unwrap();
    let c = shrink_cfg(t.path(), &d);
    crate::shrink::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert!(fs::metadata(d.join("a.jpg")).unwrap().len() < jpg.len() as u64 * 9 / 10);
    assert_eq!(fs::metadata(d.join("a.jpg")).unwrap().modified().unwrap(), mtime);
    assert!(vault_raw::probe_decode(&d.join("s/b.jpg")).is_ok());
}

#[test]
fn archive_ignores_shrink_folders_and_shrink_ignores_others() {
    let t = tempfile::tempdir().unwrap();
    let jpg = big_jpg(t.path(), "x");
    let d = t.path().join("local/2016/Dir");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("a.arw"), vault_raw::testdng::make(64, 48)).unwrap();
    put(&d.join("b.jpg"), &jpg, YEAR);
    let c = shrink_cfg(t.path(), &d);
    crate::archive::run(&c, &Ctx::new(false, false, false), None).unwrap();
    assert!(d.join("a.arw").exists(), "archive leaves shrink-mode folders alone");
    let other = t.path().join("local/2016/Other/x.jpg");
    put(&other, &jpg, YEAR);
    crate::shrink::run(&c, &Ctx::new(false, false, false)).unwrap();
    assert_eq!(fs::metadata(&other).unwrap().len(), jpg.len() as u64, "unselected folder untouched");
}

#[test]
fn preview_picks_random_arw_then_jpg_and_stays_inside_backup() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    let root = t.path().join("local");
    // nothing to test yet
    assert!(crate::preview::run(&c, &root).is_err());
    // a zip with a JPG inside
    let jpg = big_jpg(t.path(), "x");
    let zp = root.join("2016/P.zip");
    fs::create_dir_all(zp.parent().unwrap()).unwrap();
    write_test_zip(&zp, &jpg);
    let v = crate::preview::run(&c, &root).unwrap();
    assert_eq!(v["kind"], "jpg");
    assert!(v["file"].as_str().unwrap().contains("P.zip >"));
    let rows = v["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 24);
    let size = |q: u64, ch: &str| rows.iter().find(|r| r["quality"] == q && r["chroma"] == ch).unwrap()["size"].as_u64().unwrap();
    assert!(size(80, "420") < size(100, "444") && size(60, "420") < size(80, "420"));
    // an ARW takes priority and is measured against its own size
    let arw = root.join("2016/Raw/a.arw");
    fs::create_dir_all(arw.parent().unwrap()).unwrap();
    fs::write(&arw, vault_raw::testdng::make(256, 192)).unwrap();
    let v = crate::preview::run(&c, &root).unwrap();
    assert_eq!(v["kind"], "arw");
    assert_eq!(v["source_size"].as_u64().unwrap(), fs::metadata(&arw).unwrap().len());
    assert_eq!((v["width"].as_u64().unwrap(), v["height"].as_u64().unwrap()), (256, 192));
    // outside the local backup folder is refused
    fs::create_dir_all(t.path().join("outside")).unwrap();
    assert!(crate::preview::run(&c, &t.path().join("outside")).is_err());
    assert!(crate::preview::run(&c, &root.join("../outside")).is_err());
}

fn noisy_px(w: usize, h: usize) -> Vec<u8> {
    let mut px = vec![0u8; w * h * 3];
    let mut s = 7u32;
    for y in 0..h {
        for x in 0..w {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let n = (s >> 27) as u8;
            let i = (y * w + x) * 3;
            px[i] = ((x * 255 / w) as u8).saturating_add(n);
            px[i + 1] = ((y * 255 / h) as u8).saturating_add(n);
            px[i + 2] = (((x + y) / 5 % 256) as u8).saturating_add(n);
        }
    }
    px
}

#[test]
fn old_encoder_settings_are_detected_as_broken() {
    let px = noisy_px(640, 480);
    let bad = vault_raw::test_buggy_jpg(&px, 640, 480);
    assert!(vault_raw::vertically_subsampled_baseline(&bad));
    assert!(vault_raw::decoders_disagree(&bad).unwrap(), "zune-jpeg should misread the old 4:2:0 output");
}

#[test]
fn new_420_output_decodes_the_same_in_both_decoders() {
    let t = tempfile::tempdir().unwrap();
    let dng = t.path().join("t.dng");
    fs::write(&dng, vault_raw::testdng::make(640, 480)).unwrap();
    for c in [vault_raw::Chroma::C420, vault_raw::Chroma::C422, vault_raw::Chroma::C444] {
        let r = vault_raw::Render { quality: 85, chroma: c, ..Default::default() };
        let (_, j) = vault_raw::render_jpg(&dng, vault_raw::ColourMode::Preserve, 3, &r).unwrap();
        assert!(!vault_raw::decoders_disagree(&j).unwrap(), "{c:?} ARW conversion");
    }
    let px = noisy_px(640, 480);
    let bad = vault_raw::test_buggy_jpg(&px, 640, 480);
    let (re, _, _) = vault_raw::recompress::recompress(&bad, 80, vault_raw::Chroma::C420).unwrap();
    assert!(!vault_raw::decoders_disagree(&re).unwrap(), "shrink output");
}

#[test]
fn repair_fixes_broken_jpgs_losslessly_and_leaves_good_ones() {
    let t = tempfile::tempdir().unwrap();
    let c = cfg(t.path());
    let px = noisy_px(640, 480);
    let bad = vault_raw::test_buggy_jpg(&px, 640, 480);
    let good = big_jpg(t.path(), "x");
    let loose = t.path().join("local/2016/P/bad.jpg");
    put(&loose, &bad, 2 * YEAR);
    put(&t.path().join("local/2016/P/good.jpg"), &good, 2 * YEAR);
    let mtime = fs::metadata(&loose).unwrap().modified().unwrap();
    let zp = t.path().join("local/2016/Z.zip");
    let mut z = zip::ZipWriter::new(fs::File::create(&zp).unwrap());
    let st = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    z.start_file("Z/bad.jpg", st).unwrap();
    std::io::Write::write_all(&mut z, &bad).unwrap();
    z.start_file("Z/good.jpg", st).unwrap();
    std::io::Write::write_all(&mut z, &good).unwrap();
    z.finish().unwrap();

    let ctx = Ctx::new(true, false, false);
    crate::repair::run(&c, &ctx).unwrap();
    assert!(ctx.lines().iter().any(|l| l.contains("2 broken JPG(s) found")), "{:?}", ctx.lines());
    assert_eq!(fs::read(&loose).unwrap(), bad, "dry run changes nothing");

    crate::repair::run(&c, &Ctx::new(false, false, false)).unwrap();
    let fixed = fs::read(&loose).unwrap();
    assert_ne!(fixed, bad);
    assert!(!vault_raw::decoders_disagree(&fixed).unwrap());
    assert_eq!(vault_raw::reference_pixels(&fixed).unwrap(), vault_raw::reference_pixels(&bad).unwrap(), "lossless");
    assert_eq!(fs::metadata(&loose).unwrap().modified().unwrap(), mtime);
    assert_eq!(fs::read(t.path().join("local/2016/P/good.jpg")).unwrap(), good, "good file untouched");
    let mut z = zip::ZipArchive::new(fs::File::open(&zp).unwrap()).unwrap();
    let mut b = Vec::new();
    std::io::Read::read_to_end(&mut z.by_name("Z/bad.jpg").unwrap(), &mut b).unwrap();
    assert!(!vault_raw::decoders_disagree(&b).unwrap());
    let mut g = Vec::new();
    std::io::Read::read_to_end(&mut z.by_name("Z/good.jpg").unwrap(), &mut g).unwrap();
    assert_eq!(g, good);
    // nothing left to do on a second run
    let ctx = Ctx::new(false, false, false);
    crate::repair::run(&c, &ctx).unwrap();
    assert!(ctx.lines().iter().any(|l| l.contains("0 broken JPG(s) found")));
}
