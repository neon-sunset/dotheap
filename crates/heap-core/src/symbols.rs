use crate::{
    Result, analysis::HeapSnapshot, bytes::ReadBytes, core_dump::CoreDump, runtime::elf_build_id,
};
use object::{Object, ObjectSegment, ObjectSymbol, SymbolKind};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Symbol {
    pub address: u64,
    pub size: u64,
    pub name: String,
    pub library: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Library {
    pub start: u64,
    pub end: u64,
    pub name: String,
}
#[derive(Default)]
pub struct LibrarySymbols {
    pub symbols: Vec<Symbol>,
    pub functions: Vec<Symbol>,
    pub libraries: Vec<Library>,
    pub symbol_files: Vec<PathBuf>,
}
impl HeapSnapshot {
    pub fn symbols(&self) -> Result<Arc<LibrarySymbols>> {
        self.memo("symbols", || {
            Ok(LibrarySymbols::new(&self.dump, &self.dll_folders))
        })
    }
}

impl LibrarySymbols {
    pub fn new(dump: &CoreDump, folders: &[PathBuf]) -> Self {
        let mut out = Self::default();
        if !dump.platform.is_linux() {
            return out;
        }
        let mut seen = HashSet::new();
        let mut by_name = None;
        for f in &dump.files {
            if f.file_offset != 0
                || dump.u32(f.start) != Some(0x464c457f)
                || !seen.insert(f.path.clone())
            {
                continue;
            }
            let name = file_name(Path::new(&f.path));
            let mappings: Vec<_> = dump.files.iter().filter(|m| m.path == f.path).collect();
            out.libraries.push(Library {
                start: mappings.iter().map(|m| m.start).min().unwrap_or(f.start),
                end: mappings.iter().map(|m| m.end).max().unwrap_or(f.end),
                name: name.clone(),
            });
            let before = out.symbols.len() + out.functions.len();
            let _ = out.read_dynamic_symbols(dump, f.start, &name);
            let id = elf_build_id(dump, f.start);
            if let Some(id) = &id {
                let index = by_name.get_or_insert_with(|| index(folders));
                let candidates = debug_paths(&name, id, &f.path, index);
                for path in candidates {
                    if let Some(file) = ElfFile::open(&path)
                        && file.build_id().as_ref() == Some(id)
                    {
                        file.symbols(f.start, &name, &mut out);
                        out.symbol_files.push(path);
                        break;
                    }
                }
            }
            if out.symbols.len() + out.functions.len() != before {
                continue;
            }
            let index = by_name.get_or_insert_with(|| index(folders));
            for path in std::iter::once(PathBuf::from(&f.path))
                .chain(index.get(&name).into_iter().flatten().cloned())
            {
                let Some(file) = ElfFile::open(&path) else {
                    continue;
                };
                let matches = if let Some(id) = &id {
                    file.build_id().as_ref() == Some(id)
                } else {
                    dump.bytes(f.start, 4096)
                        .is_some_and(|b| file.data.starts_with(&b))
                };
                if matches {
                    file.symbols(f.start, &name, &mut out);
                    out.symbol_files.push(path);
                    break;
                }
            }
        }
        out.symbols.sort_by_key(|s| s.address);
        out.functions.sort_by_key(|s| s.address);
        out.libraries.sort_by_key(|l| l.start);
        out
    }
    fn read_dynamic_symbols(&mut self, dump: &CoreDump, base: u64, library: &str) -> Option<()> {
        let phoff = dump.u64(base.checked_add(0x20)?)?;
        let headers = dump.u16(base.checked_add(0x38)?)?;
        let phsize = dump.u16(base.checked_add(0x36)?)? as u64;
        if phsize < 56 {
            return None;
        }
        let mut loads = Vec::new();
        let mut dynamic_rva = None;
        for i in 0..headers as u64 {
            let ph = base.checked_add(phoff)?.checked_add(i * phsize)?;
            match dump.u32(ph)? {
                1 => loads.push(dump.u64(ph + 16)?),
                2 => dynamic_rva = dump.u64(ph + 16),
                _ => {}
            }
        }
        let bias = base.checked_sub(loads.into_iter().min()? & !0xfff)?;
        let dynamic = bias.checked_add(dynamic_rva?)?;
        let (mut symtab, mut strtab, mut strsz, mut gnu_hash, mut hash) = (0, 0, 0, 0, 0);
        let address = |v| {
            if v < base {
                bias.checked_add(v)
            } else {
                Some(v)
            }
        };
        for k in 0..512 {
            let p = dynamic.checked_add(k * 16)?;
            let tag = dump.u64(p)?;
            let value = dump.u64(p + 8)?;
            if tag == 0 {
                break;
            }
            match tag {
                4 => hash = address(value)?,
                5 => strtab = address(value)?,
                6 => symtab = address(value)?,
                10 => strsz = value,
                0x6fff_fef5 => gnu_hash = address(value)?,
                _ => {}
            }
        }
        if symtab == 0 || strtab == 0 || strsz == 0 {
            return None;
        }
        let count = if hash != 0 {
            dump.u32(hash.checked_add(4)?)?
        } else {
            gnu_hash_count(dump, gnu_hash)?
        };
        if count == 0 || count >= 2_000_000 {
            return None;
        }
        let names = dump.bytes(strtab, strsz.min(64 << 20) as usize)?;
        for i in 0..count as u64 {
            let s = symtab.checked_add(i * 24)?;
            let Some(data) = dump.bytes(s, 24) else {
                continue;
            };
            let name = data.u32(0)? as usize;
            let kind = data[4] & 15;
            let value = data.u64(8)?;
            let size = data.u64(16)?;
            if ![1, 2, 10].contains(&kind) || value == 0 || size == 0 || name >= names.len() {
                continue;
            }
            let symbol = Symbol {
                address: bias.checked_add(value)?,
                size,
                name: String::from_utf8_lossy(
                    &names[name..name
                        + memchr::memchr(0, &names[name..]).unwrap_or(names.len() - name)],
                )
                .into(),
                library: library.into(),
            };
            self.add(symbol, kind == 1);
        }
        Some(())
    }
    fn add(&mut self, symbol: Symbol, data: bool) {
        if data {
            self.symbols.push(symbol)
        } else {
            self.functions.push(symbol)
        }
    }
    pub fn symbol(&self, address: u64) -> Option<&Symbol> {
        lookup(&self.symbols, address)
    }
    pub fn function(&self, address: u64) -> Option<&Symbol> {
        lookup(&self.functions, address)
    }
    pub fn library(&self, address: u64) -> Option<&str> {
        let i = self
            .libraries
            .partition_point(|l| l.start <= address)
            .checked_sub(1)?;
        let l = &self.libraries[i];
        (address < l.end).then_some(l.name.as_str())
    }
    pub fn class_name(&self, address: u64) -> Option<String> {
        let s = self.symbol(address)?;
        if !s.name.starts_with("_ZTV") {
            return None;
        }
        let text = demangle(&s.name).unwrap_or_else(|| s.name.clone());
        Some(text.strip_prefix("vtable for ").unwrap_or(&text).into())
    }
}

pub(crate) fn debug_paths(
    name: &str,
    id: &str,
    path: &str,
    index: &HashMap<String, Vec<PathBuf>>,
) -> Vec<PathBuf> {
    std::iter::once(symbol_folder().join(id).join(format!("{name}.dbg")))
        .chain(std::iter::once(PathBuf::from(format!("{path}.dbg"))))
        .chain(
            index
                .get(&format!("{name}.dbg"))
                .into_iter()
                .flatten()
                .cloned(),
        )
        .collect()
}

pub(crate) fn cache_identity(dump: &CoreDump) -> String {
    let mut files = std::collections::BTreeSet::new();
    for file in dump.files.iter().filter(|f| f.file_offset == 0) {
        if let Some(id) = elf_build_id(dump, file.start) {
            let name = file_name(Path::new(&file.path));
            for path in debug_paths(&name, &id, &file.path, &HashMap::new()) {
                if let Ok(identity) = crate::compressed::identity(&path) {
                    files.insert(identity);
                }
            }
        }
        if let Ok(identity) = crate::compressed::identity(Path::new(&file.path)) {
            files.insert(identity);
        }
    }
    files.into_iter().collect::<Vec<_>>().join("\n")
}
fn lookup(symbols: &[Symbol], address: u64) -> Option<&Symbol> {
    let i = symbols
        .partition_point(|s| s.address <= address)
        .checked_sub(1)?;
    let s = &symbols[i];
    (address - s.address < s.size).then_some(s)
}
fn gnu_hash_count(dump: &CoreDump, table: u64) -> Option<u32> {
    if table == 0 {
        return None;
    }
    let buckets = dump.u32(table)?;
    let offset = dump.u32(table.checked_add(4)?)?;
    let bloom = dump.u32(table.checked_add(8)?)?;
    if buckets > 2_000_000 || bloom > 2_000_000 {
        return None;
    }
    let start = table.checked_add(16 + bloom as u64 * 8)?;
    let highest = (0..buckets as u64)
        .filter_map(|b| dump.u32(start + b * 4))
        .max()
        .unwrap_or(0);
    if highest < offset {
        return Some(offset);
    }
    let chains = start.checked_add(buckets as u64 * 4)?;
    for i in highest..highest.checked_add(1_000_000)? {
        if dump.u32(chains.checked_add((i - offset) as u64 * 4)?)? & 1 != 0 {
            return i.checked_add(1);
        }
    }
    None
}
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into()
}
pub fn symbol_folder() -> PathBuf {
    crate::cache::root().join("symbols")
}
pub fn index(folders: &[PathBuf]) -> HashMap<String, Vec<PathBuf>> {
    let mut index = HashMap::<_, Vec<_>>::new();
    let mut seen = HashSet::new();
    for folder in folders {
        let mut pending = vec![folder.clone()];
        while let Some(path) = pending.pop() {
            let Ok(canonical) = path.canonicalize() else {
                continue;
            };
            if !seen.insert(canonical) {
                continue;
            }
            let Ok(entries) = std::fs::read_dir(path) else {
                continue;
            };
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path)
                } else {
                    index.entry(file_name(&path)).or_default().push(path);
                }
            }
        }
    }
    index
}
pub fn demangle(name: &str) -> Option<String> {
    let text = cpp_demangle::Symbol::new(name).ok()?.demangle().ok()?;
    Some(
        if let Some(kind) = text
            .strip_prefix("{vtable(")
            .and_then(|s| s.strip_suffix(")}"))
        {
            format!("vtable for {kind}")
        } else {
            text
        },
    )
}
pub fn function_name(name: &str) -> String {
    let (mut out, mut group, mut depth) = (String::new(), String::new(), 0usize);
    for ch in name.chars() {
        if ch == '(' {
            depth += 1;
        }
        if depth > 0 {
            group.push(ch)
        } else {
            out.push(ch)
        }
        if ch == ')' && depth > 0 {
            depth -= 1;
            if depth == 0 {
                if group == "(anonymous namespace)" {
                    out.push_str(&group);
                }
                group.clear();
            }
        }
    }
    for suffix in [" const", " &&", " &"] {
        if out.ends_with(suffix) {
            out.truncate(out.len() - suffix.len());
        }
    }
    out
}

pub struct ElfFile {
    pub data: memmap2::Mmap,
}
impl ElfFile {
    pub fn open(path: &Path) -> Option<Self> {
        let file = std::fs::File::open(path).ok()?; // Symbol files are mapped read-only for the duration of parsing.
        let data = unsafe { memmap2::Mmap::map(&file) }.ok()?;
        if data.get(..6) != Some(b"\x7fELF\x02\x01") {
            return None;
        }
        object::File::parse(&data[..]).ok()?;
        Some(Self { data })
    }
    pub fn build_id(&self) -> Option<String> {
        let file = object::File::parse(&self.data[..]).ok()?;
        file.build_id()
            .ok()??
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .into()
    }
    fn symbols(&self, base: u64, library: &str, out: &mut LibrarySymbols) {
        let Ok(file) = object::File::parse(&self.data[..]) else {
            return;
        };
        let Some(first) = file.segments().map(|s| s.address()).min() else {
            return;
        };
        let Some(bias) = base.checked_sub(first & !0xfff) else {
            return;
        };
        let mut add = |s: object::Symbol<'_, '_>| {
            if s.is_undefined() || s.address() == 0 || s.size() == 0 {
                return;
            }
            let data = match s.kind() {
                SymbolKind::Data => true,
                SymbolKind::Text => false,
                _ => return,
            };
            if let (Some(address), Ok(name)) = (bias.checked_add(s.address()), s.name()) {
                out.add(
                    Symbol {
                        address,
                        size: s.size(),
                        name: name.into(),
                        library: library.into(),
                    },
                    data,
                );
            }
        };
        if file.symbol_table().is_some() {
            for s in file.symbols() {
                add(s)
            }
        } else {
            for s in file.dynamic_symbols() {
                add(s)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        compressed::{Source, atomic_write},
        core_dump::{FileMapping, Layout, Platform, Segment},
    };

    #[test]
    fn adding_replacing_and_removing_symbol_files_invalidates_saved_results() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("image");
        let library = directory.path().join("libfixture.so");
        let debug = directory.path().join("libfixture.so.dbg");
        let mut bytes = vec![0; 4096];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[0x20..0x28].copy_from_slice(&64u64.to_le_bytes());
        bytes[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        bytes[0x38..0x3a].copy_from_slice(&1u16.to_le_bytes());
        bytes[64..68].copy_from_slice(&4u32.to_le_bytes());
        bytes[80..88].copy_from_slice(&256u64.to_le_bytes());
        bytes[96..104].copy_from_slice(&20u64.to_le_bytes());
        bytes[256..260].copy_from_slice(&4u32.to_le_bytes());
        bytes[260..264].copy_from_slice(&4u32.to_le_bytes());
        bytes[264..268].copy_from_slice(&3u32.to_le_bytes());
        bytes[268..276].copy_from_slice(b"GNU\0\x11\x22\x33\x44");
        std::fs::write(&source, &bytes).unwrap();
        let dump = CoreDump {
            path: source.clone(),
            source: Source::open(&source, None).unwrap(),
            layout: Layout {
                platform: Platform::LinuxX64,
                segments: vec![Segment {
                    address: 0x100000,
                    size: 4096,
                    file_offset: 0,
                    writable: false,
                    executable: true,
                }],
                files: vec![FileMapping {
                    start: 0x100000,
                    end: 0x101000,
                    file_offset: 0,
                    path: library.to_string_lossy().into_owned(),
                    deleted: false,
                }],
                threads: Vec::new(),
                exited_thread_stacks: HashSet::new(),
                shared_cache_ranges: Vec::new(),
            },
        };
        assert_eq!(elf_build_id(&dump, 0x100000).as_deref(), Some("11223344"));
        let initial = cache_identity(&dump);
        std::fs::write(&debug, b"first symbols").unwrap();
        let added = cache_identity(&dump);
        assert_ne!(initial, added);
        atomic_write(&debug, b"other symbols").unwrap();
        assert_ne!(added, cache_identity(&dump));
        std::fs::remove_file(debug).unwrap();
        assert_eq!(initial, cache_identity(&dump));

        bytes[0x38..0x3a].copy_from_slice(&0u16.to_le_bytes());
        let no_id_source = directory.path().join("no-build-id");
        std::fs::write(&no_id_source, &bytes).unwrap();
        let mut dump = dump;
        dump.source = Source::open(&no_id_source, None).unwrap();
        assert!(elf_build_id(&dump, 0x100000).is_none());
        let missing = cache_identity(&dump);
        std::fs::write(&library, &bytes).unwrap();
        assert_ne!(missing, cache_identity(&dump));
    }
}
