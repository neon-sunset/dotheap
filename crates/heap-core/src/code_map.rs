use crate::{
    Result,
    analysis::HeapSnapshot,
    bytes::ReadBytes,
    core_dump::{CoreDump, Platform},
    runtime::ContractDescriptor,
    symbols::{LibrarySymbols, demangle, function_name},
    type_names::{TypeNames, short},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LocationKind {
    Managed,
    Native,
    Stub,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Location {
    pub kind: LocationKind,
    pub name: String,
}
impl Location {
    pub fn display_name(&self) -> String {
        if self.kind == LocationKind::Managed {
            short(&self.name)
        } else {
            self.name.clone()
        }
    }
}
pub struct CodeMap {
    dump: Arc<CoreDump>,
    names: Arc<TypeNames>,
    symbols: Arc<LibrarySymbols>,
    descriptor: Option<ContractDescriptor>,
    top_map: u64,
    executable: Vec<(u64, u64)>,
    cache: Mutex<HashMap<u64, Option<Location>>>,
}
impl HeapSnapshot {
    pub fn code_map(&self) -> Result<Arc<CodeMap>> {
        self.memo("code map", || {
            Ok(CodeMap::new(
                self.dump.clone(),
                self.names.clone(),
                self.symbols()?,
                self.runtime.as_ref().and_then(|r| r.descriptor.clone()),
            ))
        })
    }
}
impl CodeMap {
    pub fn new(
        dump: Arc<CoreDump>,
        names: Arc<TypeNames>,
        symbols: Arc<LibrarySymbols>,
        descriptor: Option<ContractDescriptor>,
    ) -> Self {
        let top_map = descriptor
            .as_ref()
            .and_then(|d| {
                d.globals
                    .get("ExecutionManagerCodeRangeMapAddress")?
                    .checked_add(d.offset("RangeSectionMap", "TopLevelData").unwrap_or(0))
            })
            .unwrap_or(0);
        let executable = dump
            .segments
            .iter()
            .filter(|s| s.executable)
            .map(|s| (s.address, s.end()))
            .collect();
        Self {
            dump,
            names,
            symbols,
            descriptor,
            top_map,
            executable,
            cache: Mutex::new(HashMap::new()),
        }
    }
    pub fn strip(&self, a: u64) -> u64 {
        if self.dump.platform == Platform::MacOSArm64 {
            a & 0x0000_7fff_ffff_ffff
        } else {
            a
        }
    }
    fn is_executable(&self, a: u64) -> bool {
        self.executable
            .partition_point(|&(start, _)| start <= a)
            .checked_sub(1)
            .is_some_and(|i| a < self.executable[i].1)
    }
    pub fn is_return_address(&self, raw: u64) -> bool {
        let a = self.strip(raw);
        if a <= 0x10000 || !self.is_executable(a) || !self.is_executable(a - 8) {
            return false;
        }
        let Some(b) = self.dump.bytes(a - 8, 8) else {
            return false;
        };
        if self.dump.platform.is_arm64() {
            let ins = b.u32(4).unwrap();
            return ins & 0xfc00_0000 == 0x9400_0000
                || ins & 0xffff_fc1f == 0xd63f_0000
                || ins & 0xfeff_f800 == 0xd63f_0800;
        }
        b[3] == 0xe8 || (2..=7).any(|k| b[8 - k] == 0xff && (b[9 - k] >> 3) & 7 == 2)
    }
    pub fn locate(&self, raw: u64) -> Option<Location> {
        let a = self.strip(raw);
        if let Some(value) = self.cache.lock().unwrap().get(&a) {
            return value.clone();
        }
        let value = self.managed(a).or_else(|| self.native(a));
        self.cache.lock().unwrap().insert(a, value.clone());
        value
    }
    fn native(&self, a: u64) -> Option<Location> {
        let name = if let Some(f) = self.symbols.function(a) {
            format!(
                "{}!{}",
                f.library,
                demangle(&f.name)
                    .map(|n| function_name(&n))
                    .unwrap_or_else(|| f.name.clone())
            )
        } else if self.dump.in_shared_cache(a) {
            format!("system library (dyld shared cache) 0x{a:x}")
        } else {
            let lib = self.symbols.library(a).map(str::to_owned).or_else(|| {
                self.dump
                    .file_mapping(a)
                    .map(|f| f.path.rsplit('/').next().unwrap_or(&f.path).into())
            })?;
            let base = self
                .dump
                .files
                .iter()
                .filter(|f| f.path.rsplit('/').next() == Some(&lib))
                .map(|f| f.start)
                .min()
                .unwrap_or(a);
            format!("{lib}+0x{:x}", a.saturating_sub(base))
        };
        Some(Location {
            kind: LocationKind::Native,
            name,
        })
    }
    fn off(&self, kind: &str, field: &str) -> Option<u64> {
        self.descriptor.as_ref()?.offset(kind, field)
    }
    fn global(&self, name: &str) -> Option<u64> {
        self.descriptor.as_ref()?.globals.get(name).copied()
    }
    fn pointer(&self, base: u64, kind: &str, field: &str) -> Option<u64> {
        self.dump.u64(base.checked_add(self.off(kind, field)?)?)
    }
    fn word(&self, base: u64, kind: &str, field: &str) -> Option<u32> {
        self.dump.u32(base.checked_add(self.off(kind, field)?)?)
    }
    fn method_at(&self, a: u64, section: u64, flags: u32) -> Option<u64> {
        if flags & 4 != 0 {
            return self.precode(a);
        }
        let module = self
            .pointer(section, "RangeSection", "R2RModule")
            .unwrap_or(0);
        if module != 0 {
            self.ready_to_run_method(a, section, module)
        } else {
            self.jit_method(a, section)
        }
    }
    fn managed(&self, a: u64) -> Option<Location> {
        if self.top_map == 0 {
            return None;
        }
        let section = self.range_section(a)?;
        let flags = self.word(section, "RangeSection", "Flags")?;
        let md = self.method_at(a, section, flags);
        Some(if let Some(md) = md {
            Location {
                kind: LocationKind::Managed,
                name: self
                    .method_name(md)
                    .unwrap_or_else(|| format!("managed method 0x{md:x}")),
            }
        } else {
            Location {
                kind: LocationKind::Stub,
                name: "runtime stub".into(),
            }
        })
    }
    pub fn source(&self, raw: u64) -> Option<String> {
        let a = self.strip(raw);
        if self.top_map == 0 {
            return None;
        }
        let section = self.range_section(a)?;
        let md = self.method_at(a, section, self.word(section, "RangeSection", "Flags")?)?;
        let (mt, rid) = self.token(md)?;
        self.names.method_location(mt, rid)
    }
    fn precode(&self, a: u64) -> Option<u64> {
        let g = self
            .global("PlatformMetadata")?
            .checked_add(self.off("PlatformMetadata", "PrecodeMachineDescriptor")?)?;
        let page = self.word(g, "PrecodeMachineDescriptor", "StubCodePageSize")?;
        if page == 0 {
            return None;
        }
        let data = a.checked_add(page as u64)?;
        let stub_type = g
            .checked_add(self.off("PrecodeMachineDescriptor", "StubPrecodeType")?)
            .and_then(|a| self.dump.u8(a));
        let stub = stub_type.is_some()
            && self
                .dump
                .u64(data.checked_add(16)?)
                .map(|v| (v & 255) as u8)
                == stub_type;
        self.dump
            .u64(data.checked_add(if stub { 0 } else { 8 })?)
            .filter(|&md| md != 0)
    }
    fn range_section(&self, a: u64) -> Option<u64> {
        let mut map = self.top_map;
        for level in (1..=5).rev() {
            let index = ((a >> 17) >> ((level - 1) * 8)) & 255;
            map = self.dump.u64(map.checked_add(index * 8)?)? & !1;
            if map == 0 {
                return None;
            }
        }
        for _ in 0..1000 {
            if map == 0 {
                return None;
            }
            let begin = self.pointer(map, "RangeSectionFragment", "RangeBegin")?;
            let end = self.pointer(map, "RangeSectionFragment", "RangeEndOpen")?;
            if (begin..end).contains(&a) {
                let section = self.pointer(map, "RangeSectionFragment", "RangeSection")?;
                if self
                    .pointer(section, "RangeSection", "NextForDelete")
                    .unwrap_or(0)
                    != 0
                {
                    return None;
                }
                return Some(section);
            }
            map = self.pointer(map, "RangeSectionFragment", "Next")?;
        }
        None
    }
    fn jit_method(&self, a: u64, section: u64) -> Option<u64> {
        let heap = self.pointer(section, "RangeSection", "HeapList")?;
        if heap == 0 {
            return None;
        }
        let start = self.pointer(heap, "CodeHeapListNode", "StartAddress")?;
        let end = self.pointer(heap, "CodeHeapListNode", "EndAddress")?;
        if a < start || a > end {
            return None;
        }
        let code = self.find_method_code(
            self.pointer(heap, "CodeHeapListNode", "MapBase")?,
            self.pointer(heap, "CodeHeapListNode", "HeaderMap")?,
            a,
        )?;
        let header = self.dump.u64(code.checked_sub(8)?)?;
        if header <= 15 {
            return None;
        }
        self.pointer(header, "RealCodeHeader", "MethodDesc")
            .filter(|&md| md != 0)
    }
    fn find_method_code(&self, map_base: u64, map_start: u64, pc: u64) -> Option<u64> {
        let unit = |index: u64| self.dump.u32(map_start.checked_add(index / 8 * 4)?);
        let absolute = |index: u64, nibble: u32| {
            map_base
                .checked_add(index.checked_mul(32)?)?
                .checked_add(nibble.checked_sub(1)? as u64 * 4)
        };
        let decode = |unit| decode_code_pointer(map_base, unit);
        let relative = pc.checked_sub(map_base)?;
        let mut index = relative / 32;
        let byte_index = ((relative & 31) / 4) as u32 + 1;
        let mut t = unit(index)?;
        if t & 15 > 8 {
            return decode(t);
        }
        t >>= 28 - (index & 7) as u32 * 4;
        if t & 15 != 0 && t & 15 <= byte_index {
            return absolute(index, t & 15);
        }
        t >>= 4;
        if t != 0 {
            index = index.checked_sub(1)?;
            while t & 15 == 0 {
                t >>= 4;
                index = index.checked_sub(1)?;
            }
            return absolute(index, t & 15);
        }
        index = (index & !7).checked_sub(1)?;
        let mut u = unit(index)?;
        if u == 0 {
            return None;
        }
        if u & 15 > 8 {
            return decode(u);
        }
        while index != 0 && u & 15 == 0 {
            u >>= 4;
            index -= 1;
        }
        absolute(index, u & 15)
    }
    fn ready_to_run_method(&self, a: u64, section: u64, module: u64) -> Option<u64> {
        let info = self.pointer(module, "Module", "ReadyToRunInfo")?;
        if info == 0 {
            return None;
        }
        let functions = self.pointer(info, "ReadyToRunInfo", "RuntimeFunctions")?;
        let count = self.word(info, "ReadyToRunInfo", "NumRuntimeFunctions")?;
        if count == 0 || count > 16_000_000 {
            return None;
        }
        let map = info.checked_add(self.off("ReadyToRunInfo", "EntryPointToMethodDescMap")?)?;
        let base = self.pointer(section, "RangeSection", "RangeBegin")?;
        let size = self.off("RuntimeFunction", "!").unwrap_or(12);
        let rva = u32::try_from(a.checked_sub(base)?).ok()?;
        let (mut lo, mut hi) = (0, count);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.dump.u32(functions.checked_add(mid as u64 * size)?)? <= rva {
                lo = mid + 1
            } else {
                hi = mid
            }
        }
        let mut index = lo.checked_sub(1)?;
        for _ in 0..64 {
            let begin = self.dump.u32(functions.checked_add(index as u64 * size)?)?;
            if let Some(md) = self.hash_lookup(map, base.checked_add(begin as u64)?) {
                return Some(md);
            }
            index = index.checked_sub(1)?;
        }
        None
    }
    fn hash_lookup(&self, map: u64, raw_key: u64) -> Option<u64> {
        let buckets = self.pointer(map, "HashMap", "Buckets")?;
        if buckets == 0 {
            return None;
        }
        let size = self.dump.u32(buckets)?;
        if !(2..=16_000_000).contains(&size) {
            return None;
        }
        let mask = self
            .global("HashMapValueMask")
            .unwrap_or(0x7fff_ffff_ffff_ffff);
        let slots = self.global("HashMapSlotsPerBucket").unwrap_or(4);
        let bucket_size = self.off("Bucket", "!").unwrap_or(64);
        let values = self.off("Bucket", "Values").unwrap_or(32);
        if slots > 1024 || bucket_size < slots * 8 {
            return None;
        }
        let key = if raw_key > 1 { raw_key } else { raw_key + 100 };
        let mut seed = (key >> 2) as u32;
        let increment = 1 + ((key >> 5) as u32).wrapping_add(1) % (size - 1);
        let first = buckets.checked_add(bucket_size)?;
        for _ in 0..size {
            let bucket = first.checked_add(bucket_size.checked_mul((seed % size) as u64)?)?;
            for s in 0..slots {
                if self.dump.u64(bucket.checked_add(s * 8)?) == Some(key) {
                    return Some(
                        (self
                            .dump
                            .u64(bucket.checked_add(values)?.checked_add(s * 8)?)?
                            & mask)
                            << 1,
                    );
                }
            }
            seed = seed.wrapping_add(increment);
            if self.dump.u64(bucket.checked_add(values)?)? & !mask == 0 {
                break;
            }
        }
        None
    }
    fn chunk(&self, md: u64) -> Option<u64> {
        let size = self.off("MethodDescChunk", "!")?;
        let align = self.global("MethodDescAlignment")?;
        let index = self
            .dump
            .u8(md.checked_add(self.off("MethodDesc", "ChunkIndex").unwrap_or(2))?)?;
        md.checked_sub(size)?
            .checked_sub((index as u64).checked_mul(align)?)
    }
    pub fn method_name(&self, md: u64) -> Option<String> {
        let chunk = self.chunk(md)?;
        let mt = self
            .dump
            .u64(chunk.checked_add(self.off("MethodDescChunk", "MethodTable").unwrap_or(0))?)?;
        if mt == 0 {
            return None;
        }
        let name = self.names.name(mt);
        let flags = self
            .dump
            .u16(md.checked_add(self.off("MethodDesc", "Flags").unwrap_or(6))?)
            .unwrap_or(0);
        if flags & 7 == 7
            && let Some(p) = self.pointer(md, "DynamicMethodDesc", "MethodName")
            && let Some(n) = self.dump.c_string(p, 4096)
        {
            return Some(format!("dynamic method {n}"));
        }
        let Some((_, rid)) = self.token(md) else {
            return Some(format!("{name}.?"));
        };
        Some(format!(
            "{name}.{}",
            self.names
                .method_name(mt, rid)
                .unwrap_or_else(|| format!("method #{rid}"))
        ))
    }
    fn token(&self, md: u64) -> Option<(u64, usize)> {
        let chunk = self.chunk(md)?;
        let mt = self
            .dump
            .u64(chunk.checked_add(self.off("MethodDescChunk", "MethodTable").unwrap_or(0))?)?;
        if mt == 0
            || self
                .dump
                .u16(md.checked_add(self.off("MethodDesc", "Flags").unwrap_or(6))?)
                .unwrap_or(0)
                & 7
                == 7
        {
            return None;
        }
        let bits = self
            .global("MethodDescTokenRemainderBitCount")
            .unwrap_or(12);
        if bits > 24 {
            return None;
        }
        let remainder = self.dump.u16(
            md.checked_add(
                self.off("MethodDesc", "Flags3AndTokenRemainder")
                    .unwrap_or(0),
            )?,
        )? as u32;
        let range = self.dump.u16(
            chunk.checked_add(
                self.off("MethodDescChunk", "FlagsAndTokenRange")
                    .unwrap_or(18),
            )?,
        )? as u32;
        Some((
            mt,
            (((range & (0xff_ffff >> bits)) << bits) | (remainder & (0xff_ffff >> (24 - bits))))
                as usize,
        ))
    }
}

fn decode_code_pointer(base: u64, unit: u32) -> Option<u64> {
    let low = unit & 15;
    if !(9..=12).contains(&low) {
        return None;
    }
    base.checked_add(u64::from((unit & !15) + ((low - 9) << 2)))
}

#[cfg(test)]
mod tests {
    use super::decode_code_pointer;

    #[test]
    fn code_pointer_keeps_the_high_28_address_bits() {
        for (unit, offset) in [
            (0x0000_5429, 0x5420),
            (0x0000_542a, 0x5424),
            (0x0000_542b, 0x5428),
            (0x0000_542c, 0x542c),
            (0xffff_fffc, 0xffff_fffc),
        ] {
            assert_eq!(
                decode_code_pointer(0x1000_0000, unit),
                Some(0x1000_0000 + offset)
            );
        }
        assert_eq!(decode_code_pointer(u64::MAX - 15, 0x5429), None);
        for low in [0, 8, 13, 14, 15] {
            assert_eq!(decode_code_pointer(0, 0x5420 | low), None);
        }
    }
}
