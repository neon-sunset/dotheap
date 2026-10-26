use crate::{
    core_dump::CoreDump,
    heap::MethodTableInfo,
    metadata::{AssemblyMetadata, compressed_integer},
    portable_pdb::PortablePdb,
    runtime::ContractDescriptor,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldInfo {
    pub name: String,
    pub offset: usize,
    pub element_type: u8,
    pub declaring_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_type: Option<u64>,
}
impl FieldInfo {
    pub fn is_native_pointer(&self) -> bool {
        matches!(self.element_type, 0xf | 0x18 | 0x19 | 0x1b)
    }

    pub fn is_reference(&self) -> bool {
        [0x0e, 0x12, 0x14, 0x1c, 0x1d].contains(&self.element_type)
    }
    pub fn primitive_size(&self) -> Option<usize> {
        match self.element_type {
            2 | 4 | 5 => Some(1),
            3 | 6 | 7 => Some(2),
            8 | 9 | 0xc => Some(4),
            0xa | 0xb | 0xd | 0xf | 0x18 | 0x19 | 0x1b => Some(8),
            _ if self.is_reference() => Some(8),
            _ => None,
        }
    }
    pub fn type_name(&self) -> String {
        match self.element_type {
            2 => "bool",
            3 => "char",
            4 => "sbyte",
            5 => "byte",
            6 => "short",
            7 => "ushort",
            8 => "int",
            9 => "uint",
            0xa => "long",
            0xb => "ulong",
            0xc => "float",
            0xd => "double",
            0xf => "pointer",
            0x11 => "struct",
            0x18 => "nint",
            0x19 => "nuint",
            0x1b => "function pointer",
            _ if self.is_reference() => "reference",
            _ => return format!("type {}", self.element_type),
        }
        .into()
    }
}

pub struct TypeNames {
    resolver: Mutex<Resolver>,
}
struct Resolver {
    dump: Arc<CoreDump>,
    free: u64,
    dlls: HashMap<String, PathBuf>,
    pdb_files: HashMap<String, PathBuf>,
    images: HashMap<String, Vec<(u64, u64)>>,
    image_bases: HashMap<u64, String>,
    module_names: HashMap<u64, Option<String>>,
    assemblies: HashMap<String, Option<Arc<AssemblyMetadata>>>,
    names: HashMap<u64, String>,
    field_cache: HashMap<u64, Arc<Vec<FieldInfo>>>,
    known: Vec<u64>,
    name_offset: Option<u64>,
    type_def_offset: Option<Option<u64>>,
    module_by_name: Option<HashMap<String, u64>>,
    re_registers: HashMap<u64, bool>,
    pdbs: HashMap<String, Option<Arc<PortablePdb>>>,
    await_locations: HashMap<u64, BTreeMap<i32, String>>,
}

impl TypeNames {
    pub fn new(
        dump: Arc<CoreDump>,
        free: u64,
        folders: &[PathBuf],
        method_tables: impl IntoIterator<Item = u64>,
    ) -> Self {
        Self::new_with_progress(
            dump,
            free,
            folders,
            method_tables,
            crate::progress::Progress::none(),
        )
    }

    pub(crate) fn new_with_progress(
        dump: Arc<CoreDump>,
        free: u64,
        folders: &[PathBuf],
        method_tables: impl IntoIterator<Item = u64>,
        progress: crate::progress::Progress<'_>,
    ) -> Self {
        progress.report(0.0);
        let (mut dlls, mut pdb_files) = (HashMap::new(), HashMap::new());
        for folder in folders {
            let mut pending = vec![folder.clone()];
            let mut seen = HashSet::new();
            while let Some(path) = pending.pop() {
                let Ok(canonical) = path.canonicalize() else {
                    continue;
                };
                if !seen.insert(canonical) {
                    continue;
                }
                let Ok(entries) = std::fs::read_dir(&path) else {
                    continue;
                };
                let mut entries: Vec<_> = entries.flatten().collect();
                entries.sort_by_key(|e| e.file_name());
                for entry in entries {
                    let path = entry.path();
                    if path.is_dir() {
                        pending.push(path);
                    } else if path.extension().is_some_and(|e| e == "dll") {
                        if let Some(name) = path.file_stem().and_then(|n| n.to_str()) {
                            dlls.entry(name.to_owned()).or_insert(path);
                        }
                    } else if path.extension().is_some_and(|e| e == "pdb")
                        && let Some(name) = path.file_name().and_then(|n| n.to_str())
                    {
                        pdb_files.entry(name.to_lowercase()).or_insert(path);
                    }
                }
            }
        }
        let (mut images, mut image_bases) = (HashMap::<String, Vec<_>>::new(), HashMap::new());
        for file in dump
            .files
            .iter()
            .filter(|f| f.file_offset == 0 && f.path.ends_with(".dll"))
        {
            let name = Path::new(&file.path)
                .file_stem()
                .unwrap()
                .to_string_lossy()
                .into_owned();
            images
                .entry(name.clone())
                .or_default()
                .push((file.start, file.end));
            image_bases.insert(file.start, name);
        }
        for ranges in images.values_mut() {
            ranges.sort_by_key(|&(a, b)| std::cmp::Reverse(b - a));
        }
        let mut known: BTreeSet<_> = method_tables.into_iter().collect();
        let count = known.len();
        for (i, mt) in known.clone().into_iter().enumerate() {
            progress.items(i, count);
            if dump.u32(mt).is_some_and(|f| f & 0x80000 != 0)
                && let Some(element) = dump
                    .u64(mt.wrapping_add(48))
                    .filter(|&v| v != 0 && v & 1 == 0)
            {
                known.insert(element);
            }
        }
        progress.report(1.0);
        Self {
            resolver: Mutex::new(Resolver {
                dump,
                free,
                dlls,
                pdb_files,
                images,
                image_bases,
                module_names: HashMap::new(),
                assemblies: HashMap::new(),
                names: HashMap::new(),
                field_cache: HashMap::new(),
                known: known.into_iter().collect(),
                name_offset: None,
                type_def_offset: None,
                module_by_name: None,
                re_registers: HashMap::new(),
                pdbs: HashMap::new(),
                await_locations: HashMap::new(),
            }),
        }
    }
    pub fn name(&self, mt: u64) -> String {
        self.resolver.lock().unwrap().name(mt, 0)
    }
    pub fn fields(&self, mt: u64) -> Arc<Vec<FieldInfo>> {
        self.resolver.lock().unwrap().fields(mt)
    }
    pub fn is_free(&self, mt: u64) -> bool {
        self.resolver.lock().unwrap().free == mt
    }
    pub fn is_struct(&self, mt: u64) -> bool {
        self.resolver.lock().unwrap().is_struct(mt)
    }
    pub fn known_method_tables(&self) -> Vec<u64> {
        self.resolver.lock().unwrap().known.clone()
    }
    pub fn type_argument_handles(&self, mt: u64) -> Vec<u64> {
        self.resolver.lock().unwrap().type_argument_handles(mt)
    }
    pub fn missing_assemblies(&self) -> Vec<String> {
        let r = self.resolver.lock().unwrap();
        let mut names: Vec<_> = r
            .assemblies
            .iter()
            .filter(|(_, m)| m.is_none())
            .map(|(n, _)| n.clone())
            .collect();
        names.sort();
        names
    }
    pub fn informational_version(&self, name: &str) -> Option<String> {
        let mut r = self.resolver.lock().unwrap();
        if !r.is_assembly(name) {
            return None;
        }
        r.metadata(name)?.informational_version()
    }
    pub fn method_name(&self, mt: u64, rid: usize) -> Option<String> {
        let mut r = self.resolver.lock().unwrap();
        let (_, m) = r.metadata_for(mt)?;
        m.method_name(rid)
    }
    pub fn method_location(&self, mt: u64, rid: usize) -> Option<String> {
        let mut r = self.resolver.lock().unwrap();
        let (name, m) = r.metadata_for(mt)?;
        let pdb = r.pdb(&name, &m)?;
        let first = pdb.sequence_points(rid).into_iter().next()?;
        Some(format!(
            "{}:{}",
            filename(&pdb.document_name(first.document)?),
            first.line
        ))
    }
    pub fn await_locations(&self, mt: u64) -> BTreeMap<i32, String> {
        let mut r = self.resolver.lock().unwrap();
        if let Some(found) = r.await_locations.get(&mt) {
            return found.clone();
        }
        let locations: BTreeMap<i32, String> = (|| {
            let rid = (r.dump.u32(mt.checked_add(8)?)? >> 8) as usize;
            let (name, metadata) = r.metadata_for(mt)?;
            let pdb = r.pdb(&name, &metadata)?;
            let method = metadata
                .methods(rid)
                .find(|&m| metadata.method_name(m).as_deref() == Some("MoveNext"))?;
            Some(
                pdb.await_locations(method, metadata.method_body(method))
                    .into_iter()
                    .map(|(state, (document, line))| {
                        (state, format!("{}:{line}", filename(&document)))
                    })
                    .collect(),
            )
        })()
        .unwrap_or_default();
        r.await_locations.insert(mt, locations.clone());
        locations
    }
    pub fn finalizer_re_registers(&self, mt: u64) -> bool {
        let mut r = self.resolver.lock().unwrap();
        if let Some(&found) = r.re_registers.get(&mt) {
            return found;
        }
        let mut current = mt;
        let mut found = false;
        for _ in 0..32 {
            let Some((_, metadata)) = r.metadata_for(current) else {
                break;
            };
            let rid = (r.dump.u32(current.wrapping_add(8)).unwrap_or(0) >> 8) as usize;
            if let Some(method) = metadata
                .methods(rid)
                .find(|&m| metadata.method_name(m).as_deref() == Some("Finalize"))
            {
                found = metadata.calls(method, "System.GC", "ReRegisterForFinalize");
                break;
            }
            current = r.dump.u64(current.wrapping_add(16)).unwrap_or(0);
            if current == 0 {
                break;
            }
        }
        r.re_registers.insert(mt, found);
        found
    }
    pub fn static_field_slots(
        &self,
        descriptor: Option<&ContractDescriptor>,
    ) -> HashMap<u64, String> {
        self.static_field_slots_with_progress(descriptor, crate::progress::Progress::none())
    }

    pub(crate) fn static_field_slots_with_progress(
        &self,
        descriptor: Option<&ContractDescriptor>,
        progress: crate::progress::Progress<'_>,
    ) -> HashMap<u64, String> {
        self.resolver
            .lock()
            .unwrap()
            .static_field_slots(descriptor, progress)
            .unwrap_or_default()
    }
}

impl Resolver {
    fn is_assembly(&self, name: &str) -> bool {
        self.dlls.contains_key(name) || self.images.contains_key(name)
    }
    fn metadata(&mut self, name: &str) -> Option<Arc<AssemblyMetadata>> {
        if let Some(m) = self.assemblies.get(name) {
            return m.clone();
        }
        let m = if let Some(path) = self.dlls.get(name) {
            AssemblyMetadata::from_file(path)
        } else {
            self.images
                .get(name)
                .into_iter()
                .flatten()
                .find_map(|&(start, end)| {
                    AssemblyMetadata::from_dump(self.dump.clone(), start, (end - start) as usize)
                })
        }
        .map(Arc::new);
        self.assemblies.insert(name.to_owned(), m.clone());
        m
    }
    fn metadata_for(&mut self, mt: u64) -> Option<(String, Arc<AssemblyMetadata>)> {
        let module = self.dump.u64(mt.checked_add(24)?)?;
        let name = self.module_name(module)?;
        let metadata = self.metadata(&name)?;
        Some((name, metadata))
    }
    fn module_name(&mut self, module: u64) -> Option<String> {
        if module == 0 {
            return None;
        }
        if let Some(name) = self.module_names.get(&module) {
            return name.clone();
        }
        if self.name_offset.is_none() {
            for offset in (0..0x400).step_by(8) {
                if let Some(name) = module
                    .checked_add(offset)
                    .and_then(|at| self.dump.u64(at))
                    .and_then(|p| self.dump.c_string(p, 256))
                    && self.is_assembly(&name)
                {
                    self.name_offset = Some(offset);
                    break;
                }
            }
        }
        let mut name = None;
        if let Some(offset) = self.name_offset
            && let Some(p) = module.checked_add(offset).and_then(|at| self.dump.u64(at))
        {
            name = self.dump.c_string(p, 256).filter(|n| self.is_assembly(n));
            if name.is_none() {
                name = self
                    .dump
                    .files
                    .iter()
                    .find(|f| p >= f.start && p < f.end && f.path.ends_with(".dll"))
                    .and_then(|f| Path::new(&f.path).file_stem())
                    .map(|s| s.to_string_lossy().into_owned());
            }
        }
        if name.is_none() {
            for offset in (0..0x400).step_by(8) {
                if let Some(n) = module
                    .checked_add(offset)
                    .and_then(|at| self.dump.u64(at))
                    .and_then(|p| self.image_bases.get(&p))
                    .cloned()
                {
                    name = Some(n);
                    break;
                }
            }
        }
        self.module_names.insert(module, name.clone());
        name
    }
    fn name(&mut self, mt: u64, depth: usize) -> String {
        self.resolve_name(mt, depth, &mut HashSet::new(), &mut 4096)
    }
    fn resolve_name(
        &mut self,
        mt: u64,
        depth: usize,
        path: &mut HashSet<u64>,
        remaining: &mut usize,
    ) -> String {
        if let Some(name) = self.names.get(&mt) {
            return name.clone();
        }
        if depth > 64 || *remaining == 0 || !path.insert(mt) {
            return "?".into();
        }
        *remaining -= 1;
        let flags = self.dump.u32(mt).unwrap_or(0);
        let rid = (self.dump.u32(mt.wrapping_add(8)).unwrap_or(0) >> 8) as usize;
        let result = if mt == self.free
            || (flags == 0x8000_0001 && self.dump.u64(mt.wrapping_add(24)) == Some(0))
        {
            "Free".into()
        } else if flags & MethodTableInfo::HAS_COMPONENT_SIZE != 0
            && [0x80000, 0xa0000].contains(&(flags & 0xf0000))
        {
            let element = self.dump.u64(mt.wrapping_add(48)).unwrap_or(0);
            format!(
                "{}[]",
                if element != 0 && element & 1 == 0 {
                    self.resolve_name(element, depth + 1, path, remaining)
                } else {
                    "?".into()
                }
            )
        } else {
            let module = self.dump.u64(mt.wrapping_add(24)).unwrap_or(0);
            let mut name = if let Some(name) = self.module_name(module) {
                self.metadata(&name)
                    .and_then(|m| m.type_name(rid))
                    .unwrap_or_else(|| format!("<{name} rid {rid}>"))
            } else {
                format!("<MethodTable 0x{mt:x}>")
            };
            if flags & 0x30 != 0 {
                let handles = self.type_argument_handles(mt);
                let arguments = if handles.is_empty() || handles.len() > 1024 {
                    vec!["?".into()]
                } else {
                    handles
                        .into_iter()
                        .map(|t| {
                            if t != 0 && t & 1 == 0 {
                                self.resolve_name(t, depth + 1, path, remaining)
                            } else {
                                "?".into()
                            }
                        })
                        .collect()
                };
                name += &format!("<{}>", arguments.join(", "));
            }
            name
        };
        path.remove(&mt);
        if depth == 0 {
            self.names.insert(mt, result.clone());
        }
        result
    }
    fn ee_class(&self, mt: u64) -> Option<u64> {
        let v = self.dump.u64(mt.checked_add(40)?)?;
        if v & 1 == 0 {
            Some(v)
        } else {
            self.dump
                .u64((v & !1).checked_add(40)?)
                .filter(|&p| p & 1 == 0)
        }
    }
    fn field_count(&self, mt: u64) -> usize {
        self.ee_class(mt)
            .and_then(|ee| ee.checked_add(50))
            .and_then(|at| self.dump.u16(at))
            .unwrap_or(0) as usize
    }
    fn fields(&mut self, mt: u64) -> Arc<Vec<FieldInfo>> {
        if let Some(fields) = self.field_cache.get(&mt) {
            return fields.clone();
        }
        let mut result = Vec::new();
        let mut current = mt;
        let mut seen = HashSet::new();
        while current != 0 && seen.len() < 32 && seen.insert(current) {
            let parent = self.dump.u64(current.wrapping_add(16)).unwrap_or(0);
            let inherited = if parent != 0 {
                self.field_count(parent)
            } else {
                0
            };
            let count = self.field_count(current).saturating_sub(inherited);
            if count > 0
                && count < 4096
                && let Some(list) = self
                    .ee_class(current)
                    .and_then(|ee| ee.checked_add(24))
                    .and_then(|at| self.dump.u64(at))
                    .filter(|&p| p != 0)
            {
                let declaring_type = self.name(current, 0);
                let module = self.dump.u64(current.wrapping_add(24)).unwrap_or(0);
                let metadata = self
                    .module_name(module)
                    .and_then(|name| self.metadata(&name));
                for k in 0..count {
                    let Some(fd) = list.checked_add(k as u64 * 16) else {
                        break;
                    };
                    let (Some(d1), Some(d2)) = (
                        self.dump.u32(fd.wrapping_add(8)),
                        self.dump.u32(fd.wrapping_add(12)),
                    ) else {
                        break;
                    };
                    if d1 & (1 << 24) != 0 {
                        continue;
                    }
                    let rid = (d1 & 0xff_ffff) as usize;
                    let element_type = (d2 >> 27) as u8;
                    let value_type = if element_type == 0x11 {
                        metadata
                            .as_ref()
                            .and_then(|m| self.value_type_of_field(rid, m, module, current))
                    } else {
                        None
                    };
                    result.push(FieldInfo {
                        name: metadata
                            .as_ref()
                            .and_then(|m| m.field_name(rid))
                            .unwrap_or_else(|| format!("field {rid}")),
                        offset: 8 + (d2 & 0x7ff_ffff) as usize,
                        element_type,
                        declaring_type: declaring_type.clone(),
                        value_type,
                    });
                }
            }
            current = parent;
        }
        result.sort_by_key(|f| f.offset);
        let result = Arc::new(result);
        self.field_cache.insert(mt, result.clone());
        result
    }
    fn is_struct(&self, mt: u64) -> bool {
        self.dump
            .u32(mt)
            .is_some_and(|f| [0x40000, 0x50000].contains(&(f & 0xf0000)))
    }
    fn type_argument_handles(&self, mt: u64) -> Vec<u64> {
        (|| {
            if self.dump.u32(mt)? & 0x30 == 0 {
                return None;
            }
            let per_inst = self.dump.u64(mt.checked_add(48)?)?;
            let info = self.dump.u32(per_inst.checked_sub(4)?)?;
            let (dicts, args) = ((info & 0xffff) as u64, (info >> 16) as u64);
            if dicts == 0 || args == 0 {
                return None;
            }
            let dict = self.dump.u64(per_inst.checked_add((dicts - 1) * 8)?)?;
            Some(
                (0..args)
                    .map(|i| {
                        dict.checked_add(i * 8)
                            .and_then(|at| self.dump.u64(at))
                            .unwrap_or(0)
                    })
                    .collect(),
            )
        })()
        .unwrap_or_default()
    }
    fn value_type_of_field(
        &mut self,
        rid: usize,
        metadata: &AssemblyMetadata,
        module: u64,
        declaring: u64,
    ) -> Option<u64> {
        let sig = metadata.field_signature(rid)?;
        if sig.first() != Some(&6) {
            return None;
        }
        let mut at = 1;
        while matches!(sig.get(at), Some(0x1f | 0x20)) {
            at += 1;
            compressed_integer(sig, &mut at)?;
        }
        match *sig.get(at)? {
            0x11 => {
                at += 1;
                let token = compressed_integer(sig, &mut at)?;
                self.type_handle(token, metadata, module)
            }
            0x13 => {
                at += 1;
                let index = compressed_integer(sig, &mut at)? as usize;
                self.type_argument_handles(declaring)
                    .get(index)
                    .copied()
                    .filter(|&t| t & 1 == 0 && self.is_struct(t))
            }
            0x15 if sig.get(at + 1) == Some(&0x11) => {
                at += 2;
                let token = compressed_integer(sig, &mut at)?;
                let open = self.type_handle(token, metadata, module)?;
                let open_module = self.dump.u64(open.checked_add(24)?)?;
                let open_rid = self.dump.u32(open.checked_add(8)?)? >> 8;
                let mut candidates = self.known.iter().copied().filter(|&mt| {
                    mt != open
                        && self.dump.u64(mt.wrapping_add(24)) == Some(open_module)
                        && self.dump.u32(mt.wrapping_add(8)).map(|f| f >> 8) == Some(open_rid)
                });
                let first = candidates.next()?;
                candidates.next().is_none().then_some(first)
            }
            _ => None,
        }
    }
    fn type_handle(&mut self, token: u32, metadata: &AssemblyMetadata, module: u64) -> Option<u64> {
        let row = (token >> 2) as usize;
        match token & 3 {
            0 => self.type_def_method_table(module, row),
            1 => {
                let (assembly, name) = reference_name(metadata, row, 0)?;
                if self.module_by_name.is_none() {
                    let mut map = HashMap::new();
                    for mt in self.known.clone() {
                        if let Some(module) = self.dump.u64(mt.wrapping_add(24)).filter(|&m| m != 0)
                            && let Some(name) = self.module_name(module)
                        {
                            map.entry(name).or_insert(module);
                        }
                    }
                    self.module_by_name = Some(map);
                }
                let target = self
                    .module_by_name
                    .as_ref()?
                    .get(&assembly)
                    .copied()
                    .or_else(|| assembly.is_empty().then_some(module))?;
                let name_module = self.module_name(target)?;
                let target_metadata = self.metadata(&name_module)?;
                self.type_def_method_table(target, target_metadata.type_definition(&name)?)
            }
            _ => None,
        }
    }
    fn type_def_method_table(&mut self, module: u64, rid: usize) -> Option<u64> {
        if self.type_def_offset.is_none() {
            self.type_def_offset = Some(self.find_type_def_offset());
        }
        let mut node = module.checked_add(self.type_def_offset??)?;
        let mut index = rid;
        for _ in 0..64 {
            let count = self.dump.u32(node.checked_add(16)?)? as usize;
            let table = self.dump.u64(node.checked_add(8)?)?;
            let mask = self.dump.u64(node.checked_add(24)?)?;
            if index < count {
                return self
                    .dump
                    .u64(table.checked_add(index as u64 * 8)?)
                    .map(|v| v & !mask)
                    .filter(|&v| v != 0);
            }
            index -= count;
            node = self.dump.u64(node)?;
            if node == 0 {
                break;
            }
        }
        None
    }
    fn find_type_def_offset(&self) -> Option<u64> {
        let mut votes = BTreeMap::<u64, usize>::new();
        for &mt in self
            .known
            .iter()
            .filter(|&&mt| self.dump.u32(mt).is_some_and(|f| f & 0x80030 == 0))
            .take(40)
        {
            let (Some(module), Some(rid)) = (
                self.dump.u64(mt.wrapping_add(24)),
                self.dump
                    .u32(mt.wrapping_add(8))
                    .map(|f| (f >> 8) as u64)
                    .filter(|&r| r > 0),
            ) else {
                continue;
            };
            for off in (0..0x1000u64).step_by(8) {
                let Some(node) = module.checked_add(off) else {
                    continue;
                };
                let (Some(count), Some(table), Some(mask)) = (
                    self.dump.u32(node.wrapping_add(16)),
                    self.dump.u64(node.wrapping_add(8)),
                    self.dump.u64(node.wrapping_add(24)),
                ) else {
                    continue;
                };
                if rid < count as u64
                    && table != 0
                    && mask < 8
                    && table
                        .checked_add(rid * 8)
                        .and_then(|p| self.dump.u64(p))
                        .is_some_and(|v| v & !mask == mt)
                {
                    *votes.entry(off).or_default() += 1;
                }
            }
        }
        votes
            .into_iter()
            .filter(|(_, count)| *count >= 3)
            .max_by_key(|&(off, count)| (count, std::cmp::Reverse(off)))
            .map(|(off, _)| off)
    }
    fn static_field_slots(
        &mut self,
        descriptor: Option<&ContractDescriptor>,
        progress: crate::progress::Progress<'_>,
    ) -> Option<HashMap<u64, String>> {
        let d = descriptor?;
        let (dsi, aux, gc_statics, type_map, static_count) = (
            d.offset("DynamicStaticsInfo", "!")?,
            d.offset("MethodTable", "AuxiliaryData")?,
            d.offset("DynamicStaticsInfo", "GCStatics")?,
            d.offset("Module", "TypeDefToMethodTableMap")?,
            d.offset("EEClass", "NumStaticFields")?,
        );
        let mask = d
            .globals
            .get("StaticsPointerMask")
            .copied()
            .unwrap_or(!1u64);
        let mut tables: BTreeSet<_> = self.known.iter().copied().collect();
        let modules: BTreeSet<_> = self
            .known
            .iter()
            .filter_map(|mt| self.dump.u64(mt.wrapping_add(24)))
            .filter(|&m| m != 0)
            .collect();
        let count = modules.len();
        for (index, module) in modules.into_iter().enumerate() {
            progress.portion(0.0, 0.25).items(index, count);
            let Some(mut map) = module.checked_add(type_map) else {
                continue;
            };
            for _ in 0..64 {
                if map == 0 {
                    break;
                }
                let (Some(data), Some(count)) = (
                    map.checked_add(d.offset("ModuleLookupMap", "TableData").unwrap_or(8))
                        .and_then(|p| self.dump.u64(p)),
                    map.checked_add(d.offset("ModuleLookupMap", "Count").unwrap_or(16))
                        .and_then(|p| self.dump.u32(p)),
                ) else {
                    break;
                };
                let flags = map
                    .checked_add(
                        d.offset("ModuleLookupMap", "SupportedFlagsMask")
                            .unwrap_or(24),
                    )
                    .and_then(|p| self.dump.u64(p))
                    .unwrap_or(0);
                if data != 0 && count < 1 << 20 {
                    for k in 0..count {
                        if let Some(v) = data
                            .checked_add(k as u64 * 8)
                            .and_then(|p| self.dump.u64(p))
                            .map(|v| v & !flags)
                            .filter(|&v| v != 0)
                        {
                            tables.insert(v);
                        }
                    }
                }
                map = map
                    .checked_add(d.offset("ModuleLookupMap", "Next").unwrap_or(0))
                    .and_then(|p| self.dump.u64(p))
                    .unwrap_or(0);
            }
        }
        let mut slots = HashMap::new();
        let count = tables.len();
        for (index, mt) in tables.into_iter().enumerate() {
            if index % 32 == 0 {
                progress.portion(0.25, 1.0).items(index, count);
            }
            let fields = (|| {
                if self.dump.u32(mt.checked_add(8)?)? & 2 == 0 {
                    return None;
                }
                let aux_data = self.dump.u64(mt.checked_add(aux)?)?;
                let raw = self
                    .dump
                    .u64(aux_data.checked_sub(dsi)?.checked_add(gc_statics)?)?;
                if raw & mask == 0 {
                    return None;
                }
                let ee = self.ee_class(mt)?;
                let list = self.dump.u64(ee.checked_add(24)?)?;
                let statics = self.dump.u16(ee.checked_add(static_count)?)?;
                (list != 0 && statics > 0).then_some((raw & mask, list, statics as usize))
            })();
            let Some((base, list, statics)) = fields else {
                continue;
            };
            let parent = self.dump.u64(mt.wrapping_add(16)).unwrap_or(0);
            let introduced = self.field_count(mt).saturating_sub(if parent == 0 {
                0
            } else {
                self.field_count(parent)
            });
            let name = short(&self.name(mt, 0));
            let metadata = self.metadata_for(mt).map(|(_, m)| m);
            for k in introduced..introduced + statics {
                let Some(fd) = list.checked_add(k as u64 * 16) else {
                    break;
                };
                let (Some(d1), Some(d2)) = (
                    self.dump.u32(fd.wrapping_add(8)),
                    self.dump.u32(fd.wrapping_add(12)),
                ) else {
                    break;
                };
                if d1 & (1 << 24) == 0
                    || d1 & (3 << 25) != 0
                    || ![0xe, 0x11, 0x12, 0x14, 0x15, 0x1c, 0x1d].contains(&(d2 >> 27))
                {
                    continue;
                }
                let rid = (d1 & 0xff_ffff) as usize;
                if let Some(slot) = base.checked_add((d2 & 0x7ff_ffff) as u64) {
                    slots.insert(
                        slot,
                        format!(
                            "{name}.{}",
                            metadata
                                .as_ref()
                                .and_then(|m| m.field_name(rid))
                                .unwrap_or_else(|| format!("field {rid}"))
                        ),
                    );
                }
            }
        }
        progress.report(1.0);
        Some(slots)
    }
    fn pdb(&mut self, name: &str, metadata: &AssemblyMetadata) -> Option<Arc<PortablePdb>> {
        if let Some(pdb) = self.pdbs.get(name) {
            return pdb.clone();
        }
        let entries = metadata.debug_entries();
        let mut found = entries
            .iter()
            .find(|(kind, _)| *kind == 17)
            .and_then(|(_, bytes)| PortablePdb::embedded(bytes));
        if found.is_none()
            && let Some((_, cv)) = entries
                .iter()
                .find(|(kind, bytes)| *kind == 2 && bytes.len() > 24 && bytes.starts_with(b"RSDS"))
        {
            let text = &cv[24..];
            let text = &text[..memchr::memchr(0, text).unwrap_or(text.len())];
            let path = String::from_utf8_lossy(text).into_owned();
            let file = filename(&path).to_lowercase();
            let candidates = [
                Some(PathBuf::from(&path)),
                self.pdb_files.get(&file).cloned(),
                self.pdb_files
                    .get(&format!("{}.pdb", name.to_lowercase()))
                    .cloned(),
            ];
            for path in candidates.into_iter().flatten() {
                if let Some(pdb) = std::fs::read(path).ok().and_then(PortablePdb::parse)
                    && pdb.id.get(..16) == Some(&cv[4..20])
                {
                    found = Some(pdb);
                    break;
                }
            }
        }
        let found = found.map(Arc::new);
        self.pdbs.insert(name.to_owned(), found.clone());
        found
    }
}

fn reference_name(
    metadata: &AssemblyMetadata,
    rid: usize,
    depth: usize,
) -> Option<(String, String)> {
    if depth >= 8 {
        return None;
    }
    let r = metadata.type_reference(rid)?;
    let local = if r.namespace.is_empty() {
        r.name.clone()
    } else {
        format!("{}.{}", r.namespace, r.name)
    };
    match r.scope_table {
        0x23 => Some((metadata.assembly_reference_name(r.scope_rid)?, local)),
        1 => {
            let (assembly, name) = reference_name(metadata, r.scope_rid, depth + 1)?;
            Some((assembly, format!("{name}+{}", r.name)))
        }
        _ => Some((String::new(), local)),
    }
}
fn filename(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
}
pub fn property_name(path: &str) -> String {
    let mut text = path.to_owned();
    while let Some(close) = text.find(">k__BackingField") {
        let Some(open) = text[..close].rfind('<') else {
            break;
        };
        let name = text[open + 1..close].to_owned();
        text.replace_range(open..close + ">k__BackingField".len(), &name);
    }
    text
}
pub fn without_namespaces(name: &str) -> String {
    let mut out = String::new();
    let mut start = 0;
    for (i, ch) in name.char_indices() {
        if "<>,[] ".contains(ch) {
            out += name[start..i].rsplit('.').next().unwrap_or("");
            out.push(ch);
            start = i + ch.len_utf8();
        }
    }
    out += name[start..].rsplit('.').next().unwrap_or("");
    out
}
pub fn short(name: &str) -> String {
    name.replace("System.Text.RegularExpressions.Symbolic.", "Symbolic.")
        .replace("System.Collections.Generic.", "")
        .replace("System.Collections.Concurrent.", "")
        .replace("System.Threading.Tasks.", "")
}
