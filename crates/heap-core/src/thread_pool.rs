use crate::{Result, analysis::HeapSnapshot, tasks::async_method, type_names::without_namespaces};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueuedWorkItem {
    pub object: usize,
    pub local: bool,
    pub what: String,
}

impl HeapSnapshot {
    pub fn queued_work_items(&self) -> Result<Arc<Vec<QueuedWorkItem>>> {
        let compute = || {
            let mut items = Vec::new();
            let mut add = |object, local| -> Result<()> {
                items.push(QueuedWorkItem {
                    object,
                    local,
                    what: self.work_item_name(object)?,
                });
                Ok(())
            };
            let global_types = self.type_ids(|name| name == "System.Threading.ThreadPoolWorkQueue");
            for queue in self.objects(&global_types) {
                if self.graph.live[queue] == 0 {
                    continue;
                }
                let global = ["highPriorityWorkItems", "workItems", "lowPriorityWorkItems"]
                    .into_iter()
                    .map(|name| self.target(queue, name));
                let assignable = self.elements(self.target(queue, "_assignableWorkItemQueues"));
                for queue in global.chain(assignable) {
                    for object in self.concurrent_queue_items(queue) {
                        add(object, false)?;
                    }
                }
            }
            let local_types = self
                .type_ids(|name| name == "System.Threading.ThreadPoolWorkQueue+WorkStealingQueue");
            for queue in self.objects(&local_types) {
                if self.graph.live[queue] != 0 {
                    for object in self
                        .elements(self.target(queue, "m_array"))
                        .into_iter()
                        .flatten()
                    {
                        add(object, true)?;
                    }
                }
            }
            Ok(items)
        };
        self.saved("thread pool queue", compute)
    }

    pub fn concurrent_queue_items(&self, queue: Option<usize>) -> Vec<usize> {
        let mut items = Vec::new();
        let mut segment = self.target(queue, "_head");
        let mut seen = HashSet::new();
        while let Some(current) = segment.filter(|&s| seen.insert(s)) {
            if let Some(slots) = self.target(current, "_slots")
                && let Some(count) = self.component_count(slots)
            {
                let size = self.heap.method_table(slots).component_size() as u64;
                let start = self.heap.addresses[slots] + 16;
                for index in 0..count {
                    if let Some(address) = self.dump.u64(start + index as u64 * size)
                        && let Some(object) = self.heap.object_at(address)
                    {
                        items.push(object);
                    }
                }
            }
            segment = self.target(current, "_nextSegment");
        }
        items
    }

    pub fn work_item_name(&self, object: usize) -> Result<String> {
        let name = self.type_name(object);
        if name.contains("StateMachineBox`1<") {
            let (owner, method) = async_method(&name);
            return Ok(if method.is_empty() {
                owner
            } else {
                format!("{owner}.{method}")
            });
        }
        for field in ["_callback", "m_action"] {
            if let Some(delegate) = self.target(object, field)
                && let Some(method) = self.delegate_method(delegate)?
            {
                return Ok(method);
            }
        }
        Ok(without_namespaces(&name))
    }
}
