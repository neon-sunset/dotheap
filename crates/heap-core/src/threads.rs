use crate::{
    Result, analysis::HeapSnapshot, checks::BLOCKING_FRAMES, code_map::LocationKind,
    symbols::file_name, type_names::short,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThreadState {
    pub tid: i32,
    pub pool: bool,
    pub idle: bool,
    pub sleeps: bool,
    pub managed: bool,
    pub wait: Option<String>,
    pub wait_frame: Option<String>,
    pub waits_for_lock: Option<usize>,
    pub holds_locks: Vec<usize>,
    pub frames: Vec<String>,
}
impl ThreadState {
    pub fn state(&self) -> &'static str {
        if self.waits_for_lock.is_some() {
            "waits for a lock"
        } else if self.wait.is_some() {
            "blocked"
        } else if self.idle {
            "idle"
        } else if self.sleeps {
            "sleeps"
        } else if self.managed {
            "other"
        } else {
            "native code"
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadActivity {
    pub title: String,
    pub threads: Vec<i32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadGroup {
    pub function: String,
    pub threads: Vec<i32>,
    pub activities: Vec<ThreadActivity>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StackNode {
    pub id: String,
    pub frames: Vec<String>,
    pub threads: Vec<i32>,
    pub children: Vec<StackNode>,
}
pub fn is_runtime_frame(n: &str) -> bool {
    n.starts_with("System.")
        || n.starts_with("Microsoft.")
        || n.starts_with("0x")
        || n.contains(".so")
        || n.contains(".dylib")
        || n.contains("runtime stub")
        || n.starts_with("system library")
}

fn wait_frames(names: &[String]) -> Option<(usize, Option<usize>)> {
    let mut blocking =
        (0..names.len()).filter(|&k| BLOCKING_FRAMES.iter().any(|s| names[k].contains(s)));
    let mut wait = blocking.next()?;
    for next in blocking {
        if !names[wait + 1..next].iter().all(|n| is_runtime_frame(n)) {
            break;
        }
        wait = next;
    }
    let caller = (wait + 1..names.len()).find(|&i| !is_runtime_frame(&names[i]));
    Some((wait, caller))
}

impl HeapSnapshot {
    pub fn thread_states(&self) -> Result<Arc<Vec<ThreadState>>> {
        let compute = || {
            let stacks = self.thread_stacks()?;
            let full_symbols: HashSet<_> = self
                .symbols()?
                .symbol_files
                .iter()
                .map(|p| {
                    let name = file_name(p);
                    name.strip_suffix(".dbg").unwrap_or(&name).to_owned()
                })
                .collect();
            let locks = self.lock_waits()?;
            let lock_of: HashMap<_, _> =
                locks.waits.iter().map(|w| (w.tid, w.lock.object)).collect();
            let mut held = HashMap::<i32, Vec<usize>>::new();
            for l in &locks.locks {
                if let Some(owner) = l.owner {
                    held.entry(owner).or_default().push(l.object);
                }
            }
            Ok(stacks
                .threads
                .iter()
                .map(|t| {
                    let addresses: Vec<_> = std::iter::once(t.instruction_pointer)
                        .chain(t.frames.iter().map(|f| f.return_address))
                        .collect();
                    let names: Vec<_> = addresses
                        .iter()
                        .map(|&a| {
                            stacks
                                .code
                                .locate(a)
                                .map(|l| l.name)
                                .unwrap_or_else(|| format!("0x{a:x}"))
                        })
                        .collect();
                    let pool = names.iter().any(|n| {
                        n.contains("ThreadPoolWorkQueue.Dispatch")
                            || n.contains("WorkerThread.WorkerThreadStart")
                    }) || t.name.as_ref().is_some_and(|n| n.contains("TP Worker"));
                    let (mut wait, mut wait_frame, mut idle) = (None, None, false);
                    if let Some((blocking, caller)) = wait_frames(&names) {
                        wait = Some(short(&names[blocking]));
                        wait_frame = Some(caller.map(|i| short(&names[i])).unwrap_or("-".into()));
                    } else if pool
                        && names
                            .iter()
                            .take(8)
                            .any(|n| n.contains("LowLevelLifoSemaphore"))
                    {
                        idle = true;
                    }
                    let mut frames = Vec::new();
                    for &a in addresses.iter().rev() {
                        if let Some(l) = stacks.code.locate(a)
                            && l.kind == LocationKind::Managed
                            && frames.last() != Some(&l.name)
                        {
                            frames.push(l.name);
                        }
                    }
                    let managed = !frames.is_empty();
                    if !managed {
                        if let Some(outer) = names.iter().rev().find(|n| n.contains('!')) {
                            frames.push(outer.clone());
                        }
                        for n in names.iter().rev() {
                            if n.contains('!')
                                && full_symbols.contains(n.split('!').next().unwrap_or(n))
                                && frames.last() != Some(n)
                            {
                                frames.push(n.clone());
                            }
                        }
                        if let Some(inner) =
                            names.iter().take(3).find(|n| n.contains('!')).or_else(|| {
                                names.iter().find(|n| {
                                    !n.starts_with("0x") && !n.starts_with("system library")
                                })
                            })
                            && frames.last() != Some(inner)
                        {
                            frames.push(inner.clone());
                        }
                    }
                    let sleeps = wait.is_none()
                        && names.iter().take(12).any(|n| {
                            n.ends_with("Thread.Sleep") || n.contains("Thread.SleepInternal")
                        });
                    ThreadState {
                        tid: t.tid,
                        pool,
                        idle,
                        sleeps,
                        managed,
                        wait,
                        wait_frame,
                        waits_for_lock: lock_of.get(&t.tid).copied(),
                        holds_locks: held.get(&t.tid).cloned().unwrap_or_default(),
                        frames,
                    }
                })
                .collect())
        };
        self.saved("thread states v2", compute)
    }
    pub fn thread_groups(&self) -> Result<Vec<ThreadGroup>> {
        let states = self.thread_states()?;
        let mut next = HashMap::<Vec<String>, HashSet<String>>::new();
        for t in states.iter() {
            for k in 0..t.frames.len() {
                next.entry(t.frames[..k].to_vec())
                    .or_default()
                    .insert(t.frames[k].clone());
            }
        }
        let function = |frames: &[String]| {
            let Some(first) = frames.first() else {
                return "no frames".into();
            };
            (1..frames.len())
                .find(|&k| next.get(&frames[..k]).is_some_and(|v| v.len() > 1))
                .map(|k| frames[k].clone())
                .or_else(|| frames.iter().find(|n| !is_runtime_frame(n)).cloned())
                .unwrap_or_else(|| first.clone())
        };
        let mut groups = BTreeMap::<String, Vec<&ThreadState>>::new();
        for t in states.iter() {
            groups.entry(function(&t.frames)).or_default().push(t);
        }
        let mut groups: Vec<_> = groups
            .into_iter()
            .map(|(function, list)| {
                let mut activities = BTreeMap::<String, Vec<i32>>::new();
                for t in &list {
                    let title = if let Some(wait) = &t.wait {
                        format!(
                            "{wait}{}",
                            t.wait_frame
                                .as_ref()
                                .filter(|f| f.as_str() != "-")
                                .map(|f| format!(" in {f}"))
                                .unwrap_or_default()
                        )
                    } else {
                        t.frames
                            .last()
                            .map(|n| short(n))
                            .unwrap_or("no frames".into())
                    };
                    activities.entry(title).or_default().push(t.tid);
                }
                let mut activities: Vec<_> = activities
                    .into_iter()
                    .map(|(title, threads)| ThreadActivity { title, threads })
                    .collect();
                activities.sort_by(|a, b| {
                    b.threads
                        .len()
                        .cmp(&a.threads.len())
                        .then(a.title.cmp(&b.title))
                });
                ThreadGroup {
                    function: short(&function),
                    threads: list.iter().map(|t| t.tid).collect(),
                    activities,
                }
            })
            .collect();
        groups.sort_by(|a, b| {
            b.threads
                .len()
                .cmp(&a.threads.len())
                .then(a.function.cmp(&b.function))
        });
        Ok(groups)
    }
    pub fn merged_stacks(&self) -> Result<Arc<Vec<StackNode>>> {
        let compute = || {
            #[derive(Default)]
            struct Trie {
                children: BTreeMap<String, usize>,
                order: Vec<String>,
                threads: Vec<i32>,
            }
            let mut nodes = vec![Trie::default()];
            for t in self.thread_states()?.iter() {
                let mut at = 0;
                for frame in &t.frames {
                    let next = if let Some(&i) = nodes[at].children.get(frame) {
                        i
                    } else {
                        let i = nodes.len();
                        nodes.push(Trie::default());
                        nodes[at].children.insert(frame.clone(), i);
                        nodes[at].order.push(frame.clone());
                        i
                    };
                    at = next;
                    nodes[at].threads.push(t.tid);
                }
            }
            fn build(frame: &str, index: usize, nodes: &[Trie], path: &str) -> StackNode {
                let mut frames = vec![frame.to_owned()];
                let mut at = index;
                while nodes[at].order.len() == 1 {
                    let name = &nodes[at].order[0];
                    let next = nodes[at].children[name];
                    if nodes[next].threads.len() != nodes[at].threads.len() {
                        break;
                    }
                    frames.push(name.clone());
                    at = next;
                }
                let id = format!("{path}/{}", frames.join(">"));
                let mut children: Vec<_> = nodes[at]
                    .order
                    .iter()
                    .map(|f| build(f, nodes[at].children[f], nodes, &id))
                    .collect();
                children.sort_by_key(|n| std::cmp::Reverse(n.threads.len()));
                StackNode {
                    id,
                    frames: frames.iter().map(|name| short(name)).collect(),
                    threads: nodes[index].threads.clone(),
                    children,
                }
            }
            let mut roots: Vec<_> = nodes[0]
                .order
                .iter()
                .map(|f| build(f, nodes[0].children[f], &nodes, ""))
                .collect();
            roots.sort_by_key(|n| std::cmp::Reverse(n.threads.len()));
            Ok(roots)
        };
        self.saved("merged stacks v2", compute)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::code_map::Location;

    #[test]
    fn framework_frames_do_not_split_a_task_wait() {
        for framework in [
            "System.Collections.Generic.List`1<System.__Canon>.set_Capacity",
            "System.Collections.Concurrent.ConcurrentDictionary`2<System.String, System.Object>.TryAdd",
            "System.Text.RegularExpressions.Symbolic.SymbolicRegexMatcher`1<System.UInt64>.FindMatch",
        ] {
            let names = [
                "System.Threading.Monitor.Wait",
                framework,
                "libcoreclr.so+0x4738dc",
                "System.Threading.ManualResetEventSlim.Wait",
                "System.Threading.Tasks.Task.SpinThenBlockingWait",
                "System.Threading.Tasks.Task.Wait",
                "Program.Run",
            ]
            .map(str::to_owned);
            assert_eq!(wait_frames(&names), Some((5, Some(6))));
            let location = Location {
                kind: LocationKind::Managed,
                name: names[5].clone(),
            };
            assert_eq!(location.display_name(), "Task.Wait");
        }
    }

    #[test]
    fn application_frames_stop_the_wait_chain() {
        for application in [
            "Shop.Worker.Run",
            "TaskProcessor.Run",
            "List`1<Shop.Item>.Wait",
        ] {
            let names = [
                "System.Threading.Monitor.Wait",
                application,
                "System.Threading.Tasks.Task.Wait",
                "Program.Run",
            ]
            .map(str::to_owned);
            assert_eq!(wait_frames(&names), Some((0, Some(1))));
        }
    }
}
