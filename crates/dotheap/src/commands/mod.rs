mod findings;
mod memory;
mod native;
mod objects;
mod resources;
mod summary;
mod tasks;

use crate::{arguments::Arguments, output::*};
use heap_core::{
    Error, Result,
    analysis::HeapSnapshot,
    ownership::{OwnershipGroup, OwnershipNode},
    type_names::property_name,
};
use std::collections::HashSet;

pub struct Context<'a> {
    pub snapshot: &'a HeapSnapshot,
    pub arguments: &'a Arguments,
}

impl Context<'_> {
    pub fn limit(&self) -> usize {
        self.arguments.limit()
    }
    pub fn core_word(&self) -> String {
        shell_word(&self.arguments.core)
    }
    pub fn positional(&self, index: usize, name: &str) -> Result<&str> {
        self.arguments
            .positional
            .get(index)
            .map(String::as_str)
            .ok_or_else(|| {
                Error(format!(
                    "missing <{name}>; see dotheap help {}",
                    self.arguments.command
                ))
            })
    }
    pub fn address(&self, index: usize) -> Result<u64> {
        let text = self.positional(index, "address")?;
        let digits = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .unwrap_or(text);
        u64::from_str_radix(digits, 16).map_err(|_| Error(format!("{text} is not a hex address")))
    }
    pub fn is_linux(&self) -> bool {
        self.snapshot.dump.platform.is_linux() && !self.snapshot.native.malloc_heaps.is_empty()
    }
    pub fn require_malloc(&self) -> Result<()> {
        if self.is_linux() {
            Ok(())
        } else {
            Err(Error(
                "malloc analysis needs a Linux core of a process that uses glibc malloc".into(),
            ))
        }
    }
    pub fn object(&self, address: u64) -> Result<usize> {
        self.snapshot
            .heap
            .object_containing(address)
            .ok_or_else(|| {
                Error(format!(
                    "{} is not inside an object: {}",
                    hex(address),
                    self.where_is(address)
                ))
            })
    }
    pub fn where_is(&self, a: u64) -> String {
        let s = self.snapshot;
        let core = self.core_word();
        if let Some(i) = s.heap.object_containing(a) {
            return format!(
                "it is in a {} object; see dotheap object {core} {}",
                s.type_name(i),
                hex(a)
            );
        }
        if let Some(i) = s.heap.region_containing(a) {
            let r = &s.heap.regions[i];
            return format!(
                "it is in the {} region at {} but not in an object (region header, free space or \
                    above m_used); see dotheap regions {core} {}",
                r.generation_name,
                hex(r.start),
                hex(a)
            );
        }
        if self.is_linux() && s.native.malloc_chunk(a, &s.dump).is_some() {
            return format!(
                "it is in a malloc chunk; see dotheap chunk {core} {}",
                hex(a)
            );
        }
        if let Some(m) = s.native.mappings.iter().find(|m| a >= m.start && a < m.end) {
            return format!(
                "it is in {}{}",
                m.category,
                if m.detail.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", m.detail)
                }
            );
        }
        "the dump has no memory there".into()
    }
    pub fn row(&self, i: usize, with_text: bool) -> Value {
        let s = self.snapshot;
        let mut row = record([
            ("address", address(s.heap.addresses[i])),
            ("size", bytes(s.heap.size(i))),
            ("retained", bytes(s.graph.retained[i])),
            ("state", text(s.graph.state(i))),
            ("type", text(s.type_name(i))),
        ]);
        if with_text {
            row.add(
                "text",
                optional(s.identifying_text(i).map(|s| prefix(&s, 80))),
            );
        }
        row
    }
    pub fn matching_types(&self, query: &str) -> HashSet<u32> {
        let stats = &self.snapshot.type_stats;
        for rank in 0..4 {
            let found: HashSet<_> = stats
                .iter()
                .filter(|t| match rank {
                    0 => t.name == query,
                    1 => t.name.ends_with(&format!(".{query}")),
                    2 => t.name.contains(query),
                    _ => contains(&t.name, query),
                })
                .map(|t| t.type_id)
                .collect();
            if !found.is_empty() {
                return found;
            }
        }
        HashSet::new()
    }
    pub fn ownership_address(&self, node: usize) -> Result<Option<u64>> {
        Ok(match self.snapshot.ownership()?.node(node) {
            OwnershipNode::Object(i) => Some(self.snapshot.heap.addresses[i]),
            OwnershipNode::Chunk(k) => Some(self.snapshot.native_graph()?.chunks[k].user_start()),
            _ => None,
        })
    }
    pub fn ownership_row(&self, group: &OwnershipGroup, parent: usize) -> Result<Value> {
        let s = self.snapshot;
        let ownership = s.ownership()?;
        let mut row = record([
            ("retained", bytes(group.total())),
            ("managed", bytes(group.managed)),
            ("native", bytes(group.native)),
            ("count", int(group.nodes.len())),
            (
                "example",
                optional_address(
                    group
                        .nodes
                        .first()
                        .map(|&v| self.ownership_address(v))
                        .transpose()?
                        .flatten(),
                ),
            ),
        ]);
        if group.nodes.len() == 1
            && let OwnershipNode::Object(i) = ownership.node(group.nodes[0])
        {
            if let OwnershipNode::Object(p) = ownership.node(parent) {
                row.add(
                    "field",
                    optional(s.reference_name(p, i)?.map(|n| property_name(&n))),
                );
            }
            row.add(
                "text",
                optional(s.identifying_text(i).map(|s| prefix(&s, 60))),
            );
        }
        row.add("name", text(&group.name));
        Ok(row)
    }
    pub fn ownership_path(&self, path: &[usize]) -> Result<Value> {
        let s = self.snapshot;
        let graph = s.ownership()?;
        try_limited(path.iter().enumerate().skip(1), usize::MAX, |(k, &node)| {
            let mut row = record([("address", optional_address(self.ownership_address(node)?))]);
            if k > 1
                && let (OwnershipNode::Object(p), OwnershipNode::Object(i)) =
                    (graph.node(path[k - 1]), graph.node(node))
            {
                row.add(
                    "field",
                    optional(s.reference_name(p, i)?.map(|n| property_name(&n))),
                );
            }
            row.add(
                "kind",
                text(match graph.node(node) {
                    OwnershipNode::Object(_) => "object",
                    OwnershipNode::Chunk(_) => "native",
                    _ => "root",
                }),
            );
            row.add("name", text(s.ownership_name(node)?));
            Ok(row)
        })
    }
}

pub fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || "_@%+=:,./-".contains(ch))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}
fn prefix(value: &str, count: usize) -> String {
    value.chars().take(count).collect()
}
fn contains(value: &str, query: &str) -> bool {
    value.to_lowercase().contains(&query.to_lowercase())
}
fn enum_text(value: impl serde::Serialize) -> String {
    serde_json::to_value(value)
        .expect("serializable enum")
        .as_str()
        .expect("string enum")
        .to_owned()
}

pub fn run(c: &Context<'_>) -> Result<Value> {
    let s = c.snapshot;
    match c.arguments.command.as_str() {
        "summary" => summary::summary(c),
        "types" => objects::types(c),
        "instances" => objects::instances(c),
        "object" => objects::object(c),
        "retained" => objects::retained(c),
        "handles" => objects::handles(c),
        "regions" => memory::regions(c),
        "threads" => tasks::threads(c),
        "stacks" => tasks::stacks(c),
        "native" => native::native(c),
        "chunk" => native::chunk(c),
        "bytes" => native::read_bytes(c),
        "find" => native::find(c),
        "memory" => memory::memory(c),
        "garbage" => memory::garbage(c),
        "fragmentation" => memory::fragmentation(c),
        "strings" | "collections" | "finalizable" | "zero-arrays" | "boxed" => {
            findings::heap_check(c)
        }
        "search" => findings::search(c),
        "findings" => findings::findings(c),
        "tasks" => tasks::tasks(c),
        "network" => resources::network(c),
        "files" => resources::files(c),
        "symbols" => resources::symbols(c),
        "async" => Ok(findings::report(&s.async_analysis()?.report, c)),
        name => {
            let report = match name {
                "arraypool" => s.array_pool_check()?,
                "large-objects" => s.large_object_check()?,
                "cancellation" => s.cancellation_check()?,
                "events" => s.event_check()?,
                "timers" => s.timer_check()?,
                "locks" => s.lock_check()?,
                "blocked" => s.blocked_threads_check()?,
                "connections" => s.connections_check()?,
                "exceptions" => s.exceptions_check()?,
                "instance-counts" => s.instance_count_check()?,
                "finalizer-backlog" => s.finalizer_backlog_check()?,
                "native-held-by-dead" => s.native_held_by_dead_check()?,
                "malloc-arenas" => s.malloc_arena_check()?,
                _ => return Err(Error(format!("unknown command {name}"))),
            };
            Ok(findings::report(&report, c))
        }
    }
}
