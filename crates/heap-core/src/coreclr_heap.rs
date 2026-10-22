use crate::{
    core_dump::CoreDump,
    heap::{FinalizationQueue, Gc, GcState, HeapDiscovery, HeapKind, Region, pages_for},
    runtime::{ContractDescriptor, RuntimeInfo},
    verification::{HeapVerification, VerificationKind},
};
use std::collections::HashSet;

const VARIABLES: &[&str] = &[
    "build_variant",
    "built_with_svr",
    "gc_global_mechanisms",
    "generation_table",
    "max_gen",
    "mark_array",
    "current_c_gc_state",
    "ephemeral_heap_segment",
    "saved_sweep_ephemeral_seg",
    "saved_sweep_ephemeral_start",
    "background_saved_lowest_address",
    "background_saved_highest_address",
    "alloc_allocated",
    "next_sweep_obj",
    "oom_info",
    "finalize_queue",
    "internal_root_array",
    "internal_root_array_index",
    "heap_analyze_success",
    "n_heaps",
    "g_heaps",
    "gc_structures_invalid_cnt",
    "interesting_data_per_heap",
    "compact_reasons_per_heap",
    "expand_mechanisms_per_heap",
    "interesting_mechanism_bits_per_heap",
    "handle_table_map",
    "gc_heap_field_offsets",
    "generation_field_offsets",
    "bookkeeping_start",
    "global_regions_to_decommit",
    "global_free_huge_regions",
    "free_regions",
    "freeable_soh_segment",
    "freeable_uoh_segment",
];

struct Heap {
    table: u64,
    allocated: u64,
    ephemeral: u64,
    finalize: u64,
    free_regions: u64,
}

pub(crate) fn find(dump: &CoreDump, runtime: &RuntimeInfo) -> Option<HeapDiscovery> {
    discover(dump, runtime, None)
}

pub(crate) fn verify_links(dump: &CoreDump, runtime: &RuntimeInfo, result: &mut HeapVerification) {
    discover(dump, runtime, Some(result));
}

fn discover(
    dump: &CoreDump,
    runtime: &RuntimeInfo,
    mut diagnostics: Option<&mut HeapVerification>,
) -> Option<HeapDiscovery> {
    let g = runtime.gc_dac_vars?;
    let generation_size = dump.u64(g.checked_add(8)?)?;
    let generation_count = dump.u64(g.checked_add(16)?)?;
    if !(1..=8).contains(&generation_count) || !(24..=4096).contains(&generation_size) {
        return None;
    }
    let minor = dump.u8(g + 1).unwrap_or(0);
    let variable = |name: &str| {
        g.checked_add(24 + VARIABLES.iter().position(|&v| v == name).unwrap() as u64 * 8)
            .and_then(|at| dump.u64(at))
            .unwrap_or(0)
    };
    let server = variable("n_heaps") != 0 && variable("g_heaps") != 0;
    let regions_build = dump.u8(variable("build_variant")).unwrap_or(0) & 1 != 0;
    let gen_offsets = offsets(dump, variable("generation_field_offsets"), 3)?;
    let mut heaps = Vec::new();
    if server {
        let count = dump.u32(variable("n_heaps"))?;
        if count == 0 || count >= 4096 {
            return None;
        }
        let array = dump.u64(variable("g_heaps"))?;
        let offsets = offsets(dump, variable("gc_heap_field_offsets"), 22)?;
        for i in 0..count {
            let Some(h) = array
                .checked_add(i as u64 * 8)
                .and_then(|at| dump.u64(at))
                .filter(|&h| h != 0)
            else {
                continue;
            };
            let field = |i| h.checked_add(u64::try_from(offsets[i]).ok()?);
            heaps.push(Heap {
                table: field(18)?,
                allocated: dump.u64(field(0)?).unwrap_or(0),
                ephemeral: dump.u64(field(1)?).unwrap_or(0),
                finalize: dump.u64(field(2)?).unwrap_or(0),
                free_regions: if minor >= 2 {
                    field(21).unwrap_or(0)
                } else {
                    0
                },
            });
        }
    } else {
        heaps.push(Heap {
            table: variable("generation_table"),
            allocated: dump.u64(variable("alloc_allocated")).unwrap_or(0),
            ephemeral: dump.u64(variable("ephemeral_heap_segment")).unwrap_or(0),
            finalize: dump.u64(variable("finalize_queue")).unwrap_or(0),
            free_regions: if minor >= 2 {
                variable("free_regions")
            } else {
                0
            },
        });
    }
    let (mut regions, mut unparseable, mut seen) = (Vec::new(), Vec::new(), HashSet::new());
    let names = ["gen0", "gen1", "gen2", "LOH", "POH"];
    for (h, heap) in heaps.iter().enumerate() {
        for generation in 0..generation_count {
            let address = heap.table.checked_add(generation * generation_size)?;
            if let Some(context) = address
                .checked_add(u64::try_from(gen_offsets[0]).ok()?)
                .and_then(|at| allocation_context(dump, at))
            {
                unparseable.push(context);
            }
            let mut segment = dump
                .u64(address.checked_add(u64::try_from(gen_offsets[1]).ok()?)?)
                .unwrap_or(0);
            let mut chain = HashSet::new();
            let mut previous_link = address.checked_add(u64::try_from(gen_offsets[1]).ok()?)?;
            while segment != 0 && seen.insert(segment) && regions.len() < 10_000_000 {
                if diagnostics.is_some() {
                    chain.insert(segment);
                }
                if let Some(report) = diagnostics.as_deref_mut() {
                    verify_segment(dump, segment, report);
                }
                let Some((mem, reserved, allocated, committed, flags, next)) =
                    segment_fields(dump, segment)
                else {
                    break;
                };
                let used = (if segment == heap.ephemeral {
                    heap.allocated
                } else {
                    allocated
                })
                .max(mem)
                .min(reserved);
                let frozen = flags & 1 != 0;
                let mut add =
                    |start, end, object_start, used, committed, generation, name: String| {
                        if end <= start {
                            return;
                        }
                        regions.push(Region {
                            index: regions.len(),
                            start,
                            end,
                            object_start,
                            generation,
                            generation_name: name.clone(),
                            queue: if server {
                                format!("{name}, heap {h}")
                            } else {
                                name
                            },
                            gc_heap: if server { h as i32 } else { -1 },
                            used,
                            committed,
                            immortal: frozen,
                            ..Region::default()
                        });
                    };
                if frozen {
                    add(mem, reserved, mem, used, committed, 2, "NonGC heap".into());
                } else if regions_build || generation > 2 {
                    add(
                        mem & !4095,
                        reserved,
                        mem,
                        used,
                        committed,
                        generation.min(2) as i32,
                        names
                            .get(generation as usize)
                            .map(|s| (*s).to_owned())
                            .unwrap_or_else(|| format!("gen{generation}")),
                    );
                } else if segment == heap.ephemeral
                    && gen_offsets[2] >= 0
                    && let Some(gen0) = heap
                        .table
                        .checked_add(gen_offsets[2] as u64)
                        .and_then(|at| dump.u64(at))
                    && let Some(gen1) = heap
                        .table
                        .checked_add(generation_size + gen_offsets[2] as u64)
                        .and_then(|at| dump.u64(at))
                    && gen1 >= mem
                    && gen0 >= gen1
                    && gen0 <= used
                {
                    add(
                        mem & !4095,
                        gen1,
                        mem,
                        gen1,
                        committed.min(gen1),
                        2,
                        "gen2".into(),
                    );
                    add(
                        gen1,
                        gen0,
                        gen1,
                        gen0,
                        committed.min(gen0),
                        1,
                        "gen1".into(),
                    );
                    add(gen0, reserved, gen0, used, committed, 0, "gen0".into());
                } else {
                    add(
                        mem & !4095,
                        reserved,
                        mem,
                        used,
                        committed,
                        2,
                        "gen2".into(),
                    );
                }
                previous_link = segment.checked_add(48)?;
                segment = next;
            }
            if chain.contains(&segment)
                && let Some(report) = diagnostics.as_deref_mut()
            {
                report.issue(
                    VerificationKind::Invalid,
                    "CoreCLR segment next",
                    previous_link,
                    None,
                    Some(segment),
                    "The GC segment list contains a cycle.",
                );
            }
        }
    }
    let mut gc = GcState::default();
    if regions_build && minor >= 2 {
        let kinds = dump.u32(g + 24 + 35 * 8 + 4).unwrap_or(3).min(3) as usize;
        let kind_names = ["basic", "large", "huge"];
        let mut add_list = |list: u64, heap: i32, queue: String| {
            let Some(count) = dump.u64(list).filter(|&n| n < 1_000_000) else {
                return;
            };
            let mut segment = list
                .checked_add(40)
                .and_then(|at| dump.u64(at))
                .unwrap_or(0);
            let mut walked = 0;
            while segment != 0 && walked < count && seen.insert(segment) {
                walked += 1;
                let Some((mem, reserved, _, committed, _, next)) = segment_fields(dump, segment)
                else {
                    break;
                };
                regions.push(Region {
                    index: regions.len(),
                    start: mem & !4095,
                    end: reserved,
                    object_start: mem,
                    generation: -1,
                    generation_name: "free".into(),
                    queue: queue.clone(),
                    gc_heap: heap,
                    used: mem,
                    committed,
                    ..Region::default()
                });
                segment = next;
            }
        };
        for (h, heap) in heaps
            .iter()
            .enumerate()
            .filter(|(_, h)| h.free_regions != 0)
        {
            for (k, kind) in kind_names.iter().take(kinds).enumerate() {
                if let Some(list) = heap.free_regions.checked_add(k as u64 * 56) {
                    add_list(
                        list,
                        if server { h as i32 } else { -1 },
                        format!(
                            "free, {kind}{}",
                            if server {
                                format!(", heap {h}")
                            } else {
                                String::new()
                            }
                        ),
                    );
                }
            }
        }
        let decommit = variable("global_regions_to_decommit");
        if decommit != 0 {
            for k in 0..kinds {
                if let Some(list) = decommit.checked_add(k as u64 * 56) {
                    add_list(list, -1, "waiting for decommit".into());
                }
            }
        }
        let huge = variable("global_free_huge_regions");
        if huge != 0 {
            add_list(huge, -1, "free, huge, global".into());
        }
    }
    if minor >= 4
        && let Some(p) = dump.u64(g + 24 + 35 * 8 + 16).filter(|&p| p != 0)
    {
        gc.dynamic_adaptation = dump.u32(p).map(|v| v == 1);
    }
    gc.background = dump.u8(variable("build_variant")).unwrap_or(0) & 2 == 0;
    if regions.is_empty() {
        return None;
    }
    let mut finalization = Vec::new();
    let fills = generation_count + 2;
    for heap in heaps.iter().filter(|h| h.finalize != 0) {
        let f = heap.finalize;
        if let Some(report) = diagnostics.as_deref_mut() {
            let array_field = f.checked_add(fills * 8)?;
            if let Some(array) = dump.u64(array_field) {
                let mut previous = array;
                for i in 0..fills {
                    let field = f.checked_add(i * 8)?;
                    match dump.u64(field) {
                        Some(value) if value >= previous && value & 7 == 0 => previous = value,
                        Some(value) => report.issue(
                            VerificationKind::Invalid,
                            "CoreCLR finalization queue",
                            field,
                            None,
                            Some(value),
                            "Finalization queue boundaries are misaligned or out of order.",
                        ),
                        None => report.issue(
                            VerificationKind::MissingMemory,
                            "CoreCLR finalization queue",
                            field,
                            None,
                            None,
                            "The dump does not contain this queue boundary.",
                        ),
                    }
                }
            }
        }
        if let (Some(array), Some(ready), Some(end)) = (
            f.checked_add(fills * 8).and_then(|p| dump.u64(p)),
            f.checked_add((generation_count - 1) * 8)
                .and_then(|p| dump.u64(p)),
            f.checked_add((fills - 1) * 8).and_then(|p| dump.u64(p)),
        ) && array != 0
            && ready >= array
            && end >= ready
        {
            finalization.push(FinalizationQueue {
                start: array,
                end,
                ready_start: ready,
                ready_end: end,
                registered_start: array,
                registered_end: ready,
                stride: 8,
            });
        }
    }
    unparseable.extend(thread_allocation_contexts(
        dump,
        runtime.descriptor.as_ref(),
    ));
    let mode = if server {
        format!(
            "server GC, {} heap{}",
            heaps.len(),
            if heaps.len() == 1 { "" } else { "s" }
        )
    } else {
        "workstation GC".into()
    };
    let layout = format!(
        "{mode}, {}{}",
        if regions_build { "regions" } else { "segments" },
        if gc.dynamic_adaptation == Some(true) {
            ", DATAS"
        } else {
            ""
        }
    );
    Some(HeapDiscovery {
        pages: pages_for(&regions),
        regions,
        kind: HeapKind {
            gc: Gc::CoreClr,
            layout,
            detection: format!(
                "GcDacVars {}.{minor} at 0x{g:x} in libcoreclr; field offsets from the GC",
                dump.u8(g).unwrap_or(0)
            ),
            stock: Some(gc),
        },
        unparseable,
        finalization,
    })
}

fn offsets(dump: &CoreDump, address: u64, count: usize) -> Option<Vec<i32>> {
    if address == 0 {
        return None;
    }
    Some(
        (0..count)
            .map(|i| {
                address
                    .checked_add(i as u64 * 4)
                    .and_then(|p| dump.u32(p))
                    .map(|v| v as i32)
                    .unwrap_or(-1)
            })
            .collect(),
    )
}

fn segment_fields(dump: &CoreDump, segment: u64) -> Option<(u64, u64, u64, u64, u64, u64)> {
    let mem = dump.u64(segment.checked_add(32)?)?;
    let reserved = dump.u64(segment.checked_add(16)?)?;
    if reserved <= mem {
        return None;
    }
    Some((
        mem,
        reserved,
        dump.u64(segment).unwrap_or(mem),
        dump.u64(segment.checked_add(8)?).unwrap_or(mem),
        dump.u64(segment.checked_add(40)?).unwrap_or(0),
        dump.u64(segment.checked_add(48)?).unwrap_or(0),
    ))
}

fn allocation_context(dump: &CoreDump, address: u64) -> Option<(u64, u64)> {
    let pointer = dump.u64(address)?;
    let limit = dump.u64(address.checked_add(8)?)?;
    if pointer == 0 || limit < pointer {
        return None;
    }
    Some((pointer, limit.checked_add(24)?))
}

pub fn thread_allocation_contexts(
    dump: &CoreDump,
    descriptor: Option<&ContractDescriptor>,
) -> Vec<(u64, u64)> {
    let Some(d) = descriptor else {
        return vec![];
    };
    d.allocation_contexts(dump)
        .into_iter()
        .filter_map(|(context, _)| allocation_context(dump, context))
        .collect()
}

fn verify_segment(dump: &CoreDump, segment: u64, report: &mut HeapVerification) {
    let mut values = [0; 6];
    for (i, offset) in [0, 8, 16, 32, 40, 48].into_iter().enumerate() {
        let Some(field) = segment.checked_add(offset) else {
            return;
        };
        let Some(value) = dump.u64(field) else {
            report.issue(
                VerificationKind::MissingMemory,
                "CoreCLR segment",
                field,
                None,
                None,
                "The dump does not contain this GC segment field.",
            );
            return;
        };
        values[i] = value;
    }
    let [allocated, committed, reserved, mem, _, _] = values;
    if allocated < mem || allocated > committed {
        report.issue(
            VerificationKind::Invalid,
            "CoreCLR segment allocated",
            segment,
            None,
            Some(allocated),
            "The allocated boundary is outside the segment's committed object range.",
        );
    }
    if committed < mem || committed > reserved {
        report.issue(
            VerificationKind::Invalid,
            "CoreCLR segment committed",
            segment + 8,
            None,
            Some(committed),
            "The committed boundary is outside the segment's reserved object range.",
        );
    }
    if reserved <= mem {
        report.issue(
            VerificationKind::Invalid,
            "CoreCLR segment reserved",
            segment + 16,
            None,
            Some(reserved),
            "The reserved boundary does not follow the start of the segment.",
        );
    }
}
