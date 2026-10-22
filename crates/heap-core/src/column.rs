use crate::{Error, Result};
use bytemuck::Pod;
use memmap2::{Mmap, MmapMut, MmapOptions};
use std::{
    fs::{self, File},
    marker::PhantomData,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

enum Storage {
    Read(Mmap),
    Write(MmapMut),
}

pub struct Column<T: Pod> {
    storage: Storage,
    count: usize,
    marker: PhantomData<T>,
}

impl<T: Pod> Deref for Column<T> {
    type Target = [T];
    fn deref(&self) -> &[T] {
        let bytes: &[u8] = match &self.storage {
            Storage::Read(m) => m,
            Storage::Write(m) => m,
        };
        bytemuck::cast_slice(&bytes[..self.count * size_of::<T>()])
    }
}

impl<T: Pod> DerefMut for Column<T> {
    fn deref_mut(&mut self) -> &mut [T] {
        match &mut self.storage {
            Storage::Write(m) => bytemuck::cast_slice_mut(&mut m[..self.count * size_of::<T>()]),
            Storage::Read(_) => panic!("cannot modify a read-only column"),
        }
    }
}

impl<T: Pod> Column<T> {
    pub fn flush(&self) -> Result<()> {
        if let Storage::Write(m) = &self.storage {
            m.flush()?;
        }
        Ok(())
    }

    pub fn freeze(self) -> Result<Self> {
        let storage = match self.storage {
            Storage::Write(m) => Storage::Read(m.make_read_only()?),
            s => s,
        };
        Ok(Self { storage, ..self })
    }
}

#[derive(Clone, Default)]
pub struct ColumnStore {
    pub directory: Option<PathBuf>,
}

impl ColumnStore {
    pub fn new(directory: Option<PathBuf>) -> Result<Self> {
        if let Some(path) = &directory {
            create_directory(path)?;
        }
        Ok(Self { directory })
    }

    pub fn make<T: Pod>(&self, count: usize, name: Option<&str>) -> Result<Column<T>> {
        if size_of::<T>() == 0 {
            return Err(Error::cache("zero-sized column element"));
        }
        let bytes = count
            .checked_mul(size_of::<T>())
            .ok_or_else(|| Error::cache("column is too large"))?;
        let map = if let Some(dir) = &self.directory {
            let path = name.map(|name| dir.join(format!("{name}.col")));
            let parent = path.as_ref().and_then(|p| p.parent()).unwrap_or(dir);
            create_directory(parent)?;
            let file = tempfile::NamedTempFile::new_in(parent)?;
            file.as_file().set_len(bytes as u64)?;
            // A new inode keeps any snapshot of the previous column valid.
            let map = if bytes == 0 {
                MmapOptions::new().len(1).map_anon()?
            } else {
                unsafe { MmapOptions::new().len(bytes).map_mut(file.as_file())? }
            };
            if let Some(path) = path {
                persist(file, &path)?;
            }
            map
        } else {
            MmapOptions::new().len(bytes.max(1)).map_anon()?
        };
        Ok(Column {
            storage: Storage::Write(map),
            count,
            marker: PhantomData,
        })
    }

    pub fn copy<T: Pod>(&self, values: &[T], name: Option<&str>) -> Result<Column<T>> {
        let mut column = self.make(values.len(), name)?;
        column.copy_from_slice(values);
        Ok(column)
    }

    pub fn open<T: Pod>(&self, name: &str) -> Result<Column<T>> {
        let path = self
            .directory
            .as_ref()
            .ok_or_else(|| Error::cache("no cache folder"))?
            .join(format!("{name}.col"));
        let file = File::open(&path)?;
        let bytes = usize::try_from(file.metadata()?.len())
            .map_err(|_| Error::cache("column is too large"))?;
        if size_of::<T>() == 0 || bytes % size_of::<T>() != 0 {
            return Err(Error::cache(format!(
                "invalid column size: {}",
                path.display()
            )));
        }
        if bytes == 0 {
            return self.make(0, None);
        }
        // Cache files are immutable while a snapshot uses them.
        let map = unsafe { MmapOptions::new().map(&file)? };
        Ok(Column {
            storage: Storage::Read(map),
            count: bytes / size_of::<T>(),
            marker: PhantomData,
        })
    }
}

pub(crate) fn persist(file: tempfile::NamedTempFile, path: &Path) -> Result<()> {
    let (_file, temporary) = file.keep().map_err(|e| e.error)?;
    let temporary = tempfile::TempPath::try_from_path(temporary)?;
    // std::fs::rename also supports replacing mapped files on Windows.
    fs::rename(&temporary, path).map_err(|e| Error::open(path.display(), e))?;
    Ok(())
}

pub fn create_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    Ok(())
}
