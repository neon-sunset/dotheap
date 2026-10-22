use crate::{
    Error, Result,
    core_dump::{CoreDump, Cursor},
    heap::{ManagedHeap, MethodTableInfo},
};

pub enum GcDesc {
    None,
    Series(Vec<(u64, u64)>),
    Repeating { offset: u64, runs: Vec<(u64, u64)> },
}

pub struct GcDescError {
    pub address: u64,
    pub missing: bool,
    pub message: &'static str,
}

impl GcDesc {
    pub fn read(t: MethodTableInfo, dump: &CoreDump) -> Result<Self> {
        Self::read_checked(t, dump).map_err(|e| Error(format!(
            "the GC descriptor of MethodTable 0x{:x} is missing or not valid at 0x{:x}: {} Use 'dotheap verify <dump>' to inspect heap integrity.",
            t.address, e.address, e.message
        )))
    }

    pub fn read_checked(
        t: MethodTableInfo,
        dump: &CoreDump,
    ) -> std::result::Result<Self, GcDescError> {
        if !t.contains_pointers() {
            return Ok(Self::None);
        }
        let (mt, base) = (t.address, t.base_size as u64);
        let invalid = |address, message| GcDescError {
            address,
            missing: false,
            message,
        };
        let word = |address| {
            dump.u64(address).ok_or(GcDescError {
                address,
                missing: true,
                message: "The dump does not contain this GC descriptor field.",
            })
        };
        if mt < 24 || base < 24 {
            return Err(invalid(
                mt,
                "The MethodTable or base size is too small for a GC descriptor.",
            ));
        }
        let count = word(mt - 8)? as i64;
        if count > 0 {
            if count as u64 > base / 8 || count as u64 > (mt - 8) / 16 {
                return Err(invalid(
                    mt - 8,
                    "The number of GC descriptor series exceeds the object layout.",
                ));
            }
            let mut series = Vec::new();
            for i in 0..count as u64 {
                let at = mt - 24 - i * 16;
                let size = word(at)? as i64;
                let offset = word(at + 8)?;
                if offset < 8 || offset > base - 8 || offset % 8 != 0 {
                    return Err(invalid(
                        at + 8,
                        "The GC descriptor series has an invalid offset.",
                    ));
                }
                if size < -(base as i64) || size > 0 {
                    return Err(invalid(
                        at,
                        "The GC descriptor series has an invalid size adjustment.",
                    ));
                }
                let adjustment = (-size) as u64;
                if base - adjustment > base - 8 - offset || !(base - adjustment).is_multiple_of(8) {
                    return Err(invalid(
                        at,
                        "The GC descriptor series extends beyond the object or is misaligned.",
                    ));
                }
                series.push((offset, adjustment));
            }
            series.sort_by_key(|s| s.0);
            if series.windows(2).any(|s| s[0].0 + base - s[0].1 > s[1].0) {
                return Err(invalid(mt - 8, "GC descriptor series overlap."));
            }
            Ok(Self::Series(series))
        } else {
            let component = t.component_size() as u64;
            if count >= 0 || count < -((component / 8) as i64) || (-count) as u64 > (mt - 16) / 8 {
                return Err(invalid(
                    mt - 8,
                    "The GC descriptor has an invalid repeating-series count.",
                ));
            }
            let offset = word(mt - 16)?;
            if offset < base - 8 || offset - (base - 8) >= component || offset % 8 != 0 {
                return Err(invalid(
                    mt - 16,
                    "The GC descriptor starts outside the array element or is misaligned.",
                ));
            }
            let (mut runs, mut stride) = (Vec::new(), 0);
            for i in 0..(-count) as u64 {
                let at = mt - 24 - i * 8;
                let run = word(at)?;
                let pointers = run & 0xffff_ffff;
                let skip = run >> 32;
                if pointers == 0
                    || skip % 8 != 0
                    || stride + pointers * 8 > component - (offset - (base - 8))
                {
                    return Err(invalid(
                        at,
                        "The repeating GC descriptor extends beyond the array element or is misaligned.",
                    ));
                }
                stride += pointers * 8 + skip;
                runs.push((pointers, skip));
            }
            if stride != component {
                return Err(invalid(
                    mt - 8,
                    "The repeating GC descriptor does not cover exactly one array element.",
                ));
            }
            Ok(Self::Repeating { offset, runs })
        }
    }

    pub fn scan(
        &self,
        heap: &ManagedHeap,
        i: usize,
        cursor: &mut Cursor<'_>,
        mut body: impl FnMut(i32),
    ) {
        self.slots(heap.addresses[i], heap.size(i), heap.lengths[i], |slot| {
            if let Some(v) = cursor.u64(slot)
                && v >= heap.low
                && v < heap.high
                && v & 7 == 0
                && let Some(target) = heap.object_at(v)
            {
                body(target as i32);
            }
        });
    }

    pub fn slots(&self, address: u64, size: u64, length: u32, mut body: impl FnMut(u64)) {
        let mut scan = |start, end| {
            for slot in (start..end).step_by(8) {
                body(slot);
            }
        };
        match self {
            Self::None => {}
            Self::Series(series) => {
                for &(offset, adjustment) in series {
                    if let Some(start) = address.checked_add(offset)
                        && let Some(size) = size.checked_sub(adjustment)
                        && let Some(end) = start.checked_add(size)
                    {
                        scan(start, end);
                    }
                }
            }
            Self::Repeating { offset, runs } => {
                let Some(mut slot) = address.checked_add(*offset) else {
                    return;
                };
                for _ in 0..length {
                    for &(pointers, skip) in runs {
                        let Some(end) = slot.checked_add(pointers * 8) else {
                            return;
                        };
                        scan(slot, end);
                        let Some(next) = end.checked_add(skip) else {
                            return;
                        };
                        slot = next;
                    }
                }
            }
        }
    }
}
