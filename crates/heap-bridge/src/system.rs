use crate::{
    models::*,
    snapshot::{Progress, Result},
};
use std::path::Path;

pub fn processes() -> Vec<Process> {
    heap_core::live_process::list()
}

pub fn dump_needs_conversion(path: &str) -> Result<bool> {
    Ok(heap_core::compressed::needs_conversion(Path::new(path))?)
}

pub fn convert_dump(
    source: &str,
    target: Option<String>,
    progress: Box<dyn Progress>,
) -> Result<Vec<String>> {
    Ok(heap_core::analysis::convert_dump(
        Path::new(source),
        target.as_deref().map(Path::new),
        |fraction| {
            progress.update("Converting and verifying the dump", fraction);
            !progress.cancelled()
        },
    )?)
}

pub fn smaps_next_to_dump(path: &str) -> Option<String> {
    heap_core::smaps::next_to_dump(Path::new(path)).map(|p| p.to_string_lossy().into_owned())
}

pub fn default_dump_path(process: &Process, runtime: bool) -> Result<String> {
    Ok(if runtime {
        heap_core::live_process::default_dump_path(process)?
    } else {
        heap_core::live_dump::default_path(process.pid)?
    }
    .to_string_lossy()
    .into_owned())
}

// Returns the snapshot error when the caller can offer an explicitly paused capture.
pub fn write_process_dump(
    process: &Process,
    path: &str,
    pause: bool,
    progress: Box<dyn Progress>,
) -> Result<Option<String>> {
    use heap_core::live_dump::{self, CaptureOptions};
    let phase = |phase: &str| progress.update(phase, 0.0);
    let status = |status: live_dump::Status| {
        progress.update(
            &status.phase,
            if status.total == 0 {
                0.0
            } else {
                status.done as f64 / status.total as f64
            },
        )
    };
    let cancelled = || progress.cancelled();
    match live_dump::write(
        process.pid,
        Path::new(path),
        &CaptureOptions {
            pause,
            progress: Some(&phase),
            status: Some(&status),
            cancelled: Some(&cancelled),
        },
    ) {
        Ok(_) => Ok(None),
        Err(error) if error.pause_can_capture => Ok(Some(error.description)),
        Err(error) => Err(heap_core::Error(error.description).into()),
    }
}

pub fn write_runtime_dump(
    process: &Process,
    path: &str,
    progress: Option<Box<dyn Progress>>,
) -> Result<()> {
    Ok(heap_core::live_process::write_dump(
        process,
        Path::new(path),
        |done, total| {
            if let Some(progress) = &progress {
                progress.update(
                    "Compressing the dump",
                    if total == 0 {
                        0.0
                    } else {
                        done as f64 / total as f64
                    },
                );
            }
        },
    )?)
}

pub fn cache_entries(measure: bool) -> Vec<CacheEntry> {
    heap_core::cache::entries(measure)
}

pub fn remove_cache(entry: &CacheEntry) -> Result<()> {
    Ok(heap_core::cache::remove(entry)?)
}

pub fn cache_root() -> String {
    heap_core::cache::root().to_string_lossy().into_owned()
}

pub fn disk_bytes(path: &str) -> u64 {
    heap_core::cache::disk_bytes(Path::new(path))
}

pub fn dump_next_to_smaps(path: &str) -> Option<String> {
    heap_core::smaps::dump_next_to(Path::new(path)).map(|p| p.to_string_lossy().into_owned())
}

pub fn short_type_name(name: &str) -> String {
    heap_core::type_names::short(name)
}

pub fn without_namespaces(name: &str) -> String {
    heap_core::type_names::without_namespaces(name)
}

pub fn property_name(name: &str) -> String {
    heap_core::type_names::property_name(name)
}

pub fn residency_warning(residency: &Residency) -> Option<String> {
    residency.warning()
}

pub fn residency_agrees(residency: &Residency) -> bool {
    residency.agrees()
}

pub fn handle_type_name(kind: u8) -> String {
    heap_core::handles::type_name(kind)
}

pub fn state_machine_field_name(name: &str) -> Option<String> {
    heap_core::tasks::field_name(name)
}

pub fn timer_duration(milliseconds: i64) -> String {
    heap_core::timers::duration(milliseconds)
}

pub fn timer_key(timer: &TimerInfo) -> String {
    heap_core::timers::timer_key(timer)
}

pub fn socket_group(socket: &SocketInfo) -> String {
    heap_core::connections::socket_group(socket)
}

pub fn field_type_name(field: &FieldInfo) -> String {
    field.type_name()
}

pub fn attached_threads(arena: u64, snapshot: &crate::snapshot::Snapshot) -> Result<u64> {
    let result = snapshot
        .core
        .dump
        .u64(arena.saturating_add(heap_core::native_memory::STATE_ATTACHED_THREADS))
        .unwrap_or(0);
    snapshot.checked(Ok(result))
}

pub fn verify_heap(path: &str) -> Result<HeapVerification> {
    Ok(heap_core::verification::verify_file(
        Path::new(path),
        |_, _| {},
    )?)
}
