//! Package send inputs the way secure-send-web does.
//!
//! A single regular file is sent as-is. Anything else (a folder, or multiple
//! inputs) is bundled into one standard ZIP archive that travels the normal
//! single-file protocol, mirroring the web app's `compressFilesToZip`:
//! folder entries are keyed `<folderName>/sub/file.ext` (forward slashes),
//! loose files are keyed by bare basename, and the archive is named
//! `<folder>_<yyyymmddhhmmss>.zip` when exactly one folder is selected, else
//! `files_<yyyymmddhhmmss>.zip` — the local-time stamp (the web app's
//! `archiveTimestamp`) keeps repeated sends of the same selection from all
//! arriving under one file name.
//! Only file entries are written: empty directories are omitted and no
//! explicit directory entries are added. File symlinks are followed;
//! directory symlinks inside a walked folder are skipped (with a warning) so
//! the walk always terminates.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zip::CompressionMethod;
use zip::write::SimpleFileOptions;

use crate::crypto::chunk::MAX_MESSAGE_SIZE;

/// What the transfer layer sends: either the original file untouched, or a
/// temporary ZIP bundling the selection. The temp file lives as long as this
/// struct, so hold it until the transfer completes.
#[derive(Debug)]
pub struct SendSource {
    pub path: PathBuf,
    pub file_name: String,
    pub file_size: u64,
    pub mime_type: &'static str,
    _temp: Option<tempfile::NamedTempFile>,
}

/// Prepare the send source for a selection of files and/or folders.
///
/// Blocking (walks directories and compresses); wrap in `spawn_blocking` from
/// async contexts.
pub fn prepare_send_source(inputs: &[PathBuf]) -> Result<SendSource> {
    prepare_send_source_with_cap(inputs, MAX_MESSAGE_SIZE)
}

fn prepare_send_source_with_cap(inputs: &[PathBuf], cap: u64) -> Result<SendSource> {
    if inputs.is_empty() {
        bail!("Nothing to send");
    }

    if let [single] = inputs {
        let metadata = fs::metadata(single)
            .with_context(|| format!("Cannot read {}", single.display()))?;
        if metadata.is_file() {
            return single_file_source(single, metadata.len(), cap);
        }
    }

    let (entries, archive_name) = plan_entries(inputs)?;
    let temp = write_zip(&entries)?;
    let file_size = temp
        .as_file()
        .metadata()
        .context("Cannot stat temporary archive")?
        .len();
    check_size_cap(file_size, cap)?;

    Ok(SendSource {
        path: temp.path().to_path_buf(),
        file_name: archive_name,
        file_size,
        mime_type: "application/zip",
        _temp: Some(temp),
    })
}

fn single_file_source(path: &Path, file_size: u64, cap: u64) -> Result<SendSource> {
    if file_size == 0 {
        bail!("File is empty: {}", path.display());
    }
    check_size_cap(file_size, cap)?;

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file")
        .trim()
        .to_string();
    if file_name.is_empty() {
        bail!("Missing file name");
    }

    Ok(SendSource {
        path: path.to_path_buf(),
        file_name,
        file_size,
        mime_type: "application/octet-stream",
        _temp: None,
    })
}

fn check_size_cap(file_size: u64, cap: u64) -> Result<()> {
    if file_size > cap {
        bail!(
            "File is {}, which exceeds the {} limit",
            crate::util::format_bytes(file_size),
            crate::util::format_bytes(cap)
        );
    }
    Ok(())
}

/// The wire file name a selection will travel under — minus the local-time
/// stamp appended at packaging time — without walking any directory tree
/// (cheap enough to call on every TUI redraw).
pub fn send_display_name(inputs: &[PathBuf]) -> String {
    match inputs {
        [single] if single.is_dir() => match input_name(single) {
            Ok(name) => format!("{name}.zip"),
            Err(_) => "files.zip".to_string(),
        },
        [single] => input_name(single).unwrap_or_else(|_| "file".to_string()),
        _ => "files.zip".to_string(),
    }
}

/// Local-time `yyyymmddhhmmss` stamp appended to archive names. Mirrors
/// secure-send-web's `archiveTimestamp`.
fn archive_timestamp() -> String {
    chrono::Local::now().format("%Y%m%d%H%M%S").to_string()
}

/// Plan the ZIP: sorted `(entry_key, source_path)` pairs plus the archive's
/// file name. Errors on duplicate keys, non-UTF-8 names, and empty results.
fn plan_entries(inputs: &[PathBuf]) -> Result<(Vec<(String, PathBuf)>, String)> {
    let mut entries: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut folder_names = Vec::new();
    let mut file_count = 0usize;

    for input in inputs {
        let metadata = fs::metadata(input)
            .with_context(|| format!("Cannot read {}", input.display()))?;
        if metadata.is_dir() {
            let name = input_name(input)?;
            collect_dir(input, &name, &mut entries)?;
            folder_names.push(name);
        } else if metadata.is_file() {
            let name = input_name(input)?;
            insert_entry(&mut entries, name, input.clone())?;
            file_count += 1;
        } else {
            bail!("Not a regular file or directory: {}", input.display());
        }
    }

    if entries.is_empty() {
        bail!("Nothing to send: the selection contains no files");
    }

    let stamp = archive_timestamp();
    let archive_name = match (folder_names.as_slice(), file_count) {
        ([folder], 0) => format!("{folder}_{stamp}.zip"),
        _ => format!("files_{stamp}.zip"),
    };

    Ok((entries.into_iter().collect(), archive_name))
}

/// Recursively add `dir`'s files under the `prefix/` key namespace.
fn collect_dir(dir: &Path, prefix: &str, entries: &mut BTreeMap<String, PathBuf>) -> Result<()> {
    let mut children: Vec<_> = fs::read_dir(dir)
        .with_context(|| format!("Cannot read directory {}", dir.display()))?
        .collect::<std::io::Result<_>>()
        .with_context(|| format!("Cannot read directory {}", dir.display()))?;
    children.sort_by_key(|e| e.file_name());

    for child in children {
        let path = child.path();
        let name = input_name(&path)?;
        let key = format!("{prefix}/{name}");
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("Cannot read {}", path.display()))?;

        if metadata.is_dir() {
            collect_dir(&path, &key, entries)?;
        } else if metadata.is_file() {
            insert_entry(entries, key, path)?;
        } else if metadata.is_symlink() {
            // Follow file symlinks; skip directory symlinks so the walk
            // cannot cycle.
            match fs::metadata(&path) {
                Ok(target) if target.is_file() => insert_entry(entries, key, path)?,
                Ok(target) if target.is_dir() => {
                    log::warn!("Skipping directory symlink: {}", path.display());
                }
                _ => log::warn!("Skipping unreadable symlink: {}", path.display()),
            }
        }
    }
    Ok(())
}

fn insert_entry(entries: &mut BTreeMap<String, PathBuf>, key: String, path: PathBuf) -> Result<()> {
    if entries.contains_key(&key) {
        bail!("Duplicate archive entry \"{key}\": rename one of the inputs");
    }
    entries.insert(key, path);
    Ok(())
}

/// UTF-8 basename of a path, resolving `.`-style paths through canonicalize.
fn input_name(path: &Path) -> Result<String> {
    let name = match path.file_name() {
        Some(name) => name.to_owned(),
        None => fs::canonicalize(path)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_owned()))
            .with_context(|| format!("Cannot determine a name for {}", path.display()))?,
    };
    name.into_string()
        .map_err(|_| anyhow::anyhow!("File name is not valid UTF-8: {}", path.display()))
}

fn write_zip(entries: &[(String, PathBuf)]) -> Result<tempfile::NamedTempFile> {
    let mut temp = tempfile::Builder::new()
        .prefix("secure-send-")
        .suffix(".zip")
        .tempfile()
        .context("Cannot create temporary archive")?;

    let options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let mut zip = zip::ZipWriter::new(temp.as_file_mut());
    for (key, path) in entries {
        zip.start_file(key, options)
            .with_context(|| format!("Cannot add \"{key}\" to archive"))?;
        let mut file =
            fs::File::open(path).with_context(|| format!("Cannot open {}", path.display()))?;
        std::io::copy(&mut file, &mut zip)
            .with_context(|| format!("Cannot compress {}", path.display()))?;
    }
    zip.finish().context("Cannot finalize archive")?;

    Ok(temp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, content).unwrap();
        path
    }

    fn keys(entries: &[(String, PathBuf)]) -> Vec<&str> {
        entries.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// Assert `name` is `<base>_<yyyymmddhhmmss>.zip`.
    fn assert_stamped_name(name: &str, base: &str) {
        let stamp = name
            .strip_prefix(&format!("{base}_"))
            .and_then(|rest| rest.strip_suffix(".zip"))
            .unwrap_or_else(|| panic!("unexpected archive name: {name}"));
        assert_eq!(stamp.len(), 14, "unexpected timestamp in {name}");
        assert!(stamp.bytes().all(|b| b.is_ascii_digit()), "unexpected timestamp in {name}");
    }

    #[test]
    fn single_file_is_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "report.pdf", "data");
        let source = prepare_send_source(std::slice::from_ref(&file)).unwrap();
        assert_eq!(source.path, file);
        assert_eq!(source.file_name, "report.pdf");
        assert_eq!(source.file_size, 4);
        assert_eq!(source.mime_type, "application/octet-stream");
        assert!(source._temp.is_none());
    }

    #[test]
    fn empty_single_file_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "empty.bin", "");
        let err = prepare_send_source(&[file]).unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn one_folder_prefixes_keys_and_names_archive() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "photos/a.jpg", "a");
        write(dir.path(), "photos/albums/b.jpg", "b");
        let (entries, name) = plan_entries(&[dir.path().join("photos")]).unwrap();
        assert_stamped_name(&name, "photos");
        assert_eq!(keys(&entries), ["photos/a.jpg", "photos/albums/b.jpg"]);
    }

    #[test]
    fn loose_files_use_bare_keys() {
        let dir = tempfile::tempdir().unwrap();
        let a = write(dir.path(), "b.txt", "b");
        let b = write(dir.path(), "a.txt", "a");
        let (entries, name) = plan_entries(&[a, b]).unwrap();
        assert_stamped_name(&name, "files");
        assert_eq!(keys(&entries), ["a.txt", "b.txt"]); // sorted by key
    }

    #[test]
    fn mixed_selection_is_files_zip() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "docs/x.md", "x");
        let loose = write(dir.path(), "notes.txt", "n");
        let (entries, name) = plan_entries(&[dir.path().join("docs"), loose]).unwrap();
        assert_stamped_name(&name, "files");
        assert_eq!(keys(&entries), ["docs/x.md", "notes.txt"]);
    }

    #[test]
    fn empty_directories_are_omitted() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "proj/src/main.rs", "fn main() {}");
        fs::create_dir_all(dir.path().join("proj/empty/nested")).unwrap();
        let (entries, _) = plan_entries(&[dir.path().join("proj")]).unwrap();
        assert_eq!(keys(&entries), ["proj/src/main.rs"]);
    }

    #[test]
    fn folder_with_no_files_rejected() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("hollow/inside")).unwrap();
        let err = plan_entries(&[dir.path().join("hollow")]).unwrap_err();
        assert!(err.to_string().contains("no files"));
    }

    #[test]
    fn duplicate_keys_error() {
        let dir_a = tempfile::tempdir().unwrap();
        let dir_b = tempfile::tempdir().unwrap();
        let a = write(dir_a.path(), "same.txt", "1");
        let b = write(dir_b.path(), "same.txt", "2");
        let err = plan_entries(&[a, b]).unwrap_err();
        assert!(err.to_string().contains("same.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_followed_for_files_skipped_for_dirs() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "root/real.txt", "real");
        write(dir.path(), "outside.txt", "out");
        fs::create_dir_all(dir.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("outside.txt"),
            dir.path().join("root/link.txt"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("elsewhere"),
            dir.path().join("root/loop"),
        )
        .unwrap();
        let (entries, _) = plan_entries(&[dir.path().join("root")]).unwrap();
        assert_eq!(keys(&entries), ["root/link.txt", "root/real.txt"]);
    }

    #[test]
    fn zip_round_trips_with_forward_slash_deflated_entries() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bundle/a.txt", "alpha");
        write(dir.path(), "bundle/sub/b.txt", "beta");
        let source = prepare_send_source(&[dir.path().join("bundle")]).unwrap();
        assert_stamped_name(&source.file_name, "bundle");
        assert_eq!(source.mime_type, "application/zip");
        assert!(source.file_size > 0);

        let mut archive = zip::ZipArchive::new(fs::File::open(&source.path).unwrap()).unwrap();
        assert_eq!(archive.len(), 2);
        let mut seen = Vec::new();
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).unwrap();
            assert!(!entry.is_dir());
            assert_eq!(entry.compression(), CompressionMethod::Deflated);
            let mut content = String::new();
            entry.read_to_string(&mut content).unwrap();
            seen.push((entry.name().to_string(), content));
        }
        seen.sort();
        assert_eq!(
            seen,
            [
                ("bundle/a.txt".to_string(), "alpha".to_string()),
                ("bundle/sub/b.txt".to_string(), "beta".to_string()),
            ]
        );
    }

    #[test]
    fn zip_over_cap_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "big/a.bin", &"x".repeat(4096));
        write(dir.path(), "big/b.bin", "tiny");
        let err = prepare_send_source_with_cap(&[dir.path().join("big")], 64).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn single_file_over_cap_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "big.bin", &"x".repeat(4096));
        let err = prepare_send_source_with_cap(&[file], 64).unwrap_err();
        assert!(err.to_string().contains("exceeds"));
    }

    #[test]
    fn display_name_matches_naming_rules() {
        let dir = tempfile::tempdir().unwrap();
        let file = write(dir.path(), "solo.txt", "s");
        write(dir.path(), "photos/p.jpg", "p");
        assert_eq!(send_display_name(std::slice::from_ref(&file)), "solo.txt");
        assert_eq!(send_display_name(&[dir.path().join("photos")]), "photos.zip");
        assert_eq!(
            send_display_name(&[file, dir.path().join("photos")]),
            "files.zip"
        );
    }
}
