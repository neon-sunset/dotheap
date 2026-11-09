use crate::{Error, Result};
use bytemuck::Pod;
use memmap2::{Mmap, MmapMut, MmapOptions};
use std::{
    fs,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    path::{Path, PathBuf},
};

mod codec;
mod shuffle;

enum Storage {
    Read(Mmap),
    Write(MmapMut),
}

pub struct Column<T: Pod> {
    storage: Storage,
    count: usize,
    path: Option<PathBuf>,
    delta: bool,
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
        if let Storage::Write(m) = &self.storage
            && let Some(path) = &self.path
        {
            codec::write(
                path,
                &m[..self.count * size_of::<T>()],
                size_of::<T>(),
                self.delta,
            )?;
        }
        Ok(())
    }

    pub fn freeze(self) -> Result<Self> {
        self.flush()?;
        let storage = match self.storage {
            Storage::Write(m) => Storage::Read(m.make_read_only()?),
            s => s,
        };
        Ok(Self { storage, ..self })
    }

    pub fn with_delta_encoding(mut self) -> Self {
        self.delta = true;
        self
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
        let map = MmapOptions::new().len(bytes.max(1)).map_anon()?;
        Ok(Column {
            storage: Storage::Write(map),
            count,
            path: self
                .directory
                .as_ref()
                .and_then(|dir| name.map(|name| dir.join(format!("{name}.col")))),
            delta: false,
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
        let (map, count) = codec::read(&path, size_of::<T>())?;
        Ok(Column {
            storage: Storage::Read(map.make_read_only()?),
            count,
            path: None,
            delta: false,
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
