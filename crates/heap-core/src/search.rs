use crate::{Result, analysis::HeapSnapshot, checks::Cell, native_graph::ChunkGroup, queries::hex};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SearchKind {
    Address,
    #[serde(rename = "Findings")]
    Finding,
    #[serde(rename = "Report rows")]
    CheckRow,
    #[serde(rename = "Types")]
    Type,
    #[serde(rename = "Native memory")]
    Native,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SearchTarget {
    Finding(Option<String>),
    Check(String),
    Type(u32),
    NativeGroup(String),
    Object(usize),
    Region(usize),
    Chunk(usize),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SearchResult {
    pub kind: SearchKind,
    pub title: String,
    pub detail: String,
    pub target: SearchTarget,
    pub bytes: u64,
}

impl HeapSnapshot {
    pub fn chunk_groups(&self) -> Result<Arc<Vec<ChunkGroup>>> {
        self.saved("native groups v5", || self.native_graph()?.groups(self))
    }

    pub fn search(&self, query: &str, limit_per_kind: usize) -> Result<Vec<SearchResult>> {
        let query = query.trim().to_lowercase();
        if query.is_empty() || limit_per_kind == 0 {
            return Ok(Vec::new());
        }
        let rank = |text: &str| match_rank(text, &query);
        let mut results = Vec::new();
        if let Some(address) = parse_address(&query) {
            if let Some(object) = self.heap.object_containing(address) {
                results.push((
                    0,
                    SearchResult {
                        kind: SearchKind::Address,
                        title: format!(
                            "{}  {}",
                            hex(self.heap.addresses[object]),
                            self.type_name(object)
                        ),
                        detail: if self.graph.live[object] != 0 {
                            "live object"
                        } else {
                            "dead object"
                        }
                        .into(),
                        target: SearchTarget::Object(object),
                        bytes: 0,
                    },
                ));
            }
            if let Some(index) = self.heap.region_containing(address) {
                let region = &self.heap.regions[index];
                results.push((
                    0,
                    SearchResult {
                        kind: SearchKind::Address,
                        title: format!("region {}", hex(region.start)),
                        detail: format!("{}, {}", region.generation_name, region.queue),
                        target: SearchTarget::Region(index),
                        bytes: 0,
                    },
                ));
            }
            if self.dump.platform.is_linux() {
                let graph = self.native_graph()?;
                if let Some(chunk) = graph.chunk(address) {
                    results.push((
                        0,
                        SearchResult {
                            kind: SearchKind::Address,
                            title: format!(
                                "{}  {}",
                                hex(graph.chunks[chunk].user_start()),
                                graph.group_label(chunk, self)?
                            ),
                            detail: graph.reach[chunk].title().into(),
                            target: SearchTarget::Chunk(chunk),
                            bytes: 0,
                        },
                    ));
                }
            }
        }
        for finding in self.summary_findings()?.iter() {
            if let Some(rank) = rank(&finding.text) {
                results.push((
                    rank,
                    SearchResult {
                        kind: SearchKind::Finding,
                        title: finding.text.clone(),
                        detail: String::new(),
                        target: SearchTarget::Finding(finding.see.clone()),
                        bytes: finding.bytes,
                    },
                ));
            }
        }
        for report in self.all_checks()? {
            if let Some(rank) = rank(&report.title).or_else(|| rank(&report.name)) {
                results.push((
                    rank,
                    SearchResult {
                        kind: SearchKind::CheckRow,
                        title: report.title.clone(),
                        detail: report.explanation.clone(),
                        target: SearchTarget::Check(report.name.clone()),
                        bytes: 0,
                    },
                ));
            }
            let mut seen = HashSet::new();
            for section in &report.sections {
                let mut rows: Vec<_> = section.rows.iter().rev().collect();
                while let Some(row) = rows.pop() {
                    rows.extend(row.children.iter().rev());
                    let texts: Vec<_> = row
                        .cells
                        .iter()
                        .filter_map(|c| {
                            if let Cell::Text(text) = c {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect();
                    let Some(best) = texts.iter().filter_map(|t| rank(t)).min() else {
                        continue;
                    };
                    let title = texts.join("  ·  ");
                    if !seen.insert(title.clone()) {
                        continue;
                    }
                    let bytes = row
                        .cells
                        .iter()
                        .filter_map(|c| {
                            if let Cell::Bytes(bytes) = c {
                                Some(*bytes)
                            } else {
                                None
                            }
                        })
                        .max()
                        .unwrap_or(0);
                    results.push((
                        best,
                        SearchResult {
                            kind: SearchKind::CheckRow,
                            title,
                            detail: format!("{} › {}", report.title, section.title),
                            target: SearchTarget::Check(report.name.clone()),
                            bytes,
                        },
                    ));
                }
            }
        }
        for ty in self.type_stats.iter().filter(|t| t.count > 0) {
            let head = ty.name.split(['<', '[', '`']).next().unwrap_or(&ty.name);
            let simple = head
                .rfind('.')
                .map(|i| &ty.name[i + 1..])
                .unwrap_or(&ty.name);
            if let Some(rank) = [rank(&ty.name), rank(simple)].into_iter().flatten().min() {
                results.push((
                    rank,
                    SearchResult {
                        kind: SearchKind::Type,
                        title: ty.name.clone(),
                        detail: format!("{} objects, {} live", ty.count, ty.live_count),
                        target: SearchTarget::Type(ty.type_id),
                        bytes: ty.retained,
                    },
                ));
            }
        }
        if self.dump.platform.is_linux() {
            for group in self.chunk_groups()?.iter() {
                if let Some(rank) = rank(&group.label)
                    .or_else(|| group.library.as_deref().and_then(rank).map(|r| r + 1))
                {
                    results.push((
                        rank,
                        SearchResult {
                            kind: SearchKind::Native,
                            title: group.label.clone(),
                            detail: format!(
                                "{}{}; {} chunks, held by {}",
                                group
                                    .library
                                    .as_ref()
                                    .map(|s| format!("{s}, "))
                                    .unwrap_or_default(),
                                group.kind.title(),
                                group.chunks.len(),
                                group.reach.holder()
                            ),
                            target: SearchTarget::NativeGroup(group.id.clone()),
                            bytes: group.bytes,
                        },
                    ));
                }
            }
        }
        results.sort_by_key(|(rank, result)| (result.kind, *rank, std::cmp::Reverse(result.bytes)));
        let mut counts = BTreeMap::new();
        Ok(results
            .into_iter()
            .filter_map(|(_, result)| {
                let count = counts.entry(result.kind).or_insert(0);
                *count += 1;
                (*count <= limit_per_kind).then_some(result)
            })
            .collect())
    }
}

pub fn parse_address(query: &str) -> Option<u64> {
    let lower = query.to_ascii_lowercase();
    let explicit = lower.starts_with("0x");
    let digits = if explicit { &lower[2..] } else { &lower };
    if digits.is_empty()
        || (!explicit && digits.len() < 8)
        || !digits.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

fn match_rank(text: &str, query: &str) -> Option<u8> {
    let text = text.to_lowercase();
    let index = text.find(query)?;
    Some(if text == query {
        0
    } else if index == 0 {
        1
    } else if text[..index]
        .chars()
        .next_back()
        .is_some_and(char::is_alphanumeric)
    {
        3
    } else {
        2
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_require_a_prefix_or_at_least_eight_hex_digits() {
        assert_eq!(parse_address("0x10"), Some(16));
        assert_eq!(parse_address("0XABCDEF12"), Some(0xabcdef12));
        assert_eq!(parse_address("abcdef12"), Some(0xabcdef12));
        for text in [
            "dead",
            "0x",
            "0x+1",
            "-1234567",
            "10000000000000000",
            "1234567z",
        ] {
            assert_eq!(parse_address(text), None, "{text}");
        }
    }

    #[test]
    fn search_prefers_whole_names_and_word_boundaries() {
        assert_eq!(match_rank("Exception", "exception"), Some(0));
        assert_eq!(match_rank("Exception[]", "exception"), Some(1));
        assert_eq!(match_rank("System.Exception", "exception"), Some(2));
        assert_eq!(
            match_rank("InvalidOperationException", "exception"),
            Some(3)
        );
        assert_eq!(match_rank("Événement.Handler", "handler"), Some(2));
        assert_eq!(match_rank("Événement", "événement"), Some(0));
        assert_eq!(match_rank("System.Object", "exception"), None);
    }
}
