use crate::{
    Result,
    analysis::HeapSnapshot,
    bytes::ReadBytes,
    heap::Gc,
    native_graph::{Chunk, NativeGraph},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenFile {
    pub handle: usize,
    pub path: Option<String>,
    pub fd: Option<i64>,
    pub r#async: bool,
    pub stream: Option<usize>,
    pub access: Option<String>,
    pub position: Option<i64>,
    pub unwritten: Option<i64>,
    pub buffer_size: Option<i64>,
    pub extra_references: i64,
    pub pending: Option<String>,
    pub memory_backing: Option<i64>,
    pub owner: String,
    pub owner_object: Option<usize>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MappedFile {
    pub path: String,
    pub bytes: u64,
    pub mappings: usize,
    pub kind: String,
    pub deleted: bool,
    pub resident: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeFileName {
    pub chunk: usize,
    pub class_name: String,
    pub path: String,
    pub fd: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Files {
    pub open: Vec<OpenFile>,
    pub mapped: Vec<MappedFile>,
    pub native: Vec<NativeFileName>,
}

struct FileStream {
    object: usize,
    access: Option<String>,
    position: Option<i64>,
    unwritten: Option<i64>,
    buffer_size: Option<i64>,
}

impl HeapSnapshot {
    pub fn mapped_file_path(&self, handle: usize) -> Option<String> {
        let referrers = |object| {
            self.graph
                .referrer_rows
                .row(object)
                .iter()
                .map(|&o| o as usize)
                .filter(|&o| self.graph.live[o] != 0)
                .collect::<Vec<_>>()
        };
        let mapped = referrers(handle).into_iter().find(|&o| {
            self.type_name(o) == "Microsoft.Win32.SafeHandles.SafeMemoryMappedFileHandle"
        })?;
        let mut level: Vec<_> = referrers(mapped)
            .into_iter()
            .filter(|&o| self.type_name(o) == "System.IO.MemoryMappedFiles.MemoryMappedFile")
            .flat_map(referrers)
            .collect();
        let mut seen: HashSet<_> = level.iter().copied().collect();
        for _ in 0..4 {
            let mut next = Vec::new();
            for object in level {
                if self.type_name(object)
                    == "Microsoft.Win32.SafeHandles.SafeMemoryMappedViewHandle"
                    && let Some(address) = self.integer(object, "handle").filter(|&a| a != 0)
                    && let Some(file) = self.dump.file_mapping(address as u64)
                {
                    return Some(file.path.clone());
                }
                next.extend(
                    self.graph
                        .references(object)
                        .into_iter()
                        .map(|o| o as usize)
                        .filter(|o| seen.insert(*o)),
                );
            }
            if next.is_empty() || next.len() > 10_000 {
                break;
            }
            level = next;
        }
        None
    }

    fn native_file_paths(
        &self,
        chunk: &Chunk,
        graph: &NativeGraph,
        relative: bool,
    ) -> Vec<(String, Option<i64>)> {
        let start = chunk.user_start();
        let words = (chunk.user_end().saturating_sub(start).min(1024) / 8) as usize;
        if words < 4 {
            return Vec::new();
        }
        let Some(bytes) = self.dump.bytes(start, words * 8) else {
            return Vec::new();
        };
        let mut paths = Vec::new();
        for index in 0..words - 1 {
            let pointer = bytes.u64(index * 8).unwrap();
            let length = bytes.u64((index + 1) * 8).unwrap();
            if !(2..=4096).contains(&length) || pointer & 0xffff_0000_0000_0000 != 0 {
                continue;
            }
            if pointer == start + (index + 2) as u64 * 8 {
                if length > 15 {
                    continue;
                }
            } else if graph.chunk(pointer).is_none() {
                continue;
            }
            let Some(text) = self.dump.bytes(pointer, length as usize + 1) else {
                continue;
            };
            if text[length as usize] != 0 {
                continue;
            }
            let text = &text[..length as usize];
            if !text.iter().all(|&b| (0x20..0x7f).contains(&b)) {
                continue;
            }
            if text[0] != b'/'
                && !(relative && text.len() >= 3 && text.contains(&b'/') && !text.contains(&b' '))
            {
                continue;
            }
            let end = (index + 4) * 8;
            let fd = [end, end + 4]
                .into_iter()
                .filter_map(|at| bytes.u32(at).map(|v| v as i32 as i64))
                .find(|&v| (3..1 << 20).contains(&v));
            paths.push((String::from_utf8_lossy(text).into_owned(), fd));
        }
        paths
    }

    pub fn files(&self) -> Result<Arc<Files>> {
        let compute = || {
            let resident = self.residency().is_some_and(|r| r.agrees());
            let mut rss_by_path = HashMap::<&str, u64>::new();
            if resident {
                for mapping in self.smaps.iter().flatten() {
                    *rss_by_path.entry(&mapping.path).or_default() += mapping.rss;
                }
            }
            let open = if self.heap.kind.gc == Gc::None {
                Vec::new()
            } else {
                self.open_files()?
            };
            let mut mapped: BTreeMap<String, MappedFile> = BTreeMap::new();
            for file in &self.dump.files {
                let entry = mapped
                    .entry(file.path.clone())
                    .or_insert_with(|| MappedFile {
                        path: file.path.clone(),
                        bytes: 0,
                        mappings: 0,
                        kind: if file.path.to_lowercase().ends_with(".dll")
                            || file.path.to_lowercase().ends_with(".exe")
                        {
                            "assembly"
                        } else {
                            "data"
                        }
                        .into(),
                        deleted: false,
                        resident: resident
                            .then(|| rss_by_path.get(file.path.as_str()).copied().unwrap_or(0)),
                    });
                if self
                    .dump
                    .segments
                    .iter()
                    .any(|s| s.executable && s.address < file.end && file.start < s.end())
                {
                    entry.kind = "native code".into();
                }
                entry.bytes += file.end - file.start;
                entry.mappings += 1;
                entry.deleted |= file.deleted && !file.path.starts_with("/memfd:");
            }
            let mut mapped: Vec<_> = mapped.into_values().collect();
            mapped.sort_by_cached_key(|f| {
                (
                    match f.kind.as_str() {
                        "data" => 0,
                        "assembly" => 1,
                        _ => 2,
                    },
                    std::cmp::Reverse(f.bytes),
                    f.path.clone(),
                )
            });
            let mut native = Vec::new();
            if self.dump.platform.is_linux() {
                let graph = self.native_graph()?;
                let network = self.network()?;
                let managed: HashSet<_> = open
                    .iter()
                    .filter_map(|f| f.fd)
                    .chain(network.sockets.iter().filter_map(|s| s.fd))
                    .collect();
                for (index, chunk) in graph
                    .chunks
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| graph.is_in_use(*i))
                {
                    let Some(class_name) = graph.class_name(index) else {
                        continue;
                    };
                    let file = class_name.contains("File") || class_name.contains("Directory");
                    for (path, fd) in
                        self.native_file_paths(chunk, &graph, file || class_name.contains("Logger"))
                    {
                        native.push(NativeFileName {
                            chunk: index,
                            class_name: class_name.clone(),
                            path,
                            fd: fd.filter(|n| file && !managed.contains(n)),
                        });
                    }
                }
            }
            Ok(Files {
                open,
                mapped,
                native,
            })
        };
        self.saved("files-v2", compute)
    }

    fn file_streams(&self) -> HashMap<usize, FileStream> {
        let mut streams = HashMap::new();
        for object in self
            .objects(&self.type_ids(|s| s == "System.IO.FileStream"))
            .into_iter()
            .filter(|&o| self.graph.live[o] != 0)
        {
            let mut strategy = self.target(object, "_strategy");
            let mut unwritten = None;
            let mut buffer_size = None;
            if let Some(buffer) =
                strategy.filter(|&o| self.type_name(o).ends_with("BufferedFileStreamStrategy"))
            {
                unwritten = self.integer(buffer, "_writePos");
                buffer_size = self.integer(buffer, "_bufferSize");
                strategy = self.target(buffer, "_strategy");
            }
            let Some(handle) = self.target(strategy, "_fileHandle") else {
                continue;
            };
            let access = match self.integer(strategy, "_access") {
                Some(1) => Some("read"),
                Some(2) => Some("write"),
                Some(3) => Some("read and write"),
                _ => None,
            }
            .map(str::to_owned);
            streams.insert(
                handle,
                FileStream {
                    object,
                    access,
                    position: self.integer(strategy, "_filePosition"),
                    unwritten,
                    buffer_size,
                },
            );
        }
        streams
    }

    fn open_files(&self) -> Result<Vec<OpenFile>> {
        let streams = self.file_streams();
        let mut pending = HashMap::new();
        for operation in self
            .objects(&self.type_ids(|s| s.ends_with("SafeFileHandle+ThreadPoolValueTaskSource")))
            .into_iter()
            .filter(|&o| self.graph.live[o] != 0)
        {
            let (Some(handle), Some(kind)) = (
                self.target(operation, "_fileHandle"),
                self.integer(operation, "_operation").filter(|&k| k != 0),
            ) else {
                continue;
            };
            let name = match kind {
                1 => "read",
                2 => "write",
                3 => "scatter read",
                4 => "gather write",
                _ => "operation",
            };
            let offset = self
                .integer(operation, "_fileOffset")
                .map(|o| format!(" at offset {o}"))
                .unwrap_or_default();
            pending.insert(handle, format!("{name}{offset}"));
        }
        let wrappers = [
            "System.IO.FileStream",
            "System.IO.StreamReader",
            "System.IO.StreamWriter",
            "System.IO.BufferedStream",
            "Microsoft.Win32.SafeHandles.SafeMemoryMappedFileHandle",
            "System.IO.MemoryMappedFiles.MemoryMappedFile",
        ];
        let mut files = Vec::new();
        for handle in self
            .objects(&self.type_ids(|s| s == "Microsoft.Win32.SafeHandles.SafeFileHandle"))
            .into_iter()
            .filter(|&o| self.graph.live[o] != 0)
        {
            let state = self.integer(handle, "_state").unwrap_or(1);
            let stream = streams.get(&handle);
            let owner = self.owner(stream.map(|s| s.object).unwrap_or(handle), 500, |s| {
                wrappers.contains(&s) || s.starts_with("System.IO.Strategies.")
            })?;
            let mapping = self
                .graph
                .referrer_rows
                .row(handle)
                .iter()
                .map(|&o| o as usize)
                .find(|&o| {
                    self.graph.live[o] != 0
                        && self.type_name(o)
                            == "Microsoft.Win32.SafeHandles.SafeMemoryMappedFileHandle"
                });
            let memory = mapping
                .filter(|&o| self.integer(o, "_ownsFileHandle").unwrap_or(0) != 0)
                .and_then(|o| self.integer(o, "_capacity"));
            let path = self
                .target(handle, "_path")
                .and_then(|o| self.string_value(o, 1000))
                .or_else(|| {
                    if memory.is_none() {
                        self.mapped_file_path(handle)
                    } else {
                        None
                    }
                });
            files.push(OpenFile {
                handle,
                path,
                fd: if state & 1 == 0 {
                    self.integer(handle, "handle")
                } else {
                    None
                },
                r#async: self
                    .integer(handle, "<IsAsync>k__BackingField")
                    .unwrap_or(0)
                    != 0,
                stream: stream.map(|s| s.object),
                access: stream.and_then(|s| s.access.clone()),
                position: stream.and_then(|s| s.position),
                unwritten: stream.and_then(|s| s.unwritten),
                buffer_size: stream.and_then(|s| s.buffer_size),
                extra_references: ((state >> 2) - 1).max(0),
                pending: pending.remove(&handle),
                memory_backing: memory,
                owner: owner
                    .as_ref()
                    .map(|o| self.owner_text(o))
                    .unwrap_or("nothing else".into()),
                owner_object: owner.and_then(|o| usize::try_from(o.object).ok()),
            });
        }
        Ok(files)
    }
}
