use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Cell, CheckReport, CheckRow, CheckSection, Finding},
    core_dump::Cursor,
    type_names::without_namespaces,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HeldLock {
    pub object: usize,
    pub kind: String,
    pub owner: Option<i32>,
    pub recursion: usize,
    pub waiters: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockWait {
    pub tid: i32,
    pub lock: HeldLock,
    pub frame: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LockWaits {
    pub locks: Vec<HeldLock>,
    pub waits: Vec<LockWait>,
}
const ENTER_FRAMES: [&str; 6] = [
    "Threading.Monitor.Enter",
    "Threading.Monitor.ReliableEnter",
    "Threading.Monitor.TryEnter",
    "Threading.Lock.Enter",
    "Threading.Lock.TryEnter",
    "AwareLock::Enter",
];
impl HeapSnapshot {
    pub fn held_locks(&self) -> Result<Arc<Vec<HeldLock>>> {
        let compute = || {
            let mut os_threads = HashMap::new();
            let mut sync_table = None;
            if let Some(runtime) = &self.runtime
                && let Some(d) = &runtime.descriptor
            {
                if let Some(offset) = d.offset("Thread", "Id") {
                    for (thread, os_id) in runtime.threads(&self.dump) {
                        if let Some(id) = thread.checked_add(offset).and_then(|a| self.dump.u32(a))
                        {
                            os_threads.insert(id, os_id);
                        }
                    }
                }
                if let Some(base) = d
                    .globals
                    .get("SyncTableEntries")
                    .and_then(|&a| self.dump.u64(a))
                    .filter(|&a| a != 0)
                {
                    sync_table = Some((
                        base,
                        d.offset("SyncTableEntry", "!").unwrap_or(16),
                        d.offset("SyncTableEntry", "SyncBlock").unwrap_or(0),
                    ));
                }
            }
            let mut locks = Vec::new();
            let mut cursor = Cursor::new(&self.dump, true);
            for i in 0..self.heap.count() {
                let Some(header) = self.heap.addresses[i]
                    .checked_sub(4)
                    .and_then(|a| cursor.u32(a))
                    .filter(|&h| h & 0x0fff_ffff != 0)
                else {
                    continue;
                };
                if header & 0x0800_0000 == 0 {
                    let id = header & 0xffff;
                    if id != 0 {
                        locks.push(HeldLock {
                            object: i,
                            kind: "Monitor".into(),
                            owner: os_threads.get(&id).copied(),
                            recursion: ((header >> 16) & 63) as usize + 1,
                            waiters: 0,
                        });
                    }
                } else if header & 0x0400_0000 == 0
                    && let Some((base, size, offset)) = sync_table
                {
                    let index = (header & 0x03ff_ffff) as u64;
                    if let Some(block) = index
                        .checked_mul(size)
                        .and_then(|a| a.checked_add(base))
                        .and_then(|a| a.checked_add(offset))
                        .and_then(|a| self.dump.u64(a))
                        .filter(|&b| b != 0)
                        && let Some(state) = self.dump.u32(block).filter(|&s| s & 1 != 0)
                    {
                        let id = block
                            .checked_add(8)
                            .and_then(|a| self.dump.u32(a))
                            .unwrap_or(0);
                        locks.push(HeldLock {
                            object: i,
                            kind: "Monitor".into(),
                            owner: os_threads.get(&id).copied(),
                            recursion: block
                                .checked_add(4)
                                .and_then(|a| self.dump.u32(a))
                                .unwrap_or(1) as usize,
                            waiters: (state >> 6) as usize,
                        });
                    }
                }
            }
            for o in self.objects(&self.type_ids(|s| s == "System.Threading.Lock")) {
                if let Some(state) = self.integer(o, "_state").filter(|&s| s & 1 != 0) {
                    locks.push(HeldLock {
                        object: o,
                        kind: "Lock".into(),
                        owner: self.integer(o, "_owningThreadId").map(|v| v as i32),
                        recursion: self.integer(o, "_recursionCount").unwrap_or(0).max(0) as usize
                            + 1,
                        waiters: (state as u32 >> 7) as usize,
                    });
                }
            }
            Ok(locks)
        };
        self.saved("held locks", compute)
    }
    pub fn lock_waits(&self) -> Result<Arc<LockWaits>> {
        let compute = || {
            let stacks = self.thread_stacks()?;
            let mut dump_thread = HashMap::new();
            if !self.dump.platform.is_linux()
                && let Some(r) = &self.runtime
                && let Some(offset) = r
                    .descriptor
                    .as_ref()
                    .and_then(|d| d.offset("Thread", "Frame"))
            {
                for (thread, os_id) in r.threads(&self.dump) {
                    if let Some(frame) = thread.checked_add(offset).and_then(|a| self.dump.u64(a))
                        && let Some(t) = stacks
                            .threads
                            .iter()
                            .find(|t| frame >= t.stack_pointer && frame < t.stack_end)
                    {
                        dump_thread.insert(os_id, t.tid);
                    }
                }
            }
            let locks: Vec<_> = self
                .held_locks()?
                .iter()
                .map(|l| {
                    let mut l = l.clone();
                    l.owner = l.owner.map(|o| dump_thread.get(&o).copied().unwrap_or(o));
                    l
                })
                .collect();
            let mut addresses = HashMap::new();
            for l in &locks {
                addresses.entry(self.heap.addresses[l.object]).or_insert(l);
            }
            let mut waits = Vec::new();
            for t in stacks
                .threads
                .iter()
                .filter(|t| t.stack_end > t.stack_pointer)
            {
                let names: Vec<_> = std::iter::once(t.instruction_pointer)
                    .chain(t.frames.iter().map(|f| f.return_address))
                    .map(|a| stacks.describe(a))
                    .collect();
                let Some(enter) = names
                    .iter()
                    .position(|n| ENTER_FRAMES.iter().any(|f| n.contains(f)))
                else {
                    continue;
                };
                let mut cursor = Cursor::new(&self.dump, true);
                let lock = (t.stack_pointer & !7..t.stack_end.saturating_sub(7))
                    .step_by(8)
                    .find_map(|a| {
                        cursor
                            .u64(a)
                            .and_then(|a| addresses.get(&a).copied())
                            .filter(|l| l.owner != Some(t.tid))
                    });
                if let Some(lock) = lock {
                    let frame = names[enter + 1..]
                        .iter()
                        .find(|n| {
                            !n.starts_with("System.")
                                && !n.starts_with("Microsoft.")
                                && !n.starts_with("0x")
                                && !n.contains(".so")
                                && !n.contains(".dylib")
                                && !n.starts_with("system library")
                        })
                        .cloned()
                        .unwrap_or("-".into());
                    waits.push(LockWait {
                        tid: t.tid,
                        lock: lock.clone(),
                        frame,
                    });
                }
            }
            Ok(LockWaits { locks, waits })
        };
        self.saved("lock waits", compute)
    }
    pub fn lock_check(&self) -> Result<Arc<CheckReport>> {
        let compute = || {
            let mut report = CheckReport::new(
                "locks",
                "Locks and waiting threads",
                "Held lock/Monitor.Enter and System.Threading.Lock locks, with inferred waiters. \
                    A likely waiter has a lock-enter frame and a stack word pointing to a lock \
                    another thread holds. Frames are estimated from return addresses; inspect the \
                    wait chain before concluding that a deadlock is present.",
            );
            let stacks = self.thread_stacks()?;
            let state = self.lock_waits()?;
            let locks = &state.locks;
            let waits: BTreeMap<_, _> = state.waits.iter().map(|w| (w.tid, w)).collect();
            let mut lock_names = HashMap::new();
            for l in locks {
                lock_names.insert(
                    l.object,
                    format!(
                        "{}, held by {}",
                        if l.kind == "Lock" {
                            "Lock".into()
                        } else {
                            format!(
                                "Monitor on {}",
                                without_namespaces(&self.type_name(l.object))
                            )
                        },
                        self.holder(l.object)?
                    ),
                );
            }
            let owner_text = |tid: Option<i32>| {
                tid.map(|t| stacks.label(t))
                    .unwrap_or("a thread that the runtime's thread list does not have".into())
            };
            let mut waiters = BTreeMap::<i32, Vec<i32>>::new();
            for (&tid, w) in &waits {
                waiters
                    .entry(w.lock.owner.unwrap_or(-1))
                    .or_default()
                    .push(tid);
            }
            fn rows(
                owner: i32,
                path: &HashSet<i32>,
                waiters: &BTreeMap<i32, Vec<i32>>,
                waits: &BTreeMap<i32, &LockWait>,
                names: &HashMap<usize, String>,
                stacks: &crate::thread_stacks::ThreadStacks,
            ) -> Vec<CheckRow> {
                let mut groups = Vec::<(String, Vec<i32>)>::new();
                for &tid in waiters
                    .get(&owner)
                    .into_iter()
                    .flatten()
                    .filter(|t| !path.contains(t))
                {
                    let w = waits[&tid];
                    let key = format!("waits for {}, in {}", names[&w.lock.object], w.frame);
                    if let Some((_, tids)) = groups.iter_mut().find(|(k, _)| k == &key) {
                        tids.push(tid);
                    } else {
                        groups.push((key, vec![tid]));
                    }
                }
                groups
                    .into_iter()
                    .map(|(key, tids)| {
                        let text = format!(
                            "{key}: {}{}",
                            tids.iter()
                                .take(5)
                                .map(|&t| stacks.label(t))
                                .collect::<Vec<_>>()
                                .join(", "),
                            if tids.len() > 5 { ", ..." } else { "" }
                        );
                        let mut row = CheckRow::new(
                            vec![Cell::int(tids.len()), Cell::Text(text)],
                            vec![waits[&tids[0]].lock.object],
                        );
                        for tid in tids {
                            let mut next = path.clone();
                            next.insert(tid);
                            row.children
                                .extend(rows(tid, &next, waiters, waits, names, stacks));
                        }
                        row
                    })
                    .collect()
            }
            let mut in_cycle = BTreeSet::new();
            for &start in waits.keys() {
                let mut seen = vec![start];
                let mut t = start;
                while let Some(next) = waits
                    .get(&t)
                    .and_then(|w| w.lock.owner)
                    .filter(|n| waits.contains_key(n))
                {
                    if let Some(k) = seen.iter().position(|&v| v == next) {
                        in_cycle.extend(seen[k..].iter().copied());
                        break;
                    }
                    seen.push(next);
                    t = next;
                }
            }
            let mut tree = CheckSection::new(
                "Held locks",
                "Each thread that holds a lock, with the threads that wait for its locks below it.",
                &["threads", "thread"],
            );
            let owners: BTreeSet<_> = locks.iter().map(|l| l.owner.unwrap_or(-1)).collect();
            for owner in owners.into_iter().filter(|t| !waits.contains_key(t)) {
                let held: Vec<_> = locks
                    .iter()
                    .filter(|l| l.owner.unwrap_or(-1) == owner)
                    .collect();
                let text = format!(
                    "{} holds {}{}",
                    owner_text((owner != -1).then_some(owner)),
                    held.iter()
                        .take(3)
                        .map(|l| lock_names[&l.object].as_str())
                        .collect::<Vec<_>>()
                        .join("; "),
                    if held.len() > 3 {
                        format!("; {} more", held.len() - 3)
                    } else {
                        String::new()
                    }
                );
                let mut row = CheckRow::new(
                    vec![Cell::Int(1), Cell::Text(text)],
                    held.iter().map(|l| l.object).collect(),
                );
                row.children = rows(
                    owner,
                    &HashSet::from([owner]),
                    &waiters,
                    &waits,
                    &lock_names,
                    &stacks,
                );
                tree.rows.push(row);
            }
            tree.rows
                .sort_by_key(|r| std::cmp::Reverse(r.children.len()));
            let mut deadlocks = CheckSection::new(
                "Deadlocks",
                "Threads that wait for each other's locks: none of them can continue.",
                &["threads", "thread"],
            );
            let mut done = HashSet::new();
            for &tid in &in_cycle {
                if done.contains(&tid) {
                    continue;
                }
                let mut cycle = vec![tid];
                let mut t = tid;
                while let Some(next) = waits
                    .get(&t)
                    .and_then(|w| w.lock.owner)
                    .filter(|&next| next != tid)
                {
                    if cycle.len() >= 1000 {
                        break;
                    }
                    cycle.push(next);
                    t = next;
                }
                done.extend(cycle.iter().copied());
                let text = cycle
                    .iter()
                    .map(|&t| {
                        format!(
                            "{} waits for {}",
                            stacks.label(t),
                            lock_names[&waits[&t].lock.object]
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                deadlocks.rows.push(CheckRow::new(
                    vec![Cell::int(cycle.len()), Cell::Text(text)],
                    cycle.iter().map(|t| waits[t].lock.object).collect(),
                ));
            }
            let mut all = CheckSection::new(
                "All held locks",
                "Waiters is the runtime's count of threads that wait in the lock (a thin lock in \
                    the object header has none).",
                &["waiters", "recursion", "lock", "owner"],
            );
            let mut sorted: Vec<_> = locks.iter().collect();
            sorted.sort_by(|a, b| b.waiters.cmp(&a.waiters).then(a.object.cmp(&b.object)));
            for l in sorted {
                all.rows.push(CheckRow::new(
                    vec![
                        Cell::int(l.waiters),
                        Cell::int(l.recursion),
                        Cell::Text(lock_names[&l.object].clone()),
                        Cell::Text(owner_text(l.owner)),
                    ],
                    vec![l.object],
                ));
            }
            if !deadlocks.rows.is_empty() {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!(
                        "{} threads wait for each other's locks (deadlock)",
                        in_cycle.len()
                    ),
                });
            }
            for l in locks.iter().filter(|l| l.waiters >= 4) {
                report.findings.push(Finding {
                    bytes: 0,
                    text: format!("{} threads wait for {}", l.waiters, lock_names[&l.object]),
                });
            }
            report.sections = vec![tree, deadlocks, all];
            Ok(report)
        };
        self.saved("check locks", compute)
    }
}
