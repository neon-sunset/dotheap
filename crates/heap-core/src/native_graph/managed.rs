use crate::{Result, analysis::HeapSnapshot, heap::MethodTableInfo};
use std::sync::Arc;

pub(super) struct PointerField {
    offset: u64,
    path: String,
}

pub(super) struct ManagedLayout {
    fields: Vec<PointerField>,
    data: u64,
    stride: u64,
}

pub(super) fn layouts(snapshot: &HeapSnapshot) -> Result<Arc<Vec<ManagedLayout>>> {
    snapshot.memo("native pointer layouts", || {
        Ok(snapshot
            .heap
            .types
            .iter()
            .map(|&mt| ManagedLayout::new(mt, snapshot))
            .collect())
    })
}

impl ManagedLayout {
    fn new(mt: MethodTableInfo, snapshot: &HeapSnapshot) -> Self {
        let mut layout = Self {
            fields: Vec::new(),
            data: 0,
            stride: 0,
        };
        if mt.component_size() == 0 {
            layout.collect(mt.address, 8, "", snapshot, &mut Vec::new());
        } else if mt.flags & 0xc0000 == 0x80000 {
            layout.data = u64::from(mt.base_size).saturating_sub(8);
            layout.stride = u64::from(mt.component_size());
            if let Some(element) = mt
                .address
                .checked_add(48)
                .and_then(|a| snapshot.dump.u64(a))
            {
                if element & 2 != 0 {
                    if snapshot
                        .dump
                        .u8(element & !2)
                        .is_some_and(|kind| matches!(kind, 0xf | 0x1b))
                        && layout.stride == 8
                    {
                        layout.fields.push(PointerField {
                            offset: 0,
                            path: String::new(),
                        });
                    }
                } else if element != 0
                    && snapshot
                        .dump
                        .u32(element)
                        .is_some_and(|flags| flags & 0xc0000 == 0x40000)
                {
                    layout.collect(element, 0, "", snapshot, &mut Vec::new());
                }
            }
            layout.fields.retain(|field| {
                field
                    .offset
                    .checked_add(8)
                    .is_some_and(|end| end <= layout.stride)
            });
        }
        layout.fields.sort_by_key(|field| field.offset);
        layout.fields.dedup_by_key(|field| field.offset);
        layout
    }

    fn collect(
        &mut self,
        mt: u64,
        base: u64,
        prefix: &str,
        snapshot: &HeapSnapshot,
        parents: &mut Vec<u64>,
    ) {
        if parents.len() >= 64 || parents.contains(&mt) {
            return;
        }
        parents.push(mt);
        for field in snapshot.names.fields(mt).iter() {
            let Some(offset) = field
                .offset
                .checked_sub(8)
                .and_then(|n| base.checked_add(n as u64))
            else {
                continue;
            };
            if field.is_native_pointer() || field.value_type.is_some() {
                let path = if prefix.is_empty() {
                    field.name.clone()
                } else {
                    format!("{prefix}.{}", field.name)
                };
                if field.is_native_pointer() {
                    self.fields.push(PointerField { offset, path });
                } else if let Some(value_type) = field.value_type {
                    self.collect(value_type, offset, &path, snapshot, parents);
                }
            }
        }
        parents.pop();
    }

    pub(super) fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub(super) fn slots(
        &self,
        start: u64,
        size: u64,
        length: u32,
    ) -> impl Iterator<Item = (u64, &str, Option<u64>)> {
        let size = size.saturating_sub(8);
        let count = if self.stride == 0 {
            1
        } else {
            u64::from(length).min(size.saturating_sub(self.data) / self.stride)
        };
        (0..count).flat_map(move |index| {
            let base = self.data + index * self.stride;
            self.fields.iter().filter_map(move |field| {
                let offset = base.checked_add(field.offset)?;
                (offset.checked_add(8)? <= size).then_some((
                    start.checked_add(offset)?,
                    field.path.as_str(),
                    (self.stride != 0).then_some(index),
                ))
            })
        })
    }
}
