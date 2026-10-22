use crate::{
    Result,
    analysis::HeapSnapshot,
    checks::{Cell, CheckReport, CheckRow, CheckSection, Finding},
    column::ColumnStore,
    core_dump::{CoreDump, Cursor},
    gc_desc::GcDesc,
    heap::{Gc, ManagedHeap, MethodTableInfo},
    runtime::{ContractDescriptor, RuntimeInfo},
};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, path::Path, sync::Arc};

const MAX_DIAGNOSTICS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum VerificationKind {
    Invalid,
    MissingMemory,
    Unverified,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeapDiagnostic {
    pub kind: VerificationKind,
    pub structure: String,
    pub address: u64,
    pub object: Option<u64>,
    pub value: Option<u64>,
    pub message: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HeapVerification {
    pub objects: usize,
    pub method_tables: usize,
    pub reference_slots: usize,
    pub unparsed_bytes: u64,
    pub invalid: usize,
    pub missing_memory: usize,
    pub unverified: usize,
    pub diagnostics: Vec<HeapDiagnostic>,
    pub limitations: Vec<String>,
}

impl HeapVerification {
    pub(crate) fn issue(
        &mut self,
        kind: VerificationKind,
        structure: &str,
        address: u64,
        object: Option<u64>,
        value: Option<u64>,
        message: impl Into<String>,
    ) {
        let count = match kind {
            VerificationKind::Invalid => &mut self.invalid,
            VerificationKind::MissingMemory => &mut self.missing_memory,
            VerificationKind::Unverified => &mut self.unverified,
        };
        *count += 1;
        if *count <= MAX_DIAGNOSTICS {
            self.diagnostics.push(HeapDiagnostic {
                kind,
                structure: structure.into(),
                address,
                object,
                value,
                message: message.into(),
            });
        }
    }

    fn word(&mut self, dump: &CoreDump, address: u64, structure: &str) -> Option<u64> {
        let value = dump.u64(address);
        if value.is_none() {
            self.issue(
                VerificationKind::MissingMemory,
                structure,
                address,
                None,
                None,
                "The dump does not contain this field.",
            );
        }
        value
    }

    pub fn report(&self, heap: Option<&ManagedHeap>) -> CheckReport {
        let mut report = CheckReport::new(
            "verify",
            "Heap integrity",
            "Validates object sizes, MethodTables, GC descriptors, managed references and available \
             runtime links. Invalid values can indicate corruption or a capture during a GC or \
             runtime update. Missing memory is reported separately. Native pointer fields are not \
             managed references. A successful verification does not prove that every runtime \
             structure is intact.",
        );
        for (kind, title, count) in [
            (
                VerificationKind::Invalid,
                "Invalid structures",
                self.invalid,
            ),
            (
                VerificationKind::MissingMemory,
                "Missing memory",
                self.missing_memory,
            ),
            (
                VerificationKind::Unverified,
                "Not verified",
                self.unverified,
            ),
        ] {
            let mut section = CheckSection::new(
                title,
                "",
                &["structure", "address", "object", "value", "diagnostic"],
            );
            for d in self.diagnostics.iter().filter(|d| d.kind == kind) {
                section.rows.push(CheckRow::new(
                    vec![
                        Cell::text(&d.structure),
                        Cell::Address(d.address),
                        d.object.map(Cell::Address).unwrap_or(Cell::None),
                        d.value.map(Cell::Address).unwrap_or(Cell::None),
                        Cell::text(&d.message),
                    ],
                    d.object
                        .and_then(|a| heap.and_then(|h| h.object_at(a)))
                        .into_iter()
                        .collect(),
                ));
            }
            section.omitted = count - section.rows.len();
            if count != 0 {
                let first = self.diagnostics.iter().find(|d| d.kind == kind);
                report.findings.push(Finding {
                    bytes: 0,
                    text: if let Some(d) = first {
                        format!(
                            "{title}: {count}. {} at 0x{:x}: {}",
                            d.structure, d.address, d.message
                        )
                    } else {
                        format!("{title}: {count}.")
                    },
                });
            }
            report.sections.push(section);
        }
        let coverage = CheckSection::new(
            "Coverage",
            &format!(
                "{} objects, {} MethodTables and {} managed-reference slots examined; {} unparsed bytes. {}",
                self.objects,
                self.method_tables,
                self.reference_slots,
                self.unparsed_bytes,
                self.limitations.join(" ")
            ),
            &[],
        );
        report.sections.push(coverage);
        report
    }
}

pub fn verify_file(path: &Path, progress: impl Fn(&str, f64)) -> Result<HeapVerification> {
    progress("Reading the dump", 0.0);
    let directory = tempfile::tempdir()?;
    let dump = Arc::new(CoreDump::open(path, Some(directory.path()), None)?);
    let runtime = RuntimeInfo::find(&dump);
    progress("Walking the heap", 0.1);
    let heap = ManagedHeap::new(
        dump,
        runtime.as_ref(),
        ColumnStore::new(Some(directory.path().into()))?,
    )?;
    verify(&heap, runtime.as_ref(), progress)
}

impl HeapSnapshot {
    pub fn heap_verification(&self) -> Result<Arc<HeapVerification>> {
        self.saved("heap verification", || {
            verify(&self.heap, self.runtime.as_ref(), |_, _| {})
        })
    }

    pub fn verification_check(&self) -> Result<Arc<CheckReport>> {
        Ok(Arc::new(self.heap_verification()?.report(Some(&self.heap))))
    }
}

pub fn verify(
    heap: &ManagedHeap,
    runtime: Option<&RuntimeInfo>,
    progress: impl Fn(&str, f64),
) -> Result<HeapVerification> {
    let dump = &heap.dump;
    let mut result = HeapVerification {
        unparsed_bytes: heap.regions.iter().map(|r| r.unparsed_bytes).sum(),
        ..HeapVerification::default()
    };
    if heap.kind.gc != Gc::Satori
        && let Some(runtime) = runtime
    {
        crate::coreclr_heap::verify_links(dump, runtime, &mut result);
    }
    if heap.kind.gc == Gc::None {
        result.issue(
            VerificationKind::Unverified,
            "Managed heap",
            runtime.map(|r| r.start).unwrap_or(0),
            None,
            None,
            format!("No managed heap was identified: {}.", heap.kind.detection),
        );
        return Ok(result);
    }
    progress("Verifying runtime structures", 0.2);
    verify_regions(heap, &mut result);
    let descriptor = runtime.and_then(|r| r.descriptor.as_ref());
    verify_threads(dump, descriptor, &mut result);
    crate::satori_heap::verify_links(heap, &mut result);
    progress("Verifying MethodTables", 0.3);
    let mut descriptors = Vec::with_capacity(heap.types.len());
    for &mt in &heap.types {
        result.method_tables += 1;
        verify_method_table(dump, descriptor, mt, &mut result);
        match GcDesc::read_checked(mt, dump) {
            Ok(desc) => descriptors.push(Some(desc)),
            Err(error) => {
                result.issue(
                    if error.missing {
                        VerificationKind::MissingMemory
                    } else {
                        VerificationKind::Invalid
                    },
                    "GC descriptor",
                    error.address,
                    None,
                    dump.u64(error.address),
                    format!("MethodTable 0x{:x}: {}", mt.address, error.message),
                );
                descriptors.push(None);
            }
        }
    }
    let mut cursor = Cursor::new(dump, false);
    for i in 0..heap.count() {
        if i % 65536 == 0 {
            progress(
                "Verifying managed references",
                0.4 + 0.6 * i as f64 / heap.count().max(1) as f64,
            );
        }
        result.objects += 1;
        let object = heap.addresses[i];
        let Some(desc) = &descriptors[heap.type_ids[i] as usize] else {
            continue;
        };
        desc.slots(object, heap.size(i), heap.lengths[i], |slot| {
            result.reference_slots += 1;
            let Some(target) = cursor.u64(slot) else {
                result.issue(
                    VerificationKind::MissingMemory,
                    "Managed reference",
                    slot,
                    Some(object),
                    None,
                    "The dump does not contain this reference slot.",
                );
                return;
            };
            if target == 0 || heap.object_at(target).is_some() {
                return;
            }
            let (kind, message) = if target & 7 != 0 {
                (
                    VerificationKind::Invalid,
                    "The managed reference is not aligned to 8 bytes.",
                )
            } else if heap.object_containing(target).is_some() {
                (
                    VerificationKind::Invalid,
                    "The managed reference points inside an object, not to its start.",
                )
            } else if let Some(region) = heap.region_containing(target) {
                if heap.regions[region].unparsed_bytes != 0 || heap.regions[region].walk_stopped {
                    (
                        VerificationKind::Unverified,
                        "The target is in a heap range that could not be fully parsed.",
                    )
                } else {
                    (
                        VerificationKind::Invalid,
                        "The managed reference points to free space or an allocation gap.",
                    )
                }
            } else {
                (
                    VerificationKind::Invalid,
                    "The managed reference points outside the identified managed heap.",
                )
            };
            result.issue(
                kind,
                "Managed reference",
                slot,
                Some(object),
                Some(target),
                message,
            );
        });
    }
    result.limitations.push(
        "References in unreachable objects can be stale. GC relocation and partially initialized \
         allocations can also produce temporary inconsistencies."
            .into(),
    );
    result.limitations.push(
        "Sync blocks, thin locks, card tables and assembly metadata tables are not verified."
            .into(),
    );
    result.diagnostics.sort_by_key(|d| match d.kind {
        VerificationKind::Invalid => 0,
        VerificationKind::MissingMemory => 1,
        VerificationKind::Unverified => 2,
    });
    dump.check()?;
    Ok(result)
}

fn verify_method_table(
    dump: &CoreDump,
    descriptor: Option<&ContractDescriptor>,
    mt: MethodTableInfo,
    result: &mut HeapVerification,
) {
    let component = mt.component_size();
    if mt.flags & MethodTableInfo::HAS_COMPONENT_SIZE != 0 && component == 0 {
        result.issue(
            VerificationKind::Invalid,
            "MethodTable.ComponentSize",
            mt.address,
            None,
            Some(mt.flags as u64),
            "The type has components but its component size is zero.",
        );
    }
    if !(mt.base_size >= 24 && mt.base_size.is_multiple_of(8)
        || component == 2 && mt.base_size == 22)
    {
        result.issue(
            VerificationKind::Invalid,
            "MethodTable.BaseSize",
            mt.address + 4,
            None,
            Some(mt.base_size as u64),
            "The type has an invalid base size.",
        );
    }
    let class_offset = descriptor
        .and_then(|d| d.offset("MethodTable", "EEClassOrCanonMT"))
        .unwrap_or(40);
    let back_offset = descriptor.and_then(|d| d.offset("EEClass", "MethodTable"));
    let Some(class) = ee_class(dump, mt.address, class_offset, result) else {
        return;
    };
    let Some(back_offset) = back_offset else {
        if !result
            .limitations
            .iter()
            .any(|s| s.starts_with("EEClass back-pointers"))
        {
            result.limitations.push(
                "EEClass back-pointers were not verified: the dump has no EEClass layout contract."
                    .into(),
            );
        }
        return;
    };
    let Some(at) = class.checked_add(back_offset) else {
        result.issue(
            VerificationKind::Invalid,
            "MethodTable.EEClassOrCanonMT",
            mt.address + class_offset,
            None,
            Some(class),
            "The EEClass pointer overflows the address space.",
        );
        return;
    };
    let Some(back) = result.word(dump, at, "EEClass.MethodTable") else {
        return;
    };
    if back == mt.address {
        return;
    }
    let generic_or_array = mt.flags & 0x30 != 0 || mt.flags & 0x000c_0000 == 0x0008_0000;
    if back != 0 && back & 7 == 0 && generic_or_array {
        let Some(canonical_class) = ee_class(dump, back, class_offset, result) else {
            return;
        };
        if canonical_class == class {
            return;
        }
    }
    result.issue(
        VerificationKind::Invalid,
        "EEClass.MethodTable",
        at,
        None,
        Some(back),
        format!(
            "The EEClass does not refer back to MethodTable 0x{:x} or its canonical type.",
            mt.address
        ),
    );
}

fn ee_class(dump: &CoreDump, mt: u64, offset: u64, result: &mut HeapVerification) -> Option<u64> {
    let Some(mut at) = mt.checked_add(offset) else {
        result.issue(
            VerificationKind::Invalid,
            "MethodTable",
            mt,
            None,
            Some(offset),
            "The EEClass field address overflows the address space.",
        );
        return None;
    };
    let value = result.word(dump, at, "MethodTable.EEClassOrCanonMT")?;
    let class = if value & 1 != 0 {
        let canonical = value & !1;
        if canonical == mt || canonical & 7 != 0 {
            result.issue(
                VerificationKind::Invalid,
                "MethodTable.EEClassOrCanonMT",
                at,
                None,
                Some(value),
                "The canonical MethodTable pointer is invalid.",
            );
            return None;
        }
        let Some(field) = canonical.checked_add(offset) else {
            result.issue(
                VerificationKind::Invalid,
                "MethodTable.EEClassOrCanonMT",
                at,
                None,
                Some(value),
                "The canonical MethodTable field address overflows the address space.",
            );
            return None;
        };
        at = field;
        result.word(dump, at, "Canonical MethodTable.EEClass")?
    } else {
        value
    };
    if class == 0 || class & 7 != 0 {
        result.issue(
            VerificationKind::Invalid,
            "MethodTable.EEClassOrCanonMT",
            at,
            None,
            Some(class),
            "The EEClass pointer is null or misaligned.",
        );
        None
    } else {
        Some(class)
    }
}

fn verify_regions(heap: &ManagedHeap, result: &mut HeapVerification) {
    let dump = &heap.dump;
    for (index, r) in heap.regions.iter().enumerate() {
        if index > 0 && heap.regions[index - 1].end > r.start {
            result.issue(
                VerificationKind::Invalid,
                "GC region",
                r.start,
                None,
                Some(heap.regions[index - 1].end),
                "GC regions overlap.",
            );
        }
        if r.used > r.committed || r.committed > r.end || r.object_start > r.end {
            result.issue(
                VerificationKind::Invalid,
                "GC region",
                r.start,
                None,
                Some(r.used),
                "Used, committed and reserved boundaries are inconsistent.",
            );
        }
        if let Some(object) = r.first_unparsed_object {
            result.issue(
                VerificationKind::Unverified,
                "Object.MethodTable",
                object,
                Some(object),
                dump.u64(object),
                "A zero MethodTable interrupts the heap walk. This can be an unfinished \
                 allocation or a damaged object.",
            );
        }
        let Some(object) = r.walk_stopped_at else {
            continue;
        };
        if r.walk_stopped_after_gap {
            result.issue(
                VerificationKind::Unverified,
                "Heap walk",
                object,
                None,
                None,
                "The next object boundary could not be established after an unparsed range.",
            );
            continue;
        }
        let Some(raw) = dump.u64(object) else {
            result.issue(
                VerificationKind::MissingMemory,
                "Object.MethodTable",
                object,
                Some(object),
                None,
                "The dump does not contain the object's MethodTable pointer.",
            );
            continue;
        };
        let mt = raw & !7;
        let Some((flags, base)) = dump
            .u32(mt)
            .zip(mt.checked_add(4).and_then(|p| dump.u32(p)))
        else {
            let kind = VerificationKind::MissingMemory;
            result.issue(
                kind,
                "Object.MethodTable",
                object,
                Some(object),
                Some(raw),
                "The MethodTable pointer does not lead to readable type information.",
            );
            continue;
        };
        let component = if flags & MethodTableInfo::HAS_COMPONENT_SIZE != 0 {
            flags & 0xffff
        } else {
            0
        };
        let length = if component != 0 {
            dump.u32(object + 8)
        } else {
            Some(0)
        };
        if let Some(length) = length {
            let size = if component != 0 {
                (base as u64 + component as u64 * length as u64 + 7) & !7
            } else {
                base as u64
            };
            result.issue(
                VerificationKind::Invalid,
                "Object size",
                if component != 0 { object + 8 } else { mt + 4 },
                Some(object),
                Some(if component == 0 {
                    base as u64
                } else {
                    length as u64
                }),
                format!(
                    "Base size {base}, component size {component} and length {length} imply \
                     {size} bytes. The size is invalid or extends beyond used memory (0x{:x}); \
                     the region walk stopped here.",
                    r.used.min(r.committed).min(r.end)
                ),
            );
        } else {
            result.issue(
                VerificationKind::MissingMemory,
                "Array length",
                object + 8,
                Some(object),
                None,
                "The dump does not contain the array length.",
            );
        }
    }
}

fn verify_threads(
    dump: &CoreDump,
    descriptor: Option<&ContractDescriptor>,
    result: &mut HeapVerification,
) {
    let Some((store_address, first, next)) = descriptor.and_then(|d| {
        Some((
            *d.globals.get("ThreadStore")?,
            d.offset("ThreadStore", "FirstThreadLink")?,
            d.offset("Thread", "LinkNext")?,
        ))
    }) else {
        result
            .limitations
            .push("The thread list was not verified: its layout contract is unavailable.".into());
        return;
    };
    let Some(store) = result.word(dump, store_address, "ThreadStore") else {
        return;
    };
    let Some(mut at) = store.checked_add(first) else {
        return;
    };
    let mut seen = HashSet::new();
    while let Some(link) = result.word(dump, at, "Thread.LinkNext") {
        if link == 0 {
            break;
        }
        if link & 7 != 0 || link < next || !seen.insert(link) {
            result.issue(
                VerificationKind::Invalid,
                "Thread.LinkNext",
                at,
                None,
                Some(link),
                "The runtime thread list contains a cycle or an invalid link.",
            );
            break;
        }
        if seen.len() >= 100_000 {
            result.issue(
                VerificationKind::Unverified,
                "Thread.LinkNext",
                at,
                None,
                Some(link),
                "The thread list exceeds the verification limit of 100,000 entries.",
            );
            break;
        }
        at = link;
    }
}
