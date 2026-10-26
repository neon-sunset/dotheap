use crate::{Error, Result, heap::ManagedHeap, native_memory::Mapping};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SmapsMapping {
    pub start: u64,
    pub end: u64,
    pub path: String,
    pub rss: u64,
    pub anon_huge: u64,
    pub private_bytes: u64,
    pub swap: u64,
    pub readable: bool,
}

pub(crate) const MACH_NOTE_OWNER: &[u8; 16] = b"dotheap.resident";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedResidency {
    pub version: u32,
    pub physical_footprint: Option<u64>,
    pub mappings: Vec<SmapsMapping>,
}
fn dump_name(path: &Path) -> String {
    let mut name = path.to_string_lossy().into_owned();
    for suffix in [".zst", ".dmp"] {
        if name.ends_with(suffix) {
            name.truncate(name.len() - suffix.len());
        }
    }
    name
}
pub fn path_for_dump(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.smaps.zst", dump_name(path)))
}
pub fn next_to_dump(path: &Path) -> Option<PathBuf> {
    let name = dump_name(path);
    [".smaps.zst", ".smaps.gz", ".smaps"]
        .map(|s| PathBuf::from(format!("{name}{s}")))
        .into_iter()
        .find(|p| p.is_file())
}
pub fn dump_next_to(path: &Path) -> Option<PathBuf> {
    let text = path.to_string_lossy();
    let name = text
        .strip_suffix(".zst")
        .or_else(|| text.strip_suffix(".gz"))
        .unwrap_or(&text)
        .strip_suffix(".smaps")?;
    [".dmp.zst", ".dmp"]
        .map(|s| PathBuf::from(format!("{name}{s}")))
        .into_iter()
        .find(|p| p.is_file())
}
pub fn read(path: &Path) -> Result<Vec<SmapsMapping>> {
    let file = File::open(path).map_err(|e| Error::open(path.display(), e))?;
    let mut reader: Box<dyn Read> = match path.extension().and_then(|s| s.to_str()) {
        Some("zst") => Box::new(zstd::stream::read::Decoder::new(file)?),
        Some("gz") => Box::new(flate2::read::MultiGzDecoder::new(file)),
        _ => Box::new(file),
    };
    let mut text = String::new();
    reader
        .read_to_string(&mut text)
        .map_err(|e| Error::open(path.display(), e))?;
    if text.contains("REGION TYPE") || text.contains("==== Writable regions") {
        return Ok(parse_vmmap(&text));
    }
    parse(&text)
}
pub fn parse(text: &str) -> Result<Vec<SmapsMapping>> {
    let mut list = Vec::new();
    let mut current: Option<SmapsMapping> = None;
    for line in text.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        let Some(&first) = parts.first() else {
            continue;
        };
        if let Some((a, b)) = first.split_once('-')
            && !first.ends_with(':')
        {
            let start =
                u64::from_str_radix(a, 16).map_err(|_| Error("invalid address in smaps".into()))?;
            let end =
                u64::from_str_radix(b, 16).map_err(|_| Error("invalid address in smaps".into()))?;
            if start >= end || parts.len() < 5 {
                return Err(Error("invalid mapping in smaps".into()));
            }
            if let Some(m) = current.take() {
                list.push(m);
            }
            let mut path = parts.get(5..).unwrap_or(&[]).join(" ");
            if path.ends_with(" (deleted)") {
                path.truncate(path.len() - " (deleted)".len());
            }
            current = Some(SmapsMapping {
                start,
                end,
                path,
                rss: 0,
                anon_huge: 0,
                private_bytes: 0,
                swap: 0,
                readable: parts[1].starts_with('r'),
            });
        } else if let Some(m) = current.as_mut()
            && [
                "Rss:",
                "AnonHugePages:",
                "Private_Clean:",
                "Private_Dirty:",
                "Swap:",
            ]
            .contains(&first)
        {
            let bytes = parts
                .get(1)
                .and_then(|s| s.parse::<u64>().ok())
                .and_then(|n| n.checked_mul(1024))
                .ok_or_else(|| Error("invalid size in smaps".into()))?;
            match first {
                "Rss:" => m.rss = bytes,
                "AnonHugePages:" => m.anon_huge = bytes,
                "Swap:" => m.swap = bytes,
                _ => {
                    m.private_bytes = m
                        .private_bytes
                        .checked_add(bytes)
                        .ok_or_else(|| Error("invalid size in smaps".into()))?
                }
            }
        }
    }
    if let Some(m) = current {
        list.push(m);
    }
    Ok(list)
}
fn parse_vmmap(text: &str) -> Vec<SmapsMapping> {
    fn bytes(s: &str) -> Option<u64> {
        let (number, multiplier) = match s.as_bytes().last()? {
            b'K' => (s.get(..s.len() - 1)?, 1024.0),
            b'M' => (s.get(..s.len() - 1)?, 1048576.0),
            b'G' => (s.get(..s.len() - 1)?, 1073741824.0),
            _ => (s, 1.0),
        };
        let n = number.parse::<f64>().ok()? * multiplier;
        (n.is_finite() && n >= 0.0 && n <= u64::MAX as f64).then_some(n as u64)
    }
    text.lines()
        .filter_map(|line| {
            let (head, rest) = line.split_once('[')?;
            let (sizes, _) = rest.split_once(']')?;
            let mut head: Vec<_> = head.split_whitespace().collect();
            let (a, b) = head.pop()?.split_once('-')?;
            let start = u64::from_str_radix(a, 16).ok()?;
            let end = u64::from_str_radix(b, 16).ok()?;
            if end <= start {
                return None;
            }
            let sizes: Vec<_> = sizes.split_whitespace().collect();
            Some(SmapsMapping {
                start,
                end,
                path: format!("vmmap: {}", head.join(" ")),
                rss: bytes(sizes.get(1)?)?,
                anon_huge: 0,
                private_bytes: bytes(sizes.get(2)?)?,
                swap: bytes(sizes.get(3)?)?,
                readable: true,
            })
        })
        .collect()
}
impl SmapsMapping {
    pub fn category(&self, heap: &ManagedHeap) -> String {
        if heap.owns_memory(self.start, self.end) {
            return format!("{} pages", heap.label());
        }
        if let Some(name) = self.path.strip_prefix("vmmap: ") {
            return name.into();
        }
        if self.path.starts_with("/memfd:doublemapper") {
            return "JIT code double mapping (memfd, W^X)".into();
        }
        if self.path.starts_with("/memfd:") {
            return "other memfd mappings".into();
        }
        if self.path == "[stack]" {
            return "main stack".into();
        }
        if self.path.starts_with('/') {
            return if self.path.ends_with(".dll") {
                "files: *.dll".into()
            } else if self.path.ends_with(".so") || self.path.contains(".so.") {
                "files: *.so".into()
            } else {
                format!("files: {}", self.path.rsplit('/').next().unwrap())
            };
        }
        if self.path.starts_with('[') {
            return self.path.clone();
        }
        "anonymous (native heaps, thread stacks, loader heaps)".into()
    }
    pub fn is_anonymous(&self) -> bool {
        self.path.is_empty()
            || self.path.starts_with('[')
                && !["[vvar]", "[vdso]", "[vsyscall]", "[vvar_vclock]"]
                    .contains(&self.path.as_str())
    }
}
pub fn resident(mappings: &[SmapsMapping], from: u64, to: u64) -> f64 {
    mappings
        .iter()
        .map(|v| {
            let (lo, hi) = (v.start.max(from), v.end.min(to));
            if hi > lo {
                v.rss as f64 * (hi - lo) as f64 / (v.end - v.start) as f64
            } else {
                0.0
            }
        })
        .sum()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Residency {
    pub rss: BTreeMap<u64, u64>,
    pub swap: BTreeMap<u64, u64>,
    pub not_in_dump: u64,
    pub dump_bytes: u64,
    pub same_range: u64,
    pub anonymous_data: u64,
    pub data_not_resident: u64,
}
impl Residency {
    pub fn new(mappings: &[Mapping], smaps: &[SmapsMapping]) -> Self {
        let mut vmas: Vec<_> = smaps.iter().collect();
        vmas.sort_by_key(|v| v.start);
        let mut dump: Vec<_> = mappings.iter().collect();
        dump.sort_by_key(|m| m.start);
        let mut result = Self {
            rss: BTreeMap::new(),
            swap: BTreeMap::new(),
            not_in_dump: 0,
            dump_bytes: 0,
            same_range: 0,
            anonymous_data: 0,
            data_not_resident: 0,
        };
        let mut covered = HashSet::new();
        for m in &dump {
            let mut k = vmas.partition_point(|v| v.end <= m.start);
            let (mut r, mut s) = (0.0, 0.0);
            while k < vmas.len() && vmas[k].start < m.end {
                let v = vmas[k];
                let share =
                    (v.end.min(m.end) - v.start.max(m.start)) as f64 / (v.end - v.start) as f64;
                r += v.rss as f64 * share;
                s += v.swap as f64 * share;
                covered.insert(k);
                k += 1;
            }
            result.rss.insert(m.start, r.round() as u64);
            result.swap.insert(m.start, s.round() as u64);
        }
        result.not_in_dump = vmas
            .iter()
            .enumerate()
            .filter(|(i, _)| !covered.contains(i))
            .map(|(_, v)| v.rss)
            .sum();
        fn runs(ranges: impl Iterator<Item = (u64, u64)>) -> Vec<(u64, u64, usize, usize)> {
            let mut out: Vec<(u64, u64, usize, usize)> = Vec::new();
            for (i, (s, e)) in ranges.enumerate() {
                if let Some(last) = out.last_mut()
                    && last.1 == s
                {
                    last.1 = e;
                    last.3 = i;
                } else {
                    out.push((s, e, i, i));
                }
            }
            out
        }
        let readable: Vec<_> = vmas
            .iter()
            .filter(|v| {
                v.readable
                    && !["[vvar]", "[vdso]", "[vsyscall]", "[vvar_vclock]"]
                        .contains(&v.path.as_str())
            })
            .collect();
        let smaps_runs: BTreeMap<_, _> = runs(readable.iter().map(|v| (v.start, v.end)))
            .into_iter()
            .map(|r| (r.0, r))
            .collect();
        for (start, end, first, last) in runs(dump.iter().map(|m| (m.start, m.end))) {
            result.dump_bytes += end - start;
            let Some(&(_, v_end, v_first, v_last)) = smaps_runs.get(&start) else {
                continue;
            };
            if v_end != end {
                continue;
            }
            result.same_range += end - start;
            let list = &readable[v_first..=v_last];
            if !list.iter().all(|v| v.is_anonymous()) {
                continue;
            }
            let data: u64 = dump[first..=last].iter().map(|m| m.non_zero_bytes).sum();
            result.anonymous_data += data;
            result.data_not_resident +=
                data.saturating_sub(list.iter().map(|v| v.rss + v.swap).sum());
        }
        result
    }
    pub fn agrees(&self) -> bool {
        self.same_range as f64 >= 0.95 * self.dump_bytes as f64
            && self.data_not_resident as f64 <= 0.05 * self.anonymous_data as f64
    }
    pub fn warning(&self) -> Option<String> {
        if self.agrees() {
            return None;
        }
        let mut reasons = Vec::new();
        if (self.same_range as f64) < 0.95 * self.dump_bytes as f64 {
            reasons.push(format!(
                "{:.0}% of the dump's mappings have no VMA with the same range in it",
                100.0 * (self.dump_bytes - self.same_range) as f64 / self.dump_bytes as f64
            ));
        }
        if self.data_not_resident as f64 > 0.05 * self.anonymous_data as f64 {
            reasons.push(format!("{:.1} MB of pages with data in the dump are neither resident nor swapped out in it",self.data_not_resident as f64/1048576.0));
        }
        Some(format!(
            "smaps does not match the dump: {}. Capture smaps close to the dump. Memory totals \
                use dump estimates; Residency shows the supplied smaps.",
            reasons.join(", and ")
        ))
    }
}
