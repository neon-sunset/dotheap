use crate::{
    Result,
    analysis::HeapSnapshot,
    queries::FieldValue,
    type_names::{property_name, short},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Cell {
    Text(String),
    Int(i64),
    Bytes(u64),
    Address(u64),
    None,
}
impl Cell {
    pub fn int(value: usize) -> Self {
        Self::Int(value as i64)
    }
    pub fn text(value: impl Into<String>) -> Self {
        Self::Text(value.into())
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckSection {
    pub title: String,
    pub explanation: String,
    pub columns: Vec<String>,
    pub rows: Vec<CheckRow>,
    pub omitted: usize,
}
impl CheckSection {
    pub fn new(title: &str, explanation: &str, columns: &[&str]) -> Self {
        Self {
            title: title.into(),
            explanation: explanation.into(),
            columns: columns.iter().map(|s| (*s).into()).collect(),
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckRow {
    pub cells: Vec<Cell>,
    pub samples: Vec<usize>,
    pub children: Vec<CheckRow>,
}
impl CheckRow {
    pub fn new(cells: Vec<Cell>, samples: Vec<usize>) -> Self {
        Self {
            cells,
            samples,
            children: vec![],
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Finding {
    pub bytes: u64,
    pub text: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SummaryFinding {
    pub bytes: u64,
    pub text: String,
    pub see: Option<String>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct CheckReport {
    pub name: String,
    pub title: String,
    pub explanation: String,
    pub sections: Vec<CheckSection>,
    pub findings: Vec<Finding>,
}
impl CheckReport {
    pub fn new(name: &str, title: &str, explanation: &str) -> Self {
        Self {
            name: name.into(),
            title: title.into(),
            explanation: explanation.into(),
            ..Default::default()
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Owner {
    pub object: i64,
    pub field: String,
}
pub const TIMER_QUEUE_TYPES: [&str; 3] = [
    "System.Threading.TimerQueueTimer",
    "System.Threading.TimerQueue",
    "System.Threading.TimerQueue[]",
];
pub const SOCKET_ENGINE_TYPES: [&str; 2] = [
    "System.Net.Sockets.SocketAsyncContext",
    "System.Net.Sockets.SocketAsyncContext[]",
];
pub const BLOCKING_FRAMES: [&str; 11] = [
    "Task.Wait",
    "Task.InternalWait",
    "Task.SpinThenBlockingWait",
    "Task.WaitAll",
    "Task`1.GetResultCore",
    "TaskAwaiter.HandleNonSuccessAndDebuggerNotification",
    "ManualResetEventSlim.Wait",
    "SemaphoreSlim.Wait",
    "Monitor.Wait",
    "WaitHandle.WaitOne",
    "Thread.Join",
];
pub fn pool_bucket_length(n: usize) -> bool {
    (16..=16usize << 26).contains(&n) && n.is_power_of_two()
}

pub fn format_size(bytes: u64) -> String {
    let value = bytes as f64;
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1 << 20 {
        format!("{:.1} KB", value / 1024.0)
    } else if bytes < 1 << 30 {
        format!("{:.1} MB", value / 1_048_576.0)
    } else {
        format!("{:.2} GB", value / 1_073_741_824.0)
    }
}

pub(crate) fn counted_names(
    counts: &std::collections::BTreeMap<String, usize>,
    limit: usize,
) -> String {
    let mut names: Vec<_> = counts.iter().collect();
    names.sort_by(|(a, ac), (b, bc)| bc.cmp(ac).then_with(|| a.cmp(b)));
    names
        .into_iter()
        .take(limit)
        .map(|(name, count)| format!("{name} ×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

impl HeapSnapshot {
    pub fn all_checks(&self) -> Result<Vec<Arc<CheckReport>>> {
        Ok(vec![
            self.verification_check()?,
            Arc::new(self.async_analysis()?.report.clone()),
            self.array_pool_check()?,
            self.large_object_check()?,
            self.cancellation_check()?,
            self.event_check()?,
            self.timer_check()?,
            self.blocked_threads_check()?,
            self.lock_check()?,
            self.exceptions_check()?,
            self.connections_check()?,
            self.instance_count_check()?,
            self.finalizer_backlog_check()?,
            self.native_held_by_dead_check()?,
            self.malloc_arena_check()?,
        ])
    }

    pub fn summary_findings(&self) -> Result<Arc<Vec<SummaryFinding>>> {
        use crate::{native_graph::Reach, ownership::OwnershipNode, queries::hex};
        let compute = || {
            let mut findings = Vec::new();
            let totals = self.totals();
            let ownership = self.ownership()?;
            let held = ownership.retained(0);
            if let Some(group) = self
                .ownership_groups(0)?
                .first()
                .filter(|g| g.total() > held / 4)
                && let Some(&first) = group.nodes.first()
            {
                let address = match ownership.node(first) {
                    OwnershipNode::Object(i) => Some(self.heap.addresses[i]),
                    OwnershipNode::Chunk(c) => Some(self.native_graph()?.chunks[c].user_start()),
                    _ => None,
                };
                if let Some(address) = address {
                    let (what, verb) = if group.nodes.len() == 1 {
                        (format!("one {} ({})", group.name, hex(address)), "keeps")
                    } else {
                        (format!("{} × {}", group.nodes.len(), group.name), "keep")
                    };
                    let percent = (100.0 * group.total() as f64 / held.max(1) as f64) as u64;
                    findings.push(SummaryFinding {
                        bytes: group.total(),
                        text: format!(
                            "{what} {verb} {} alive ({} managed, {} native), \
                            {percent}% of the live managed and native memory",
                            format_size(group.total()),
                            format_size(group.managed),
                            format_size(group.native)
                        ),
                        see: Some("retained".into()),
                    });
                }
            }
            if totals.dead() > totals.committed / 5 {
                let percent =
                    (100.0 * totals.dead() as f64 / totals.committed.max(1) as f64) as u64;
                findings.push(SummaryFinding {
                    bytes: totals.dead(),
                    text: format!(
                        "{} of dead objects are not swept yet ({percent}% of the committed heap)",
                        format_size(totals.dead())
                    ),
                    see: Some("garbage".into()),
                });
            }
            if totals.committed.saturating_sub(totals.live) > totals.live
                && totals.committed - totals.live > 64 << 20
            {
                findings.push(SummaryFinding {
                    bytes: totals.committed - totals.live,
                    text: format!(
                        "the heap commits {} for {} of live objects",
                        format_size(totals.committed),
                        format_size(totals.live)
                    ),
                    see: Some("fragmentation".into()),
                });
            }
            if self.dump.platform.is_linux()
                && !self.native.malloc_heaps.is_empty()
                && let Some(total) = self
                    .native_graph()?
                    .totals()
                    .iter()
                    .find(|t| t.reach == Reach::Unreferenced)
                && total.bytes > 1 << 20
            {
                findings.push(SummaryFinding {
                    bytes: total.bytes,
                    text: format!("{} of malloc chunks are in use but no pointer in the dump reaches them (leak candidates)", format_size(total.bytes)),
                    see: Some("native --reach unreferenced".into()),
                });
            }
            if self.graph.pending_count > 0 {
                findings.push(SummaryFinding {
                    bytes: self.graph.pending_bytes,
                    text: format!(
                        "{} objects ({}) are pending finalization",
                        self.graph.pending_count,
                        format_size(self.graph.pending_bytes)
                    ),
                    see: Some("finalizable".into()),
                });
            }
            for check in self.all_checks()? {
                findings.extend(check.findings.iter().map(|f| SummaryFinding {
                    bytes: f.bytes,
                    text: f.text.clone(),
                    see: Some(check.name.clone()),
                }));
            }
            findings.sort_by_key(|f| {
                (
                    f.see.as_deref() != Some("verify"),
                    std::cmp::Reverse(f.bytes),
                )
            });
            Ok(findings)
        };
        self.saved("summary findings", compute)
    }

    pub fn field(&self, i: usize, name: &str) -> Option<FieldValue> {
        self.field_values(i)
            .into_iter()
            .find(|f| f.field.name == name)
    }
    pub fn target(&self, i: impl Into<Option<usize>>, name: &str) -> Option<usize> {
        self.field(i.into()?, name)?.target
    }
    pub fn integer_field(&self, f: Option<&FieldValue>) -> Option<i64> {
        let f = f?;
        match f.field.primitive_size()? {
            1 => self.dump.u8(f.address).map(i64::from),
            2 => self.dump.u16(f.address).map(i64::from),
            4 => self.dump.u32(f.address).map(|v| v as i32 as i64),
            _ => self.dump.u64(f.address).map(|v| v as i64),
        }
    }
    pub fn integer(&self, i: impl Into<Option<usize>>, name: &str) -> Option<i64> {
        self.integer_field(self.field(i.into()?, name).as_ref())
    }
    pub fn elements(&self, array: Option<usize>) -> Vec<Option<usize>> {
        array
            .and_then(|i| self.reference_elements(i, 1 << 24))
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.target)
            .collect()
    }
    pub fn objects(&self, types: &HashSet<u32>) -> Vec<usize> {
        if types.is_empty() {
            return vec![];
        }
        (0..self.heap.count())
            .filter(|&i| types.contains(&self.heap.type_ids[i]))
            .collect()
    }
    pub fn type_ids(&self, predicate: impl Fn(&str) -> bool) -> HashSet<u32> {
        self.type_stats
            .iter()
            .filter(|t| t.count > 0 && predicate(&t.name))
            .map(|t| t.type_id)
            .collect()
    }
    pub fn types_with_field(&self, name: &str) -> HashSet<u32> {
        self.type_stats
            .iter()
            .filter(|t| {
                t.count > 0
                    && self
                        .names
                        .fields(self.heap.types[t.type_id as usize].address)
                        .iter()
                        .any(|f| f.name == name)
            })
            .map(|t| t.type_id)
            .collect()
    }
    pub fn delegate_method(&self, d: usize) -> Result<Option<String>> {
        let code = self.code_map()?;
        for name in ["_methodPtrAux", "_methodPtr"] {
            if let Some(p) = self.integer(d, name).filter(|&p| p != 0)
                && let Some(l) = code.locate(p as u64)
            {
                return Ok(Some(l.name));
            }
        }
        Ok(None)
    }
    pub fn delegate_method_with_source(&self, d: usize) -> Result<Option<String>> {
        let code = self.code_map()?;
        for name in ["_methodPtrAux", "_methodPtr"] {
            if let Some(p) = self.integer(d, name).filter(|&p| p != 0)
                && let Some(l) = code.locate(p as u64)
            {
                return Ok(Some(format!(
                    "{}{}",
                    l.name,
                    code.source(p as u64)
                        .map(|s| format!(" ({s})"))
                        .unwrap_or_default()
                )));
            }
        }
        Ok(None)
    }
    pub fn holder(&self, i: usize) -> Result<String> {
        let live: Vec<_> = self
            .graph
            .referrer_rows
            .row(i)
            .iter()
            .map(|&r| r as usize)
            .filter(|&r| self.graph.live[r] != 0)
            .collect();
        for &r in &live {
            if let Some(name) = self
                .reference_name(r, i)?
                .filter(|n| n.starts_with("static "))
            {
                return Ok(name);
            }
        }
        if let Some(&r) = live.first() {
            return Ok(format!(
                "{}{}",
                short(&self.type_name(r)),
                self.reference_name(r, i)?
                    .as_deref()
                    .map(property_name)
                    .unwrap_or_default()
            ));
        }
        if let Some(&s) = self.graph.sources_of.row(i).first() {
            return self.describe_source(s as usize);
        }
        Ok("-".into())
    }
    pub fn owning_field(&self, i: usize) -> Result<String> {
        self.owning_field_with(i, short)
    }
    pub fn owning_field_with(&self, i: usize, name: impl Fn(&str) -> String) -> Result<String> {
        let mut child = i;
        for _ in 0..64 {
            let p = self.graph.parent[child];
            if p < 0 {
                return if p == i32::MIN {
                    Ok("-".into())
                } else {
                    self.describe_source((-(p + 1)) as usize)
                };
            }
            let r = p as usize;
            let field = self.reference_name(r, child)?;
            if let Some(field) = field.as_ref().filter(|f| f.starts_with("static ")) {
                return Ok(property_name(field));
            }
            let t = self.type_name(r);
            if !t.ends_with(']') && !t.starts_with("System.Collections.") {
                return Ok(name(&t) + &field.as_deref().map(property_name).unwrap_or_default());
            }
            child = r;
        }
        self.holder(i)
    }
    pub fn owner(
        &self,
        o: usize,
        limit: usize,
        skip: impl Fn(&str) -> bool,
    ) -> Result<Option<Owner>> {
        let mut queue = vec![o];
        let mut seen = HashSet::from([o]);
        let mut k = 0;
        while k < queue.len() && k < limit {
            let x = queue[k];
            k += 1;
            let parts: HashSet<_> = self.graph.references(x).into_iter().collect();
            for &r in self.graph.referrer_rows.row(x) {
                let r = r as usize;
                if self.graph.live[r] == 0 || parts.contains(&(r as i32)) || !seen.insert(r) {
                    continue;
                }
                let field = self.reference_name(r, x)?;
                if let Some(field) = field.as_ref().filter(|f| f.starts_with("static ")) {
                    return Ok(Some(Owner {
                        object: x as i64,
                        field: field.clone(),
                    }));
                }
                let t = self.type_name(r);
                if skip(&t) || t.ends_with(']') || t.starts_with("System.Collections.") {
                    queue.push(r);
                    continue;
                }
                return Ok(Some(Owner {
                    object: r as i64,
                    field: field.as_deref().map(property_name).unwrap_or_default(),
                }));
            }
        }
        if let Some(&s) = self.graph.sources_of.row(o).first() {
            return Ok(Some(Owner {
                object: -1 - s as i64,
                field: self
                    .stack_holder_text(o)?
                    .unwrap_or(self.describe_source(s as usize)?),
            }));
        }
        Ok(None)
    }
    pub fn path_avoiding(
        &self,
        o: usize,
        avoid: &HashSet<u32>,
    ) -> Option<(Option<usize>, Vec<usize>)> {
        if avoid.is_empty() {
            return Some(self.graph.path_to_root(o));
        }
        let mut toward = HashMap::from([(o, None)]);
        let mut queue = vec![o];
        let mut k = 0;
        while k < queue.len() && k < 500_000 {
            let x = queue[k];
            k += 1;
            if let Some(&s) = self.graph.sources_of.row(x).first() {
                let mut chain = vec![x];
                let mut c = toward[&x];
                while let Some(i) = c {
                    chain.push(i);
                    c = toward[&i];
                }
                return Some((Some(s as usize), chain));
            }
            for &r in self.graph.referrer_rows.row(x) {
                let r = r as usize;
                if self.graph.live[r] != 0
                    && !toward.contains_key(&r)
                    && !avoid.contains(&self.heap.type_ids[r])
                {
                    toward.insert(r, Some(x));
                    queue.push(r);
                }
            }
        }
        None
    }
    pub fn holder_on_root_path(
        &self,
        o: usize,
        avoid: &[&str],
        skip: impl Fn(&str) -> bool,
    ) -> Result<Option<Owner>> {
        let mut types = self.type_ids(|s| avoid.contains(&s));
        types.extend(self.types_with_field("_methodPtr"));
        let Some((source, objects)) = self.path_avoiding(o, &types) else {
            return Ok(None);
        };
        if objects.len() > 1 {
            for k in (0..objects.len() - 1).rev() {
                let (r, x) = (objects[k], objects[k + 1]);
                let field = self.reference_name(r, x)?;
                if let Some(field) = field.as_ref().filter(|f| f.starts_with("static ")) {
                    return Ok(Some(Owner {
                        object: x as i64,
                        field: field.clone(),
                    }));
                }
                let t = self.type_name(r);
                if skip(&t) || t.ends_with(']') || t.starts_with("System.Collections.") {
                    continue;
                }
                return Ok(Some(Owner {
                    object: r as i64,
                    field: field.as_deref().map(property_name).unwrap_or_default(),
                }));
            }
        }
        source
            .map(|s| {
                Ok(Owner {
                    object: -1 - s as i64,
                    field: self
                        .stack_holder_text(objects.first().copied().unwrap_or(o))?
                        .unwrap_or(self.describe_source(s)?),
                })
            })
            .transpose()
    }
}
