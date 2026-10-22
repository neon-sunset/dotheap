use crate::{
    Error, Result, column::create_directory, compressed::identity, graph::AnalysisOptions,
};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

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
    pub directory: PathBuf,
    pub name: String,
    pub dump_path: Option<PathBuf>,
    pub bytes: u64,
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
pub fn entries(measure: bool) -> Vec<CacheEntry> {
    let mut result = Vec::new();
    let Ok(entries) = fs::read_dir(root()) else {
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
        let dump_path = fs::read_to_string(&recorded).ok().map(PathBuf::from);
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
            directory: path,
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
pub fn remove(entry: &CacheEntry) -> Result<()> {
    let canonical = entry.directory.canonicalize()?;
    let expected = root().canonicalize()?;
    if canonical.parent() != Some(expected.as_path())
        || !fs::symlink_metadata(&entry.directory)?.is_dir()
    {
        return Err(Error::cache("not an analysis cache folder"));
    }
    let _guard = CacheGuard::lock(&entry.directory)?;
    fs::remove_dir_all(&entry.directory)?;
    Ok(())
}
