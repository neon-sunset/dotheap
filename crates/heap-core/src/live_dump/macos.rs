use super::{
    CaptureError, CaptureOptions, CaptureResult, Result,
    core_file::{CoreFile, check_free_space},
    meter::Meter,
};
use crate::smaps::{CapturedResidency, MACH_NOTE_OWNER, SmapsMapping};
use std::{ffi::CStr, path::Path, ptr, time::Instant};

const ARM_THREAD_STATE64: i32 = 6;
const ARM_THREAD_STATE64_COUNT: usize = 68;
const VM_REGION_SUBMAP_INFO_V0_COUNT_64: u32 = 16;
const VM_MEMORY_IOKIT: u32 = 21;

#[repr(C, packed(4))]
#[derive(Default)]
struct RegionInfo {
    protection: i32,
    max_protection: i32,
    inheritance: i32,
    offset: u64,
    user_tag: u32,
    pages_resident: u32,
    pages_shared_now_private: u32,
    pages_swapped_out: u32,
    pages_dirtied: u32,
    ref_count: u32,
    shadow_depth: u16,
    external_pager: u8,
    share_mode: u8,
    is_submap: i32,
    behavior: i32,
    object_id: u32,
    user_wired_count: u16,
    flags: u16,
}

const _: () = assert!(size_of::<RegionInfo>() == VM_REGION_SUBMAP_INFO_V0_COUNT_64 as usize * 4);

unsafe extern "C" {
    static mach_task_self_: u32;
    fn mach_port_deallocate(task: u32, name: u32) -> i32;
    fn task_generate_corpse(task: u32, corpse: *mut u32) -> i32;
    fn task_suspend2(task: u32, token: *mut u32) -> i32;
    fn task_resume2(token: u32) -> i32;
    fn mach_vm_region_recurse(
        task: u32,
        address: *mut u64,
        size: *mut u64,
        depth: *mut u32,
        info: *mut RegionInfo,
        count: *mut u32,
    ) -> i32;
    fn mach_vm_read_overwrite(task: u32, address: u64, size: u64, data: u64, read: *mut u64)
    -> i32;
    fn thread_get_state(thread: u32, flavor: i32, state: *mut u32, count: *mut u32) -> i32;
}

struct Port(u32);
impl Drop for Port {
    fn drop(&mut self) {
        unsafe {
            mach_port_deallocate(mach_task_self_, self.0);
        }
    }
}

struct Suspension(u32);
impl Drop for Suspension {
    fn drop(&mut self) {
        unsafe {
            // task_resume2 consumes the send-once right.
            task_resume2(self.0);
        }
    }
}

fn mach_error(operation: &str, code: i32) -> CaptureError {
    let message = unsafe { CStr::from_ptr(libc::mach_error_string(code)) }.to_string_lossy();
    CaptureError::new(format!("{operation}: {message} ({code})"))
}

fn checked(operation: &str, code: i32) -> Result<()> {
    if code == libc::KERN_SUCCESS {
        Ok(())
    } else {
        Err(mach_error(operation, code))
    }
}

struct Region {
    address: u64,
    size: u64,
    protection: i32,
    max_protection: i32,
    tag: u32,
    residency: SmapsMapping,
}

fn regions(task: u32) -> Result<Vec<Region>> {
    let mut result = Vec::new();
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as u64;
    let (mut address, mut depth) = (0, 0);
    loop {
        let mut info = RegionInfo::default();
        let mut size = 0;
        let mut count = VM_REGION_SUBMAP_INFO_V0_COUNT_64;
        let code = unsafe {
            mach_vm_region_recurse(
                task,
                &mut address,
                &mut size,
                &mut depth,
                &mut info,
                &mut count,
            )
        };
        if code == libc::KERN_INVALID_ADDRESS {
            break;
        }
        checked("cannot enumerate memory regions", code)?;
        if size == 0 || count < VM_REGION_SUBMAP_INFO_V0_COUNT_64 {
            return Err(CaptureError::terminal(
                "the kernel returned an incomplete memory region",
            ));
        }
        if info.is_submap != 0 {
            depth += 1;
            continue;
        }
        let end = address
            .checked_add(size)
            .ok_or_else(|| CaptureError::terminal("a memory region wraps the address space"))?;
        result.push(Region {
            address,
            size,
            protection: info.protection,
            max_protection: info.max_protection,
            tag: info.user_tag,
            residency: SmapsMapping {
                start: address,
                end,
                path: format!("vmmap: {}", region_name(&info)),
                rss: u64::from(info.pages_resident) * page_size,
                anon_huge: 0,
                private_bytes: u64::from(if matches!(info.share_mode, 2 | 6) {
                    info.pages_resident
                } else {
                    info.pages_shared_now_private
                }) * page_size,
                swap: u64::from(info.pages_swapped_out) * page_size,
                readable: info.protection & libc::VM_PROT_READ != 0,
            },
        });
        address = end;
    }
    Ok(result)
}

fn region_name(info: &RegionInfo) -> String {
    match info.user_tag {
        1..=4 | 6..=9 | 11..=13 => "malloc".into(),
        20 => "Mach messages".into(),
        21 => "IOKit".into(),
        22 => "VM reclaim".into(),
        30 => "Thread stacks".into(),
        31 => "Guard pages".into(),
        32 => "Shared page tables".into(),
        33 => "Shared libraries".into(),
        35 => "Unshared page tables".into(),
        60 | 61 => "Dynamic loader".into(),
        73 => "OS allocations".into(),
        0 if info.external_pager != 0 => "Mapped files".into(),
        0 => "Anonymous memory".into(),
        tag => format!("VM tag {tag}"),
    }
}

fn threads(task: u32) -> Result<Vec<[u32; ARM_THREAD_STATE64_COUNT]>> {
    let mut array = ptr::null_mut();
    let mut count = 0;
    checked("cannot enumerate threads", unsafe {
        libc::task_threads(task, &mut array, &mut count)
    })?;
    let ports: Vec<_> = if count == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(array, count as usize) }
            .iter()
            .map(|&port| Port(port))
            .collect()
    };
    if !array.is_null() {
        unsafe {
            libc::vm_deallocate(
                mach_task_self_,
                array as usize,
                count as usize * size_of::<u32>(),
            );
        }
    }
    if ports.is_empty() {
        return Err(CaptureError::terminal("the process has no threads"));
    }
    ports
        .iter()
        .map(|port| {
            let mut state = [0; ARM_THREAD_STATE64_COUNT];
            let mut count = ARM_THREAD_STATE64_COUNT as u32;
            checked(
                "cannot read ARM64 registers (translated x86 processes are not supported)",
                unsafe {
                    thread_get_state(port.0, ARM_THREAD_STATE64, state.as_mut_ptr(), &mut count)
                },
            )?;
            if count as usize != ARM_THREAD_STATE64_COUNT {
                return Err(CaptureError::terminal(
                    "the kernel returned incomplete ARM64 registers",
                ));
            }
            Ok(state)
        })
        .collect()
}

fn header(regions: &[Region], threads: &[[u32; ARM_THREAD_STATE64_COUNT]], note: &[u8]) -> Vec<u8> {
    let commands = regions.len() * 72 + threads.len() * (16 + ARM_THREAD_STATE64_COUNT * 4) + 40;
    let size = 32 + commands;
    let mut bytes = Vec::with_capacity(size);
    for word in [
        0xfeedfacf,
        0x100000c,
        0,
        4,
        (regions.len() + threads.len() + 1) as u32,
        commands as u32,
        0,
        0,
    ] {
        bytes.extend_from_slice(&word.to_le_bytes());
    }
    let mut offset = size as u64;
    for region in regions {
        bytes.extend_from_slice(&0x19u32.to_le_bytes());
        bytes.extend_from_slice(&72u32.to_le_bytes());
        bytes.extend_from_slice(&[0; 16]);
        for value in [region.address, region.size, offset, region.size] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for value in [region.max_protection, region.protection, 0, 0] {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        offset += region.size;
    }
    for state in threads {
        for word in [
            4,
            (16 + ARM_THREAD_STATE64_COUNT * 4) as u32,
            ARM_THREAD_STATE64 as u32,
            ARM_THREAD_STATE64_COUNT as u32,
        ] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        for word in state {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
    }
    bytes.extend_from_slice(&0x31u32.to_le_bytes());
    bytes.extend_from_slice(&40u32.to_le_bytes());
    bytes.extend_from_slice(MACH_NOTE_OWNER);
    bytes.extend_from_slice(&offset.to_le_bytes());
    bytes.extend_from_slice(&(note.len() as u64).to_le_bytes());
    bytes
}

pub(super) fn write(pid: i32, path: &Path, options: &CaptureOptions<'_>) -> Result<CaptureResult> {
    if pid <= 0 || pid == unsafe { libc::getpid() } {
        return Err(CaptureError::terminal(
            "choose another process with a positive PID",
        ));
    }
    options.check_cancelled()?;
    let path = std::path::absolute(path)?;
    crate::live_process::ensure_absent(&path).map_err(|e| CaptureError::terminal(e.0))?;
    check_free_space(&path)?;
    let mut task = 0;
    let code = unsafe { libc::task_for_pid(mach_task_self_, pid, &mut task) };
    if code != libc::KERN_SUCCESS {
        return Err(CaptureError::new(format!(
            "macOS denied access to process {pid}. Direct capture needs debugger access to the target. \
             Protected applications may not permit it, even as root. {}",
            mach_error("task_for_pid", code)
        )));
    }
    let task = Port(task);
    let physical_footprint = crate::live_process::memory_bytes(pid);
    let started = Instant::now();
    let (snapshot, suspension, pause);
    let source = if options.pause {
        options.report("pausing the process for capture");
        let mut token = 0;
        checked("cannot pause the process", unsafe {
            task_suspend2(task.0, &mut token)
        })?;
        suspension = Some(Suspension(token));
        snapshot = None;
        pause = Instant::now();
        task.0
    } else {
        options.report("creating a kernel snapshot");
        let mut corpse = 0;
        let code = unsafe { task_generate_corpse(task.0, &mut corpse) };
        if code != libc::KERN_SUCCESS {
            let mut error = mach_error("the kernel could not create a snapshot", code);
            error.pause_can_capture = true;
            return Err(error);
        }
        snapshot = Some(Port(corpse));
        suspension = None;
        pause = Instant::now();
        corpse
    };
    let meter = Meter::new();
    let result: Result<_> = std::thread::scope(|scope| {
        if let Some(status) = options.status {
            scope.spawn(|| meter.report(status));
        }
        struct StopMeter<'a>(&'a Meter);
        impl Drop for StopMeter<'_> {
            fn drop(&mut self) {
                self.0.stop();
            }
        }
        let _stop = StopMeter(&meter);
        options.check_cancelled()?;
        options.report("reading memory regions and residency");
        let regions = regions(source)?;
        let note = serde_json::to_vec(&CapturedResidency {
            version: 1,
            physical_footprint,
            mappings: regions.iter().map(|r| r.residency.clone()).collect(),
        })
        .map_err(|e| CaptureError::terminal(e.to_string()))?;
        // Reading I/O mappings can affect the device, as documented by Apple's gcore.
        let regions: Vec<_> = regions
            .into_iter()
            .filter(|r| r.protection & libc::VM_PROT_READ != 0 && r.tag != VM_MEMORY_IOKIT)
            .collect();
        let threads = threads(source)?;
        let header = header(&regions, &threads, &note);
        let total = regions.iter().map(|r| r.size).sum::<u64>() + note.len() as u64;
        meter.start("writing the compressed dump", total);
        let pool = rayon::ThreadPoolBuilder::new()
            .build()
            .map_err(|e| CaptureError::new(e.to_string()))?;
        let mut output = CoreFile::new(&path, header.len(), false, &pool, &meter)?;
        let ranges = regions.iter().flat_map(|region| {
            (0..region.size)
                .step_by(crate::compressed::BLOCK_SIZE)
                .map(|offset| {
                    (
                        region.address + offset,
                        (region.size - offset).min(crate::compressed::BLOCK_SIZE as u64) as usize,
                    )
                })
        });
        output.append_parallel(ranges, |address, bytes| {
            options.check_cancelled()?;
            let mut read = 0;
            checked(&format!("cannot read memory at {address:#x}"), unsafe {
                mach_vm_read_overwrite(
                    source,
                    address,
                    bytes.len() as u64,
                    bytes.as_mut_ptr() as u64,
                    &mut read,
                )
            })?;
            if read != bytes.len() as u64 {
                return Err(CaptureError::new(
                    "the kernel returned an incomplete memory read",
                ));
            }
            Ok(())
        })?;
        options.check_cancelled()?;
        output.append(&note)?;
        let bytes = output.finish(&header)?;
        Ok((bytes, threads.len()))
    });
    drop(suspension);
    drop(snapshot);
    let pause = if options.pause {
        pause.elapsed().as_secs_f64()
    } else {
        0.0
    };
    let (bytes, threads) = result?;
    options.report(format!(
        "capture completed in {:.1} s{}",
        started.elapsed().as_secs_f64(),
        if options.pause {
            format!("; process paused for {pause:.3} s")
        } else {
            String::new()
        }
    ));
    Ok(CaptureResult {
        path,
        bytes,
        copied_again: 0,
        pause,
        threads,
        soft_dirty: false,
    })
}
