use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Owner, TIMER_QUEUE_TYPES},
    tasks::{Awaiter, async_method, field_name},
    type_names::{property_name, without_namespaces},
};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Borrow,
    collections::{HashMap, HashSet},
    sync::Arc,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerInfo {
    pub object: usize,
    pub kind: String,
    pub source: Option<usize>,
    pub callback: String,
    pub target: Option<usize>,
    pub awaited_by: Option<String>,
    pub outermost: Option<String>,
    pub awaiting_call: Option<usize>,
    pub owner: Option<usize>,
    pub owner_text: String,
    pub due_time: Option<u32>,
    pub period: Option<u32>,
    pub state: String,
    pub age: Option<i64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Timers {
    pub timers: Vec<TimerInfo>,
    pub latest_start: i64,
}

impl HeapSnapshot {
    pub fn timer_places(&self, timers: &[impl Borrow<TimerInfo>]) -> Result<Vec<String>> {
        if timers.len() <= 1 {
            return Ok(timers
                .iter()
                .map(|t| t.borrow().owner_text.clone())
                .collect());
        }
        let avoid = self.type_ids(|name| TIMER_QUEUE_TYPES.contains(&name));
        let mut paths = Vec::new();
        let fallback: Vec<_> = timers
            .iter()
            .map(|t| format!("timer 0x{:x}", self.heap.addresses[t.borrow().object]))
            .collect();
        for timer in timers {
            let timer = timer.borrow();
            let object = timer.owner.or(timer.source).unwrap_or(timer.object);
            let path = self.path_avoiding(object, &avoid).or_else(|| {
                (self.graph.live[object] != 0).then(|| self.graph.path_to_root(object))
            });
            let mut steps = Vec::new();
            if let Some((source, objects)) = path {
                for pair in objects.windows(2).rev() {
                    let field = self
                        .reference_name(pair[0], pair[1])?
                        .as_deref()
                        .map(property_name)
                        .unwrap_or_default();
                    steps.push(if field.starts_with("static ") {
                        field
                    } else {
                        without_namespaces(&self.type_name(pair[0])) + &field
                    });
                }
                if let Some(&first) = objects.first() {
                    if let Some((function, thread)) = self.stack_holder(first)? {
                        steps.extend([format!("a local of {function}"), thread]);
                    } else {
                        steps.push(
                            source
                                .map(|s| self.describe_source(s))
                                .transpose()?
                                .unwrap_or("no path from a root".into()),
                        );
                    }
                }
            }
            paths.push(steps);
        }
        Ok(path_labels(&paths, &fallback))
    }

    pub fn timers(&self) -> Result<Arc<Timers>> {
        let compute = || {
            let awaiters = self.async_analysis()?.graphs.awaiters();
            let timer_types = self.type_ids(|s| s == "System.Threading.TimerQueueTimer");
            let queue_types = self.type_ids(|s| s == "System.Threading.TimerQueue");
            let mut latest_start = 0;
            for queue in self
                .objects(&queue_types)
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
            {
                latest_start =
                    latest_start.max(self.integer(queue, "_currentTimerStartTicks").unwrap_or(0));
            }
            let objects: Vec<_> = self
                .objects(&timer_types)
                .into_iter()
                .filter(|&o| self.graph.live[o] != 0)
                .collect();
            for &timer in &objects {
                if self.integer(timer, "_dueTime").unwrap_or(-1) as u32 != u32::MAX {
                    latest_start =
                        latest_start.max(self.integer(timer, "_startTicks").unwrap_or(0));
                }
            }
            let mut timers = Vec::new();
            for timer in objects {
                let source = self
                    .graph
                    .referrer_rows
                    .row(timer)
                    .iter()
                    .map(|&o| o as usize)
                    .find(|&o| {
                        self.graph.live[o] != 0
                            && !timer_types.contains(&self.heap.type_ids[o])
                            && !queue_types.contains(&self.heap.type_ids[o])
                    });
                timers.push(self.timer_info(timer, source, &awaiters, latest_start)?);
            }
            Ok(Timers {
                timers,
                latest_start,
            })
        };
        self.saved("timers", compute)
    }

    fn timer_info(
        &self,
        object: usize,
        source: Option<usize>,
        awaiters: &HashMap<usize, Awaiter>,
        latest: i64,
    ) -> Result<TimerInfo> {
        let source_type = source.map(|o| self.type_name(o)).unwrap_or_default();
        let state = self.target(object, "_state");
        let callback = self.target(object, "_timerCallback");
        let mut kind = if source_type.is_empty() {
            "TimerQueueTimer".into()
        } else {
            without_namespaces(&source_type)
        };
        let mut what = callback
            .map(|o| self.delegate_method_with_source(o))
            .transpose()?
            .flatten()
            .unwrap_or("?".into());
        let mut target = self.target(callback, "_target").or(state);
        let mut awaited = None;
        let mut skip = HashSet::new();
        match source_type.as_str() {
            "System.Threading.TimerHolder" => {
                kind = "Timer".into();
                skip.extend(["System.Threading.TimerHolder", "System.Threading.Timer"]);
                if let Some(timer) = target.filter(|&o| self.type_name(o) == "System.Timers.Timer")
                {
                    kind = "System.Timers.Timer".into();
                    skip.insert("System.Timers.Timer");
                    if let Some(handler) = self.target(timer, "_onIntervalElapsed") {
                        let mut handlers: Vec<_> = self
                            .elements(self.target(handler, "_invocationList"))
                            .into_iter()
                            .flatten()
                            .collect();
                        if handlers.is_empty() {
                            handlers.push(handler);
                        }
                        let mut methods = Vec::new();
                        for &handler in &handlers {
                            if let Some(method) = self.delegate_method_with_source(handler)? {
                                methods.push(method);
                            }
                        }
                        what = methods.join(", ");
                        target = self.target(handlers[0], "_target");
                    } else {
                        what = "no Elapsed handler".into();
                    }
                }
            }
            "System.Threading.PeriodicTimer" => {
                kind = "PeriodicTimer".into();
                awaited = state;
                what = "signals the PeriodicTimer".into();
                skip.insert("System.Threading.PeriodicTimer");
            }
            name if name.ends_with("+DelayPromise")
                || name.ends_with("+DelayPromiseWithCancellation") =>
            {
                kind = "Task.Delay".into();
                awaited = source;
                what = "completes the Task.Delay task".into();
            }
            name if name.contains("+CancellationPromise`1<") => {
                kind = "Task.WaitAsync".into();
                awaited = source;
                what = "ends the wait of Task.WaitAsync with a timeout".into();
            }
            name if name.starts_with("System.Threading.CancellationTokenSource")
                || name.ends_with("CancellationTokenSource") =>
            {
                kind = "CancelAfter".into();
                skip.insert(name);
                let first = self.target(
                    self.target(self.target(source, "_registrations"), "Callbacks"),
                    "Callback",
                );
                let method = first
                    .map(|o| self.delegate_method_with_source(o))
                    .transpose()?
                    .flatten();
                what = format!(
                    "cancels the CancellationTokenSource{}",
                    method
                        .map(|s| format!("; a callback: {s}"))
                        .unwrap_or("; no callbacks registered".into())
                );
                target = source;
            }
            _ => {}
        }
        let call = awaited.or(source).and_then(|o| awaiters.get(&o));
        let owner =
            if let Some(source) = source.filter(|_| awaited.is_none() || kind == "PeriodicTimer") {
                self.owner(source, 10_000, |name| skip.contains(name))?
            } else {
                call.map(|c| Owner {
                    object: c.object as i64,
                    field: String::new(),
                })
            };
        let owner_text = match &owner {
            Some(owner) if call.is_some_and(|c| owner.object == c.object as i64) => format!(
                "the state machine of {}, which awaits it",
                call.unwrap().call
            ),
            Some(owner) => self.owner_text(owner),
            None => "nothing else".into(),
        };
        let due = self.integer(object, "_dueTime").unwrap_or(-1) as u32;
        let period = self.integer(object, "_period").unwrap_or(-1) as u32;
        let canceled = self.integer(object, "_canceled").unwrap_or(0) != 0;
        let start = self.integer(object, "_startTicks").unwrap_or(0);
        Ok(TimerInfo {
            object,
            kind,
            source,
            callback: what,
            target,
            awaited_by: call.map(|c| c.call.clone()),
            outermost: call.map(|c| c.outermost.clone()),
            awaiting_call: call.map(|c| c.object),
            owner: owner.and_then(|o| usize::try_from(o.object).ok()),
            owner_text,
            due_time: (due != u32::MAX).then_some(due),
            period: (period != 0 && period != u32::MAX).then_some(period),
            state: if canceled {
                "disposed"
            } else if due == u32::MAX {
                "stopped"
            } else {
                "scheduled"
            }
            .into(),
            age: (due != u32::MAX && start != 0 && latest != 0)
                .then(|| latest.checked_sub(start))
                .flatten(),
        })
    }

    pub fn owner_text(&self, owner: &Owner) -> String {
        if owner.object < 0 || owner.field.starts_with("static ") {
            return owner.field.clone();
        }
        let name = self.type_name(owner.object as usize);
        if name.contains("StateMachineBox`1<") {
            let (owner_type, method) = async_method(&name);
            let local = owner
                .field
                .strip_prefix(".StateMachine.")
                .and_then(field_name);
            let method = if method.is_empty() {
                String::new()
            } else {
                format!(".{method}")
            };
            let local = local.map(|s| format!(", {s}")).unwrap_or_default();
            return format!("the async call {owner_type}{method}{local}");
        }
        without_namespaces(&name) + &owner.field
    }
}

pub fn duration(milliseconds: i64) -> String {
    if milliseconds < 1000 {
        return format!("{milliseconds} ms");
    }
    let seconds = milliseconds as f64 / 1000.0;
    let (value, unit) = if seconds < 120.0 {
        (seconds, "s")
    } else if seconds < 7200.0 {
        ((seconds / 6.0).round() / 10.0, "min")
    } else if seconds < 172_800.0 {
        ((seconds / 360.0).round() / 10.0, "h")
    } else {
        ((seconds / 8640.0).round() / 10.0, "d")
    };
    if value.fract() == 0.0 {
        format!("{value:.0} {unit}")
    } else {
        format!("{value:.1} {unit}")
    }
}

pub fn timer_key(timer: &TimerInfo) -> String {
    let period = timer
        .period
        .map(|p| format!("every {}", duration(p as i64)))
        .unwrap_or_else(|| {
            if timer.state == "scheduled" {
                "once".into()
            } else {
                timer.state.clone()
            }
        });
    [
        timer.kind.as_str(),
        if timer.awaited_by.is_none() {
            timer.callback.as_str()
        } else {
            "-"
        },
        timer.awaited_by.as_deref().unwrap_or("-"),
        &timer.owner_text,
        &period,
    ]
    .join("\t")
}

fn path_labels(paths: &[Vec<String>], fallback: &[String]) -> Vec<String> {
    let depth = paths.iter().map(Vec::len).max().unwrap_or(0);
    let step = |index: usize, level: usize| {
        paths[index]
            .get(level)
            .or_else(|| paths[index].last())
            .map(String::as_str)
            .unwrap_or("")
    };
    let first = (0..depth).find(|&level| {
        (0..paths.len())
            .map(|i| step(i, level))
            .collect::<HashSet<_>>()
            .len()
            > 1
    });
    let Some(first) = first else {
        return fallback.to_vec();
    };
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            if path.is_empty() {
                return fallback[index].clone();
            }
            let mut level = first;
            while level < depth - 1
                && (0..paths.len()).any(|other| {
                    other != index && (0..=level).all(|k| step(other, k) == step(index, k))
                })
            {
                level += 1;
            }
            let upper = level.min(path.len() - 1);
            path[first.min(upper)..=upper]
                .iter()
                .rev()
                .cloned()
                .collect::<Vec<_>>()
                .join(" → ")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_include_the_steps_needed_to_distinguish_shared_owners() {
        let paths = [
            vec!["Worker._timer", "Worker[][0]", "static Program.workers"],
            vec!["Worker._timer", "Worker[][1]", "static Program.workers"],
        ]
        .map(|p| p.into_iter().map(str::to_owned).collect());
        assert_eq!(
            path_labels(&paths, &["timer a".into(), "timer b".into()]),
            ["Worker[][0]", "Worker[][1]"]
        );
        assert_eq!(duration(999), "999 ms");
        assert_eq!(duration(1500), "1.5 s");
        assert_eq!(duration(120_000), "2 min");
        assert_eq!(duration(7_200_000), "2 h");
        assert_eq!(duration(172_800_000), "2 d");
    }
}
