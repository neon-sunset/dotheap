use crate::{
    Result,
    analysis::HeapSnapshot,
    bytes::ReadBytes,
    code_map::{CodeMap, LocationKind},
    graph::RootKind,
    progress::Progress,
};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[repr(C)]
pub struct Frame {
    pub slot: u64,
    pub return_address: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadInfo {
    pub tid: i32,
    pub name: Option<String>,
    pub managed_thread: Option<usize>,
    pub stack_pointer: u64,
    pub instruction_pointer: u64,
    pub stack_start: u64,
    pub stack_end: u64,
    pub deepest_touched: u64,
    pub frames: Vec<Frame>,
}
impl ThreadInfo {
    pub fn in_use_bytes(&self) -> u64 {
        self.stack_end.saturating_sub(self.stack_pointer)
    }
    pub fn touched_bytes(&self) -> u64 {
        self.stack_end.saturating_sub(self.deepest_touched)
    }
}
pub struct ThreadStacks {
    pub threads: Vec<ThreadInfo>,
    pub code: Arc<CodeMap>,
}
impl HeapSnapshot {
    pub fn thread_stacks(&self) -> Result<Arc<ThreadStacks>> {
        self.memo("thread stacks", || ThreadStacks::new(self))
    }
    pub fn thread_stacks_with_progress(
        &self,
        progress: impl Fn(f64) + Sync,
    ) -> Result<Arc<ThreadStacks>> {
        let update = |_: &str, fraction| progress(fraction);
        let progress = Progress::new(&update);
        progress.report(0.0);
        let stacks = self.memo("thread stacks", || {
            ThreadStacks::new_with_progress(self, progress)
        })?;
        progress.report(1.0);
        Ok(stacks)
    }
    pub fn describe_source(&self, s: usize) -> Result<String> {
        let source = &self.graph.sources[s];
        Ok(match source.kind {
            RootKind::Stack { tid } => format!(
                "stack of {} (0x{:x}-0x{:x})",
                self.thread_stacks()?.label(tid),
                source.start,
                source.end
            ),
            RootKind::Registers { tid } => {
                format!("registers of {}", self.thread_stacks()?.label(tid))
            }
            RootKind::ThreadStatics { tid } => {
                format!("thread statics of {}", self.thread_stacks()?.label(tid))
            }
            _ => source.description(),
        })
    }
    pub fn stack_holder(&self, i: usize) -> Result<Option<(String, String)>> {
        let Some(slot) = self.graph.root_slot(i).filter(|&s| s != 0) else {
            return Ok(None);
        };
        let stacks = self.thread_stacks()?;
        Ok(stacks
            .threads
            .iter()
            .find(|t| slot >= t.stack_pointer && slot < t.stack_end)
            .map(|t| (stacks.describe(stacks.owner(slot, t)), stacks.label(t.tid))))
    }
    pub fn stack_holder_text(&self, i: usize) -> Result<Option<String>> {
        Ok(self
            .stack_holder(i)?
            .map(|(function, thread)| format!("a local of {function} on {thread}")))
    }
}
impl ThreadStacks {
    pub fn new(s: &HeapSnapshot) -> Result<Self> {
        Self::new_with_progress(s, Progress::none())
    }

    fn new_with_progress(s: &HeapSnapshot, progress: Progress<'_>) -> Result<Self> {
        let dump = &s.dump;
        let code = s.code_map()?;
        progress.report(0.1);
        let mut runtime_threads = HashMap::new();
        if let Some(runtime) = &s.runtime
            && let Some(d) = &runtime.descriptor
            && let Some(handle_field) = d.offset("Thread", "GCHandle")
        {
            for (thread, os_id) in runtime.threads(dump) {
                if os_id <= 0 {
                    continue;
                }
                let frame = d
                    .offset("Thread", "Frame")
                    .and_then(|o| thread.checked_add(o))
                    .and_then(|a| dump.u64(a))
                    .unwrap_or(0);
                let tid = if dump.platform.is_linux() {
                    Some(os_id)
                } else {
                    dump.threads
                        .iter()
                        .find(|t| {
                            dump.segment_index(t.stack_pointer).is_some_and(|i| {
                                frame >= t.stack_pointer && frame < dump.segments[i].end()
                            })
                        })
                        .map(|t| t.tid)
                };
                if let Some(tid) = tid {
                    let object = thread
                        .checked_add(handle_field)
                        .and_then(|a| dump.u64(a))
                        .and_then(|h| dump.u64(h))
                        .and_then(|t| s.heap.object_at(t));
                    let name = object
                        .and_then(|o| {
                            s.field_values(o)
                                .iter()
                                .find(|f| f.field.name == "_name")
                                .and_then(|f| f.target)
                        })
                        .and_then(|t| s.string_value(t, 4096));
                    runtime_threads.insert(tid, (object, name));
                }
            }
        }
        let mut threads = Vec::new();
        for (index, t) in dump.threads.iter().enumerate() {
            let thread_progress = progress.portion(0.2, 1.0).portion(
                index as f64 / dump.threads.len() as f64,
                (index + 1) as f64 / dump.threads.len() as f64,
            );
            thread_progress.report(0.0);
            let (object, name) = runtime_threads.remove(&t.tid).unwrap_or_default();
            let mut info = ThreadInfo {
                tid: t.tid,
                name,
                managed_thread: object,
                stack_pointer: t.stack_pointer,
                instruction_pointer: t.instruction_pointer,
                stack_start: 0,
                stack_end: 0,
                deepest_touched: 0,
                frames: vec![],
            };
            if let Some(i) = dump.segment_index(t.stack_pointer) {
                let seg = &dump.segments[i];
                info.stack_start = seg.address;
                info.stack_end = seg.end();
                let mut deepest = None;
                dump.scan(seg.address, seg.end(), |a, bytes| {
                    if deepest.is_none()
                        && let Some(k) = bytes.iter().position(|&b| b != 0)
                    {
                        deepest = Some(seg.address + ((a - seg.address + k as u64) / 4096) * 4096);
                    }
                    thread_progress
                        .portion(0.0, 0.4)
                        .items((a - seg.address) as usize + bytes.len(), seg.size as usize);
                });
                info.deepest_touched = deepest.unwrap_or(seg.end()).min(t.stack_pointer);
                let start = t.stack_pointer & !7;
                if start >= seg.address && start < seg.end() {
                    dump.scan(start, start + (seg.end() - start) / 8 * 8, |a, bytes| {
                        let first = (start.wrapping_sub(a) & 7) as usize;
                        for k in (first..bytes.len()).step_by(8) {
                            let slot = a + k as u64;
                            if slot + 8 > seg.end() {
                                break;
                            }
                            if let Some(v) = bytes.u64(k).or_else(|| dump.u64(slot))
                                && code.is_return_address(v)
                            {
                                info.frames.push(Frame {
                                    slot,
                                    return_address: code.strip(v),
                                });
                            }
                        }
                        thread_progress.portion(0.4, 1.0).items(
                            (a - start) as usize + bytes.len(),
                            (seg.end() - start) as usize,
                        );
                    });
                }
            }
            threads.push(info);
            thread_progress.report(1.0);
        }
        progress.report(1.0);
        Ok(Self { threads, code })
    }
    pub fn thread(&self, tid: i32) -> Option<&ThreadInfo> {
        self.threads.iter().find(|t| t.tid == tid)
    }
    pub fn owner(&self, slot: u64, t: &ThreadInfo) -> u64 {
        t.frames
            .partition_point(|f| f.slot < slot)
            .checked_sub(1)
            .map(|i| t.frames[i].return_address)
            .unwrap_or(t.instruction_pointer)
    }
    pub fn describe(&self, a: u64) -> String {
        self.code
            .locate(a)
            .map(|l| l.display_name())
            .unwrap_or_else(|| format!("0x{a:x}"))
    }
    pub fn top_frames(&self, t: &ThreadInfo, count: usize) -> Vec<String> {
        let mut names = vec![self.describe(t.instruction_pointer)];
        for f in &t.frames {
            if names.len() >= count {
                break;
            }
            let name = self.describe(f.return_address);
            if names.last() != Some(&name) {
                names.push(name);
            }
        }
        names
    }
    pub fn current_frame(&self, t: &ThreadInfo) -> Option<String> {
        let addresses: Vec<_> = std::iter::once(t.instruction_pointer)
            .chain(t.frames.iter().map(|f| f.return_address))
            .collect();
        addresses
            .iter()
            .filter_map(|&a| self.code.locate(a))
            .find(|l| l.kind == LocationKind::Managed)
            .map(|l| l.display_name())
            .or_else(|| {
                addresses
                    .iter()
                    .map(|&a| self.describe(a))
                    .find(|n| !n.starts_with("libc.so") && !n.starts_with("libpthread"))
            })
    }
    pub fn summary(&self, t: &ThreadInfo) -> String {
        let used = t.touched_bytes();
        let size = if used >= 1 << 20 {
            format!("{:.1} MB", used as f64 / 1_048_576.0)
        } else {
            format!("{} KB", used / 1024)
        };
        let mut text = format!(
            "deepest call used {size}, in use now {} KB",
            t.in_use_bytes() / 1024
        );
        if let Some(frame) = self.current_frame(t) {
            text += &format!(", in {frame}");
        }
        text
    }
    pub fn label(&self, tid: i32) -> String {
        match self
            .thread(tid)
            .and_then(|t| t.name.as_ref())
            .filter(|n| !n.is_empty())
        {
            Some(name) => format!("thread {tid} ({name})"),
            None => format!("thread {tid}"),
        }
    }
}
