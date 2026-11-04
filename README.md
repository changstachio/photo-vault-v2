# photo-vault-v2

Linux photo backup tool in Rust. One binary, `vault`, with four jobs, a scheduler and a small web UI.

| Job | Command | What it does |
|---|---|---|
| 1 Sync | `vault sync` | Copies the listed folders (not everything) to the network share. Skips unchanged files (size + mtime), never deletes on the share. |
| 2 Ingest | `vault ingest` | For each project folder in the SSD ingest cache: confirm it in the local backup byte for byte (BLAKE3), copying it to `Sony Backup/<year>/<project>/` if missing, then delete it from the cache. |
| 3 Archive | `vault archive` | Oldest first: ARW to JPG (LibRaw), delete the ARW only after the JPG is read back and decodes, then zip (stored) folders that contain only JPGs and delete the folder after the zip is verified. |
| 4 Dates | `vault dates [--apply]` | Walks the network and local backup roots, finds each project's real year, and lists (or with `--apply` moves) projects in the wrong `<year>` folder. |

`--dry-run` (sync, ingest, archive) changes nothing.

## Rules the code follows

| Rule | Where |
|---|---|
| Year = oldest of EXIF capture time, file creation time, modified time; anything before 2010 or in the future is ignored | `dates.rs` |
| Ingest deletes only files it has just re-hashed at the destination, never overwrites a differing file, skips projects touched within `min_idle_minutes` | `ingest.rs` |
| ARW is deleted only after the JPG's hash matches the rendered bytes and it decodes | `archive.rs` |
| ARW with a same-name JPG follows `archive.existing_jpg` (below) | `archive.rs` |
| Folder is deleted only after the zip is re-read and every file's hash matches | `archive.rs` |
| A project that exists only as a zip in the local backup is left alone by ingest | `ingest.rs` |
| Sync refuses to run if `sync.require_mount` is not a mount point | `sync.rs` |
| Copies go to a temp file, then rename; source changes mid-copy fail the file | `util.rs` |

## ARW with a same-name JPG

Set `archive.existing_jpg` in the config, pass `vault archive --existing-jpg <mode>`, or pick it in the web UI.

| Mode | Effect |
|---|---|
| `rename` (default) | Keep the existing JPG, save the conversion as `name_arw.jpg`, delete the ARW |
| `use_existing` | Check the existing JPG decodes, then delete the ARW with no conversion. A JPG that does not decode keeps the ARW and reports an error |
| `overwrite` | Replace the existing JPG with a fresh conversion, then delete the ARW |

## Install

```bash
cargo build --release
sudo ./install/install.sh
```

```bash
sudo $EDITOR /etc/photovault/config.toml
```

```bash
sudo systemctl enable --now photovault
```

The daemon starts at boot, runs the `[[schedule]]` entries (local time, `HH:MM`, optional `days`), can run tasks once at start (`daemon.run_on_start`), and serves the UI on `web.bind` (default `127.0.0.1:8787`). One task runs at a time. Mount the NAS yourself (`/etc/fstab`, CIFS or NFS); the tool stores no credentials. Needs a C++ compiler to build (LibRaw is vendored).

**Choosing what gets backed up:** the web UI's *Backup selection* box is a folder browser with checkboxes. Tick folders and/or individual files, then *Save selection*; the scheduled sync copies exactly those (files go under their folder's name on the share). The picker only shows `web.browse_roots` (default: the ingest and local backup folders) and refuses anything outside them, including `..` and symlink escapes. Saving rewrites only the `[[sync.source]]` entries and keeps the rest of the config file.

If the picker is empty it lists why: usually the service user (`photovault`) cannot read the folder, e.g. a drive mounted under `/run/media/<you>/` or a home folder. Run the service as your own user (`User=` in `/etc/systemd/system/photovault.service`) or mount the drive somewhere readable.

All configured paths must be absolute mount paths. A URL such as `smb://host/share` is not a path: mount the share (`/etc/fstab`) and use the mount point. `sync.dest` may not be `/`.

Settings can be edited in the web UI (Settings box at the bottom): the text is checked, written to the config file (comments kept) and used from the next task on. `web.bind` and `web.enabled` need a service restart. The service user must own the config file and its folder (`install.sh` does this).

The web UI has no login, so anyone who can reach it can change paths and run the deleting jobs. Keep it on localhost, use an SSH tunnel, or put it behind an authenticating reverse proxy. POST requests need an `X-Vault` header.

## Performance

Sync copies several files in parallel (`sync.threads`); each file uses a reader thread (read + hash) and a writer thread with 8 MiB buffers, so reading and the network write overlap. For 2.5 GbE (about 280 MB/s) use an SMB/NFS mount with large rsize/wsize, e.g. CIFS `vers=3.1.1,rsize=4194304,wsize=4194304`, and raise `threads` for many small files.
Measured here (sandbox disk, not a network): 3.77 GB in 2004 files in 39 s with `vault sync`, versus 32 s for `cp -r` on the same disk. A 2.5 GbE link was not available, so the link-saturation target is untested.

## Limits

- EXIF is read from the first 4 MiB of each file; if it is further in, file times are used.
- Zipped projects are dated from file and zip entry times, not EXIF.
- Symlinks are skipped.
- Stale `.*.vtmp` files can remain after a crash and are safe to delete.

## Sources

- LibRaw: https://www.libraw.org/docs/API-datastructs.html
- BLAKE3: https://github.com/BLAKE3-team/BLAKE3
- systemd.service: https://www.freedesktop.org/software/systemd/man/latest/systemd.service.html
- mount.cifs options: https://manpages.debian.org/mount.cifs
