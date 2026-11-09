use crate::{
    Error, Result, column::create_directory, compressed::identity, graph::AnalysisOptions,
};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::BufWriter,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

pub(crate) fn read_result<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let bytes = zstd::stream::decode_all(File::open(path)?)?;
    Ok(serde_json::from_slice(&bytes)?)
}

pub(crate) fn write_result<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| Error::cache("result has no parent folder"))?;
    create_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    let mut encoder = zstd::stream::write::Encoder::new(file.as_file_mut(), 1)?;
    encoder.include_checksum(true)?;
    let mut writer = BufWriter::with_capacity(64 << 10, encoder);
    serde_json::to_writer(&mut writer, value)?;
    writer.into_inner().map_err(|e| e.into_error())?.finish()?;
    file.as_file().sync_all()?;
    crate::column::persist(file, path)
}

pub(crate) fn discard_columns(directory: &Path) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        if (name == "manifest.json"
            || path.extension().is_some_and(|e| e == "col")
                && !name.to_string_lossy().starts_with("core."))
            && !entry.file_type()?.is_dir()
        {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

pub(crate) fn discard_results(directory: &Path, invalidate: bool) -> Result<()> {
    let root = directory.join("rust-results");
    if !fs::symlink_metadata(&root).is_ok_and(|m| m.is_dir()) {
        return Ok(());
    }
    'directory: for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            if path.extension().is_some_and(|e| e == "invalid")
                && !path.with_extension("").exists()
                && let Err(error) = fs::remove_file(path)
                && error.kind() != std::io::ErrorKind::NotFound
            {
                return Err(error.into());
            }
            continue;
        }
        let marker = path.with_extension("invalid");
        if invalidate {
            crate::compressed::atomic_write(&marker, b"")?;
        } else if !marker.is_file() {
            continue;
        }
        let mut guards = Vec::new();
        for name in ["native-graph", "ownership"] {
            let child = path.join(name);
            if fs::symlink_metadata(&child).is_ok_and(|m| m.is_dir()) {
                match CacheGuard::try_lock(&child) {
                    Ok(Some(guard)) => guards.push(guard),
                    _ => continue 'directory,
                }
            }
        }
        fs::remove_dir_all(path)?;
        fs::remove_file(marker)?;
    }
    Ok(())
}

#[cfg(test)]
mod compressed_results {
    use super::*;

    #[test]
    fn results_in_use_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let result = dir.path().join("rust-results/invalid-format");
        let guard = CacheGuard::lock(&result.join("ownership")).unwrap();
        discard_results(dir.path(), true).unwrap();
        assert!(result.is_dir());
        assert!(result.with_extension("invalid").is_file());
        let current = dir.path().join("rust-results/current");
        fs::create_dir(&current).unwrap();
        drop(guard);
        discard_results(dir.path(), false).unwrap();
        assert!(!result.exists());
        assert!(!result.with_extension("invalid").exists());
        assert!(current.is_dir());
    }

    #[test]
    fn discarded_columns_leave_dump_storage_and_other_files_intact() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = CacheGuard::lock(dir.path()).unwrap();
        let kept = [
            "core.index.col",
            "core.starts.col",
            "core.blocks",
            "core.dump",
            "dump.path",
            "capture.dmp",
            "notes.txt",
        ];
        for name in kept.into_iter().chain(["obsolete.col", "manifest.json"]) {
            fs::write(dir.path().join(name), b"contents").unwrap();
        }
        discard_columns(dir.path()).unwrap();
        for name in kept {
            assert_eq!(fs::read(dir.path().join(name)).unwrap(), b"contents");
        }
        assert!(!dir.path().join("obsolete.col").exists());
        assert!(!dir.path().join("manifest.json").exists());
        assert!(dir.path().join(".lock").exists());
    }

    #[test]
    fn results_are_compressed_and_checksum_failures_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("result.json.zst");
        let value = vec!["repeated result"; 10_000];
        write_result(&path, &value).unwrap();
        assert_eq!(read_result::<Vec<String>>(&path).unwrap(), value);
        let mut bytes = fs::read(&path).unwrap();
        assert!(bytes.len() < 1024);
        *bytes.last_mut().unwrap() ^= 1;
        fs::write(&path, bytes).unwrap();
        assert!(read_result::<Vec<String>>(&path).is_err());
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CacheChoice {
    #[default]
    Automatic,
    Folder(PathBuf),
    Rebuild,
    None,
}
pub fn root() -> PathBuf {
    #[cfg(target_os = "macos")]
    let path = std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|p| p.join("Library/Caches"));
    #[cfg(windows)]
    let path = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", windows)))]
    let path = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .map(|p| p.join(".cache"))
        });
    path.unwrap_or_else(std::env::temp_dir).join("dotheap")
}
pub fn hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}
impl CacheChoice {
    pub fn directory(&self, path: &Path, options: AnalysisOptions) -> Result<Option<PathBuf>> {
        match self {
            Self::None => Ok(None),
            Self::Folder(p) => Ok(Some(p.clone())),
            Self::Automatic | Self::Rebuild => {
                let metadata = fs::metadata(path)?;
                let modified = metadata
                    .modified()?
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let hash = hash(&identity(path)?);
                Ok(Some(root().join(format!(
                    "{name}-{hash:x}-{}-{modified}{}",
                    metadata.len(),
                    if options.native_memory_roots {
                        "-native-roots"
                    } else {
                        ""
                    }
                ))))
            }
        }
    }
}

pub struct CacheGuard {
    _file: File,
}
impl CacheGuard {
    pub fn lock(directory: &Path) -> Result<Self> {
        create_directory(directory)?;
        let file = Self::file(directory)?;
        file.lock()?;
        Ok(Self { _file: file })
    }

    pub fn try_lock(directory: &Path) -> Result<Option<Self>> {
        let file = Self::file(directory)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    fn file(directory: &Path) -> Result<File> {
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(directory.join(".lock"))?)
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CacheEntry {
    pub directories: Vec<PathBuf>,
    pub name: String,
    pub dump_path: Option<PathBuf>,
    pub bytes: u64,
    pub dump_bytes: u64,
    pub captured: bool,
    pub last_opened: f64,
    pub processed: bool,
    pub dump_exists: Option<bool>,
}
pub fn disk_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let mut todo = vec![path.to_owned()];
    while let Some(path) = todo.pop() {
        let Ok(m) = fs::symlink_metadata(&path) else {
            continue;
        };
        if m.is_dir() {
            if let Ok(entries) = fs::read_dir(path) {
                todo.extend(entries.flatten().map(|e| e.path()));
            }
        } else if m.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                total += m.blocks() * 512;
            }
            #[cfg(not(unix))]
            {
                total += m.len();
            }
        }
    }
    total
}
pub fn analysis_entries(measure: bool) -> Vec<CacheEntry> {
    analysis_entries_in(&root(), measure)
}
fn analysis_entries_in(root: &Path, measure: bool) -> Vec<CacheEntry> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !entry.file_type().is_ok_and(|t| t.is_dir())
            || ["live", "symbols"].contains(&entry.file_name().to_string_lossy().as_ref())
        {
            continue;
        }
        let processed = path.join("manifest.json").is_file();
        if !processed && !path.join("core.index.col").is_file() {
            continue;
        }
        let recorded = path.join("dump.path");
        let dump_path = fs::read_to_string(&recorded)
            .ok()
            .map(PathBuf::from)
            .map(|p| p.canonicalize().unwrap_or(p));
        let name = dump_path
            .as_ref()
            .and_then(|p| p.file_name())
            .unwrap_or(&entry.file_name())
            .to_string_lossy()
            .into_owned();
        let opened = fs::metadata(if recorded.is_file() { &recorded } else { &path })
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|t| t.as_secs_f64())
            .unwrap_or(0.0);
        result.push(CacheEntry {
            bytes: if measure { disk_bytes(&path) } else { 0 },
            directories: vec![path],
            dump_bytes: 0,
            captured: false,
            name,
            dump_exists: dump_path.as_ref().map(|p| p.is_file()),
            dump_path,
            last_opened: opened,
            processed,
        });
    }
    result.sort_by(|a, b| b.last_opened.total_cmp(&a.last_opened));
    result
}
pub fn entries(measure: bool) -> Vec<CacheEntry> {
    entries_in(&root(), measure)
}
fn entries_in(root: &Path, measure: bool) -> Vec<CacheEntry> {
    let mut grouped = BTreeMap::<PathBuf, CacheEntry>::new();
    for entry in analysis_entries_in(root, measure) {
        let key = entry
            .dump_path
            .as_ref()
            .unwrap_or(&entry.directories[0])
            .clone();
        if let Some(existing) = grouped.get_mut(&key) {
            existing.bytes += entry.bytes;
            existing.directories.extend(entry.directories);
            existing.last_opened = existing.last_opened.max(entry.last_opened);
            existing.processed |= entry.processed;
        } else {
            grouped.insert(key, entry);
        }
    }
    let live = root.join("live");
    if fs::symlink_metadata(&live).is_ok_and(|m| m.is_dir())
        && let Ok(files) = fs::read_dir(&live)
    {
        for file in files.flatten() {
            if !file.file_type().is_ok_and(|t| t.is_file()) || !is_dump(&file.path()) {
                continue;
            }
            let Ok(path) = file.path().canonicalize() else {
                continue;
            };
            let entry = grouped.entry(path.clone()).or_insert_with(|| CacheEntry {
                directories: Vec::new(),
                name: file.file_name().to_string_lossy().into_owned(),
                dump_path: Some(path.clone()),
                bytes: 0,
                dump_bytes: 0,
                captured: true,
                last_opened: 0.0,
                processed: false,
                dump_exists: Some(true),
            });
            entry.captured = true;
            entry.dump_bytes = if measure { disk_bytes(&path) } else { 0 };
        }
    }
    let mut result: Vec<_> = grouped.into_values().collect();
    for entry in &mut result {
        entry.directories.sort();
    }
    result.sort_by(|a, b| {
        b.last_opened
            .total_cmp(&a.last_opened)
            .then_with(|| a.name.cmp(&b.name))
    });
    result
}

fn is_dump(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|name| {
            !name.starts_with('.') && (name.ends_with(".dmp") || name.ends_with(".dmp.zst"))
        })
}

pub fn remove(entry: &CacheEntry) -> Result<()> {
    remove_in(&root(), entry, false)
}
pub fn remove_captured_dump(entry: &CacheEntry) -> Result<()> {
    remove_in(&root(), entry, true)
}

/// Remove the unlocked analysis caches of earlier dumps at this path, so that a replaced dump does not leave its cache behind.
pub fn remove_replaced(dump: &Path) -> Result<()> {
    remove_replaced_in(&root(), dump)
}
fn remove_replaced_in(root: &Path, dump: &Path) -> Result<()> {
    let root = root.canonicalize()?;
    let current = [false, true]
        .map(|native_memory_roots| {
            CacheChoice::Automatic.directory(
                dump,
                AnalysisOptions {
                    native_memory_roots,
                },
            )
        })
        .into_iter()
        .collect::<Result<Vec<_>>>()?;
    let current: Vec<_> = current
        .iter()
        .flatten()
        .filter_map(|d| d.file_name())
        .collect();
    let dump = dump.canonicalize()?;
    for directory in analysis_entries_in(&root, false)
        .into_iter()
        .filter(|e| e.dump_path.as_ref() == Some(&dump))
        .flat_map(|e| e.directories)
        .filter(|d| d.file_name().is_some_and(|n| !current.contains(&n)))
    {
        if let Ok(_guard) = lock_analysis(&root, &directory) {
            fs::remove_dir_all(&directory)?;
        }
    }
    Ok(())
}

fn lock_analysis(root: &Path, directory: &Path) -> Result<CacheGuard> {
    let canonical = directory.canonicalize()?;
    if canonical.parent() != Some(root)
        || !fs::symlink_metadata(directory)?.is_dir()
        || directory
            .file_name()
            .is_some_and(|n| n == "live" || n == "symbols")
        || !(directory.join("manifest.json").is_file()
            || directory.join("core.index.col").is_file())
    {
        return Err(Error::cache("not an analysis cache folder"));
    }
    CacheGuard::try_lock(directory)?.ok_or_else(|| Error::cache("the analysis cache is in use"))
}

fn remove_in(root: &Path, entry: &CacheEntry, captured: bool) -> Result<()> {
    let root = root.canonicalize()?;
    let mut directories = entry.directories.clone();
    let mut files = Vec::new();
    if captured {
        let dump = entry
            .dump_path
            .as_ref()
            .ok_or_else(|| Error::cache("no captured dump path"))?;
        let live = root.join("live");
        if !fs::symlink_metadata(&live)?.is_dir()
            || !fs::symlink_metadata(dump)?.is_file()
            || dump.canonicalize()?.parent() != Some(live.as_path())
            || !is_dump(dump)
        {
            return Err(Error::cache("not a captured dump in the cache folder"));
        }
        let dump = dump.canonicalize()?;
        directories = analysis_entries_in(&root, false)
            .into_iter()
            .filter(|e| e.dump_path.as_ref() == Some(&dump))
            .flat_map(|e| e.directories)
            .collect();
        let residency = crate::smaps::path_for_dump(&dump);
        let shared = fs::read_dir(&live)?.flatten().any(|f| {
            f.path() != dump
                && f.file_type().is_ok_and(|t| t.is_file())
                && is_dump(&f.path())
                && crate::smaps::path_for_dump(&f.path()) == residency
        });
        if !shared {
            for path in [
                residency.clone(),
                residency.with_extension("gz"),
                residency.with_extension(""),
            ] {
                match fs::symlink_metadata(&path) {
                    Ok(m) if m.is_file() || m.is_symlink() => files.push(path),
                    Ok(_) => return Err(Error::cache("the residency path is not a file")),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        files.push(dump);
    }
    let _guards: Vec<_> = directories
        .iter()
        .map(|d| lock_analysis(&root, d))
        .collect::<Result<_>>()?;
    for path in files {
        fs::remove_file(path)?;
    }
    for directory in directories {
        fs::remove_dir_all(directory)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"test data").unwrap();
    }

    fn analysis(root: &Path, name: &str, dump: &Path) -> PathBuf {
        let path = root.join(name);
        write(&path.join("manifest.json"));
        fs::write(path.join("dump.path"), dump.to_str().unwrap()).unwrap();
        path
    }

    fn entry(root: &Path, dump: &Path) -> CacheEntry {
        let dump = dump.canonicalize().unwrap();
        entries_in(root, true)
            .into_iter()
            .find(|e| e.dump_path.as_ref() == Some(&dump))
            .unwrap()
    }

    #[test]
    fn captures_and_analysis_share_one_entry() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dump = root.join("live/first.dmp.zst");
        let unopened = root.join("live/second.dmp");
        write(&dump);
        write(&unopened);
        write(&root.join("live/first.smaps.zst"));
        write(&root.join("live/.unfinished.dmp"));
        write(&root.join("live/first.dmp.zst.tmp"));
        write(&root.join("symbols/runtime.debug"));
        let first = analysis(root, "first-cache", &dump);
        let second = analysis(root, "first-native-cache", &dump);
        let entries = entries_in(root, true);
        assert_eq!(entries.len(), 2);
        let processed = entry(root, &dump);
        assert_eq!(processed.directories, [first.clone(), second.clone()]);
        assert_eq!(processed.bytes, disk_bytes(&first) + disk_bytes(&second));
        assert_eq!(processed.dump_bytes, disk_bytes(&dump));
        assert!(processed.captured && processed.processed);
        let new = entry(root, &unopened);
        assert!(new.captured && !new.processed && new.directories.is_empty());
        assert_eq!(new.last_opened, 0.0);
        assert!(
            entries_in(root, false)
                .iter()
                .all(|e| e.bytes == 0 && e.dump_bytes == 0)
        );
        assert_eq!(analysis_entries_in(root, false).len(), 2);
    }

    #[test]
    fn clearing_analysis_keeps_captured_dump_and_residency() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let dump = root.join("live/first.dmp.zst");
        let residency = root.join("live/first.smaps.zst");
        write(&dump);
        write(&residency);
        let first = analysis(root, "first-cache", &dump);
        let second = analysis(root, "second-cache", &dump);
        remove_in(root, &entry(root, &dump), false).unwrap();
        assert!(!first.exists() && !second.exists());
        assert!(dump.exists() && residency.exists());
        assert!(entry(root, &dump).directories.is_empty());
    }

    #[test]
    fn replaced_dump_analysis_is_removed_unless_locked() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let dump = temp.path().join("core.zst");
        let other = temp.path().join("other.zst");
        write(&dump);
        write(&other);
        let name = |native_memory_roots| {
            let options = AnalysisOptions {
                native_memory_roots,
            };
            let directory = CacheChoice::Automatic.directory(&dump, options).unwrap();
            directory
                .unwrap()
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned()
        };
        let current = analysis(&root, &name(false), &dump);
        let native_roots = analysis(&root, &name(true), &dump);
        let replaced = analysis(&root, "core.zst-replaced", &dump);
        let busy = analysis(&root, "core.zst-busy", &dump);
        let unrelated = analysis(&root, "other.zst-cache", &other);
        let _guard = CacheGuard::lock(&busy).unwrap();
        remove_replaced_in(&root, &dump).unwrap();
        assert!(!replaced.exists());
        for kept in [current, native_roots, busy, unrelated] {
            assert!(kept.exists());
        }
    }

    #[test]
    fn deleting_capture_removes_its_files_and_all_matching_analysis() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let dump = root.join("live/first.dmp.zst");
        let other = root.join("live/other.dmp.zst");
        let external = temp.path().join("first.dmp.zst");
        for file in [
            &dump,
            &other,
            &external,
            &root.join("symbols/runtime.debug"),
        ] {
            write(file);
        }
        let sidecars = ["first.smaps.zst", "first.smaps.gz", "first.smaps"];
        for name in sidecars {
            write(&root.join("live").join(name));
        }
        let cached = analysis(&root, "first-cache", &dump);
        let outside = analysis(&root, "external-cache", &external);
        let selected = entry(&root, &dump);
        let later = analysis(&root, "later-cache", &dump);
        remove_in(&root, &selected, true).unwrap();
        assert!(!dump.exists() && !cached.exists() && !later.exists());
        for name in sidecars {
            assert!(!root.join("live").join(name).exists());
        }
        assert!(external.exists() && outside.exists() && other.exists());
        assert!(root.join("symbols/runtime.debug").exists());
    }

    #[test]
    fn residency_is_kept_while_another_dump_uses_it() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let raw = root.join("live/first.dmp");
        let compressed = root.join("live/first.dmp.zst");
        let residency = root.join("live/first.smaps.zst");
        for file in [&raw, &compressed, &residency] {
            write(file);
        }
        remove_in(root, &entry(root, &raw), true).unwrap();
        assert!(compressed.exists() && residency.exists());
        remove_in(root, &entry(root, &compressed), true).unwrap();
        assert!(!residency.exists());
    }

    #[test]
    fn capture_deletion_refuses_external_files_and_busy_analysis() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let dump = root.join("live/first.dmp.zst");
        let external = temp.path().join("external.dmp");
        write(&dump);
        write(&external);
        let cached = analysis(&root, "first-cache", &dump);
        let selected = entry(&root, &dump);
        let guard = CacheGuard::lock(&cached).unwrap();
        assert!(remove_in(&root, &selected, true).is_err());
        assert!(dump.exists() && cached.exists());
        drop(guard);
        let mut forged = selected.clone();
        forged.dump_path = Some(external.clone());
        assert!(remove_in(&root, &forged, true).is_err());
        let outside = analysis(temp.path(), "outside-cache", &external);
        forged = selected;
        forged.directories.push(outside.clone());
        assert!(remove_in(&root, &forged, false).is_err());
        assert!(external.exists() && dump.exists() && cached.exists() && outside.exists());
    }

    #[cfg(unix)]
    #[test]
    fn deletion_does_not_follow_symlinks_out_of_the_cache() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cache");
        let dump = root.join("live/first.dmp.zst");
        let external = temp.path().join("external.dmp");
        write(&dump);
        write(&external);
        let alias = root.join("live/alias.dmp");
        symlink(&external, &alias).unwrap();
        assert_eq!(entries_in(&root, true).len(), 1);
        let mut forged = entry(&root, &dump);
        forged.dump_path = Some(alias);
        assert!(remove_in(&root, &forged, true).is_err());
        let outside = analysis(temp.path(), "outside-cache", &external);
        let cache_alias = root.join("cache-alias");
        symlink(&outside, &cache_alias).unwrap();
        forged.directories = vec![cache_alias];
        assert!(remove_in(&root, &forged, false).is_err());
        let residency = root.join("live/first.smaps.zst");
        symlink(&external, &residency).unwrap();
        remove_in(&root, &entry(&root, &dump), true).unwrap();
        assert!(external.exists() && outside.exists());
        assert!(fs::symlink_metadata(&residency).is_err());
    }
}
