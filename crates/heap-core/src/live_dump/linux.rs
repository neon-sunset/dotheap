use super::{
    CaptureError, CaptureOptions, CaptureResult, Result,
    core_file::{CoreFile, check_free_space},
    mappings::Mapping,
    memory::{MemoryReader, OomPreference, hash_page, in_memory, soft_dirty, soft_dirty_works},
    meter::Meter,
    notes::{self, ProcessNotes},
    page_store::{PageStore, StopMemory},
    thread_stop::ThreadStop,
};
use rayon::prelude::*;
use std::{path::Path, sync::atomic::Ordering, time::Instant};

struct Copy {
    mapping: Mapping,
    offset: u64,
    absent: Vec<u64>,
    hashes: Vec<u64>,
    latest: Vec<u64>,
}

impl Copy {
    fn is_absent(&self, page: usize) -> bool {
        self.absent[page / 64] & (1 << (page % 64)) != 0
    }
    fn set_absent(&mut self, page: usize, absent: bool) {
        let bit = 1 << (page % 64);
        if absent {
            self.absent[page / 64] |= bit;
        } else {
            self.absent[page / 64] &= !bit;
        }
    }
    fn location(&self, page: usize, page_size: usize) -> u64 {
        self.latest
            .get(page)
            .copied()
            .filter(|&at| at != 0)
            .unwrap_or(self.offset + (page * page_size) as u64)
    }
    fn same_memory(&self, mapping: &Mapping, address: u64) -> bool {
        self.mapping.path == mapping.path
            && (!mapping.file_backed()
                || self
                    .mapping
                    .offset
                    .checked_add(address - self.mapping.start)
                    == mapping.offset.checked_add(address - mapping.start))
    }
}

#[derive(Clone, Copy)]
enum Source {
    Copy(u64),
    Again(usize, usize),
    Zero,
}

struct Run {
    pages: usize,
    source: Source,
}

fn add_run(runs: &mut Vec<Run>, pages: usize, source: Source, page_size: usize) {
    if let Some(last) = runs.last_mut() {
        let bytes = last.pages * page_size;
        let follows = match (last.source, source) {
            (Source::Copy(a), Source::Copy(b)) => a + bytes as u64 == b,
            (Source::Again(a, offset), Source::Again(b, next)) => a == b && offset + bytes == next,
            (Source::Zero, Source::Zero) => true,
            _ => false,
        };
        if follows {
            last.pages += pages;
            return;
        }
    }
    runs.push(Run { pages, source });
}

#[derive(Clone, Copy)]
struct Piece {
    mapping: usize,
    first: usize,
    pages: usize,
}

fn pieces(mappings: &[Mapping], page_size: usize) -> Vec<Piece> {
    let piece_pages = (16 << 20) / page_size;
    mappings
        .iter()
        .enumerate()
        .flat_map(|(mapping, range)| {
            let pages = range.pages(page_size);
            (0..pages).step_by(piece_pages).map(move |first| Piece {
                mapping,
                first,
                pages: piece_pages.min(pages - first),
            })
        })
        .collect()
}

pub(super) fn write(pid: i32, path: &Path, options: &CaptureOptions<'_>) -> Result<CaptureResult> {
    if pid <= 0 || pid == unsafe { libc::getpid() } {
        return Err(CaptureError::terminal(
            "choose another process with a positive PID",
        ));
    }
    let path = std::path::absolute(path)?;
    crate::live_process::ensure_absent(&path).map_err(|e| CaptureError::terminal(e.0))?;
    check_free_space(&path)?;
    let _oom = OomPreference::enter();
    let reader = MemoryReader::open(pid)?;
    let meter = Meter::new();
    std::thread::scope(|scope| {
        if let Some(status) = options.status {
            scope.spawn(|| meter.report(status));
        }
        struct StopMeter<'a>(&'a Meter);
        impl Drop for StopMeter<'_> {
            fn drop(&mut self) {
                self.0.stop();
            }
        }
        let _stop_meter = StopMeter(&meter);
        capture(&reader, &path, options, &meter)
    })
}

fn capture(
    reader: &MemoryReader,
    path: &Path,
    options: &CaptureOptions<'_>,
    meter: &Meter,
) -> Result<CaptureResult> {
    let begun = Instant::now();
    let workers = (std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        / 2)
    .max(1);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers.min(8))
        .build()
        .map_err(|e| CaptureError::new(e.to_string()))?;
    let stopped_pool = rayon::ThreadPoolBuilder::new()
        .num_threads(workers)
        .build()
        .map_err(|e| CaptureError::new(e.to_string()))?;
    let page_size = reader.page_size;
    let chunk_pages = (1 << 20) / page_size;
    let first = Mapping::parse(&reader.proc_file("maps")?, page_size)?;
    let use_dirty = soft_dirty_works(page_size);
    if use_dirty {
        reader.clear_soft_dirty()?;
    }
    let total: u64 = first.iter().map(|m| m.end - m.start).sum();
    options.report(format!(
        "copying {} from process {}{}",
        size(total),
        reader.pid,
        if use_dirty {
            ""
        } else {
            " (no soft-dirty tracking; comparing resident pages)"
        }
    ));
    let mut file = CoreFile::new(path, 0, true, &pool, meter)?;
    let mut copies = Vec::new();
    let mut chunk = vec![0; chunk_pages * page_size];
    let mut entries = vec![0; chunk_pages];
    meter.start("copying memory", total);
    for mapping in first {
        let pages = mapping.pages(page_size);
        let mut copy = Copy {
            offset: file.position,
            absent: vec![0; pages.div_ceil(64)],
            hashes: if mapping.file_backed() {
                Vec::new()
            } else {
                vec![0; pages]
            },
            latest: Vec::new(),
            mapping,
        };
        for page in (0..pages).step_by(chunk_pages) {
            options.check_cancelled()?;
            let count = chunk_pages.min(pages - page);
            let address = copy.mapping.start + (page * page_size) as u64;
            if copy.mapping.anonymous() {
                reader.pagemap(address, &mut entries[..count])?;
                let mut index = 0;
                while index < count {
                    if in_memory(entries[index]) {
                        let end = (index + 1..count)
                            .find(|&i| !in_memory(entries[i]))
                            .unwrap_or(count);
                        reader.read(
                            address + (index * page_size) as u64,
                            &mut chunk[index * page_size..end * page_size],
                            false,
                        )?;
                        index = end;
                    } else {
                        chunk[index * page_size..(index + 1) * page_size].fill(0);
                        copy.set_absent(page + index, true);
                        index += 1;
                    }
                }
            } else {
                reader.read(address, &mut chunk[..count * page_size], false)?;
            }
            if !copy.hashes.is_empty() {
                for index in 0..count {
                    copy.hashes[page + index] =
                        hash_page(&chunk[index * page_size..(index + 1) * page_size]);
                }
            }
            file.append(&chunk[..count * page_size])?;
            meter.done((count * page_size) as u64);
        }
        file.end_frame();
        copies.push(copy);
    }
    file.flush()?;
    let copy_mappings: Vec<_> = copies.iter().map(|c| c.mapping.clone()).collect();
    let copy_pieces = pieces(&copy_mappings, page_size);
    let mut brief_stops = Vec::new();
    let mut previous = usize::MAX;
    for _ in 0..3 {
        options.check_cancelled()?;
        meter.start("finding pages to compare", 0);
        let candidates = if use_dirty {
            if candidates(reader, &copies, &copy_pieces, use_dirty, &pool)?.len() * page_size
                < 64 << 20
            {
                break;
            }
            let started = Instant::now();
            let stop = ThreadStop::stop(reader, options)?;
            let list = candidates(reader, &copies, &copy_pieces, use_dirty, &stopped_pool)?;
            reader.clear_soft_dirty()?;
            drop(stop);
            let pause = started.elapsed().as_secs_f64();
            brief_stops.push(pause);
            options.report(format!(
                "paused for {pause:.3} s to read and reset soft-dirty bits"
            ));
            list
        } else {
            candidates(reader, &copies, &copy_pieces, use_dirty, &pool)?
        };
        meter.start("comparing pages", (candidates.len() * page_size) as u64);
        let dirty = changed(reader, &copies, &candidates, &pool, meter)?;
        options.report(format!(
            "{} changed out of {} compared",
            size((dirty.len() * page_size) as u64),
            size((candidates.len() * page_size) as u64)
        ));
        meter.start("copying changed pages", (dirty.len() * page_size) as u64);
        let mut index = 0;
        while index < dirty.len() {
            options.check_cancelled()?;
            let (copy_index, first_page) = dirty[index];
            let copy = &mut copies[copy_index];
            let mut end = index + 1;
            while end < dirty.len()
                && end - index < chunk_pages
                && dirty[end] == (copy_index, first_page + end - index)
            {
                end += 1;
            }
            let count = end - index;
            reader.read(
                copy.mapping.start + (first_page * page_size) as u64,
                &mut chunk[..count * page_size],
                false,
            )?;
            if copy.latest.is_empty() {
                copy.latest.resize(copy.mapping.pages(page_size), 0);
            }
            for index in 0..count {
                let page = first_page + index;
                let bytes = &chunk[index * page_size..(index + 1) * page_size];
                copy.latest[page] = file.position + (index * page_size) as u64;
                copy.hashes[page] = hash_page(bytes);
                copy.set_absent(page, bytes.iter().all(|&b| b == 0));
            }
            file.append(&chunk[..count * page_size])?;
            meter.done((count * page_size) as u64);
            index = end;
        }
        file.flush()?;
        if dirty.len() * page_size < 64 << 20 || dirty.len() >= previous / 2 {
            break;
        }
        previous = dirty.len();
    }

    options.report("pausing the process for the final copy");
    options.check_cancelled()?;
    meter.start("final copy (process paused)", 0);
    let memory = StopMemory::new(path);
    let started = Instant::now();
    let stop = ThreadStop::stop(reader, options)?;
    let registers = stop.registers()?;
    let mappings = Mapping::parse(&reader.proc_file("maps")?, page_size)?;
    let process = ProcessNotes {
        auxv: reader.proc_file("auxv")?,
        stat: String::from_utf8_lossy(&reader.proc_file("stat")?).into_owned(),
        status: String::from_utf8_lossy(&reader.proc_file("status")?).into_owned(),
        command_line: reader.proc_file("cmdline")?,
    };
    let list = pieces(&mappings, page_size);
    let stores: Result<Vec<_>> = stopped_pool.install(|| {
        (0..workers)
            .into_par_iter()
            .map(|worker| {
                final_pages(
                    reader, &copies, &mappings, &list, worker, workers, use_dirty, &memory, options,
                )
            })
            .collect()
    });
    let stores = stores?;
    drop(stop);
    let pause = started.elapsed().as_secs_f64();
    let mut copied_by = [0u64; 3];
    let mut plans: Vec<_> = stores.iter().flat_map(|s| &s.pieces).collect();
    plans.sort_by_key(|p| p.index);
    for plan in &plans {
        for (total, bytes) in copied_by.iter_mut().zip(plan.by) {
            *total += bytes;
        }
    }
    options.report(format!(
        "process resumed after {pause:.3} s; copied {} of changed pages, {} of file and shared \
        mappings, and {} of new mappings",
        size(copied_by[0]),
        size(copied_by[1]),
        size(copied_by[2])
    ));
    if memory.file_bytes() > 0 {
        options.report(format!(
            "allocated {} in a temporary file beside the dump to limit memory use",
            size(memory.file_bytes())
        ));
    }
    let smaps = reader.proc_file("smaps").ok();
    let notes = notes::notes(reader.pid, &registers, &mappings, page_size, &process)?;
    let header = notes::header(&mappings, &notes, page_size)?;
    let mut core = CoreFile::new(path, header.len(), false, &pool, meter)?;
    let total: u64 = mappings.iter().map(|m| m.end - m.start).sum();
    meter.start("writing dump", total);
    let mut first_read = vec![u64::MAX; plans.len() + 1];
    for index in (0..plans.len()).rev() {
        let mut lowest = first_read[index + 1];
        for run in &plans[index].runs {
            if let Source::Copy(offset) = run.source {
                lowest = lowest.min(file.frames[file.frame(offset)?].offset);
            }
        }
        first_read[index] = lowest;
    }
    let mut peak_disk = file.disk_bytes();
    for (index, plan) in plans.iter().enumerate() {
        options.check_cancelled()?;
        for run in &plan.runs {
            let bytes = run.pages * page_size;
            match run.source {
                Source::Copy(offset) => core.append_from(&mut file, offset, bytes)?,
                Source::Again(store, offset) => {
                    stores[store].pages.write(offset, bytes, &mut core)?
                }
                Source::Zero => core.append_zeros(bytes as u64)?,
            }
        }
        if list[plan.index].first + list[plan.index].pages
            == mappings[list[plan.index].mapping].pages(page_size)
        {
            core.end_frame();
        }
        peak_disk = peak_disk.max(file.disk_bytes() + core.disk_bytes());
        if first_read[index + 1] != u64::MAX {
            file.punch(first_read[index + 1]);
        }
    }
    let written = core.finish(&header)?;
    if let Some(smaps) = smaps {
        if let Err(error) = super::save_smaps_bytes(&smaps, path) {
            options.report(format!("dump saved; could not save smaps: {error}"));
        }
    }
    brief_stops.push(pause);
    options.report(format!(
        "capture completed in {:.1} s\n  captured memory   {}\n  dump size         {}\n  \
        peak disk use     {}\n  pauses            {}\n  total pause time  {:.3} s\n  \
        longest pause     {:.3} s",
        begun.elapsed().as_secs_f64(),
        size(total),
        size(written),
        size(peak_disk),
        brief_stops.len(),
        brief_stops.iter().sum::<f64>(),
        brief_stops.iter().copied().fold(0.0, f64::max)
    ));
    Ok(CaptureResult {
        path: path.into(),
        bytes: total,
        copied_again: stores.iter().map(|s| s.pages.bytes).sum(),
        pause,
        threads: registers.len(),
        soft_dirty: use_dirty,
    })
}

fn candidates(
    reader: &MemoryReader,
    copies: &[Copy],
    pieces: &[Piece],
    use_dirty: bool,
    pool: &rayon::ThreadPool,
) -> Result<Vec<(usize, usize)>> {
    let chunk_pages = (1 << 20) / reader.page_size;
    let found: Result<Vec<_>> = pool.install(|| {
        pieces
            .par_iter()
            .map_init(
                || vec![0; chunk_pages],
                |entries, piece| {
                    let copy = &copies[piece.mapping];
                    let mut found = Vec::new();
                    if copy.hashes.is_empty() {
                        return Ok(found);
                    }
                    for page in (piece.first..piece.first + piece.pages).step_by(chunk_pages) {
                        let count = chunk_pages.min(piece.first + piece.pages - page);
                        reader.pagemap(
                            copy.mapping.start + (page * reader.page_size) as u64,
                            &mut entries[..count],
                        )?;
                        for (index, &entry) in entries[..count].iter().enumerate() {
                            if in_memory(entry) && (!use_dirty || soft_dirty(entry)) {
                                found.push((piece.mapping, page + index));
                            }
                        }
                    }
                    Ok(found)
                },
            )
            .collect()
    });
    Ok(found?.into_iter().flatten().collect())
}

fn changed(
    reader: &MemoryReader,
    copies: &[Copy],
    list: &[(usize, usize)],
    pool: &rayon::ThreadPool,
    meter: &Meter,
) -> Result<Vec<(usize, usize)>> {
    let page_size = reader.page_size;
    let chunk_pages = (1 << 20) / page_size;
    let found: Result<Vec<_>> = pool.install(|| {
        list.par_chunks(chunk_pages)
            .map_init(
                || vec![0; chunk_pages * page_size],
                |buffer, list| {
                    let mut found = Vec::new();
                    let mut index = 0;
                    while index < list.len() {
                        let (copy, first) = list[index];
                        let mut end = index + 1;
                        while end < list.len() && list[end] == (copy, first + end - index) {
                            end += 1;
                        }
                        reader.read(
                            copies[copy].mapping.start + (first * page_size) as u64,
                            &mut buffer[..(end - index) * page_size],
                            false,
                        )?;
                        for offset in 0..end - index {
                            if hash_page(&buffer[offset * page_size..(offset + 1) * page_size])
                                != copies[copy].hashes[first + offset]
                            {
                                found.push((copy, first + offset));
                            }
                        }
                        meter.done(((end - index) * page_size) as u64);
                        index = end;
                    }
                    Ok(found)
                },
            )
            .collect()
    });
    Ok(found?.into_iter().flatten().collect())
}

#[derive(Clone, Copy)]
enum Plan {
    Keep(u64),
    Zero,
    Read,
    Compare(usize, usize),
}

impl Plan {
    fn reads(self) -> bool {
        matches!(self, Self::Read | Self::Compare(..))
    }
}

struct PlannedPiece {
    index: usize,
    runs: Vec<Run>,
    by: [u64; 3],
}

struct CopiedPages<'a> {
    pages: PageStore<'a>,
    pieces: Vec<PlannedPiece>,
}

fn final_pages<'a>(
    reader: &MemoryReader,
    copies: &[Copy],
    mappings: &[Mapping],
    pieces: &[Piece],
    worker: usize,
    workers: usize,
    use_dirty: bool,
    memory: &'a StopMemory,
    options: &CaptureOptions<'_>,
) -> Result<CopiedPages<'a>> {
    let page_size = reader.page_size;
    let chunk_pages = (1 << 20) / page_size;
    let mut entries = vec![0; chunk_pages];
    let mut chunk = vec![0; chunk_pages * page_size];
    let mut plan = vec![Plan::Read; chunk_pages];
    let mut why = vec![0; chunk_pages];
    let mut output = CopiedPages {
        pages: PageStore::new(memory),
        pieces: Vec::new(),
    };
    let initial_read_failed = reader.unreadable.load(Ordering::Relaxed) != 0;
    for index in (worker..pieces.len()).step_by(workers) {
        let piece = pieces[index];
        let mapping = &mappings[piece.mapping];
        let mut runs = Vec::new();
        let mut by = [0; 3];
        let start = mapping.start + (piece.first * page_size) as u64;
        let mut copy_index = copies.partition_point(|c| c.mapping.end <= start);
        for first in (piece.first..piece.first + piece.pages).step_by(chunk_pages) {
            options.check_cancelled()?;
            let count = chunk_pages.min(piece.first + piece.pages - first);
            let address = mapping.start + (first * page_size) as u64;
            reader.pagemap(address, &mut entries[..count])?;
            for index in 0..count {
                let at = address + (index * page_size) as u64;
                while copy_index < copies.len() && copies[copy_index].mapping.end <= at {
                    copy_index += 1;
                }
                let copied = copies
                    .get(copy_index)
                    .filter(|c| c.mapping.start <= at && c.same_memory(mapping, at))
                    .map(|c| {
                        (
                            copy_index,
                            ((at - c.mapping.start) / page_size as u64) as usize,
                        )
                    });
                if mapping.anonymous() && !in_memory(entries[index]) {
                    plan[index] = if let Some((copy, page)) =
                        copied.filter(|&(c, p)| copies[c].is_absent(p))
                    {
                        Plan::Keep(copies[copy].location(page, page_size))
                    } else {
                        Plan::Zero
                    };
                } else if let Some((copy, page)) =
                    copied.filter(|_| !mapping.file_backed() && !mapping.shared)
                {
                    plan[index] =
                        if use_dirty && !soft_dirty(entries[index]) && !initial_read_failed {
                            Plan::Keep(copies[copy].location(page, page_size))
                        } else {
                            Plan::Compare(copy, page)
                        };
                    why[index] = 0;
                } else {
                    plan[index] = Plan::Read;
                    why[index] = if copied.is_none() { 2 } else { 1 };
                }
            }
            let mut index = 0;
            while index < count {
                match plan[index] {
                    Plan::Keep(offset) => {
                        add_run(&mut runs, 1, Source::Copy(offset), page_size);
                        index += 1;
                    }
                    Plan::Zero => {
                        add_run(&mut runs, 1, Source::Zero, page_size);
                        index += 1;
                    }
                    Plan::Read | Plan::Compare(..) => {
                        let end = (index + 1..count)
                            .find(|&i| !plan[i].reads())
                            .unwrap_or(count);
                        reader.read(
                            address + (index * page_size) as u64,
                            &mut chunk[..(end - index) * page_size],
                            true,
                        )?;
                        for page in index..end {
                            let start = (page - index) * page_size;
                            let bytes = &chunk[start..start + page_size];
                            let unchanged = match plan[page] {
                                Plan::Compare(copy, original)
                                    if hash_page(bytes) == copies[copy].hashes[original] =>
                                {
                                    Some(copies[copy].location(original, page_size))
                                }
                                _ => None,
                            };
                            if let Some(offset) = unchanged {
                                add_run(&mut runs, 1, Source::Copy(offset), page_size);
                            } else {
                                let offset = output.pages.add(bytes)?;
                                add_run(&mut runs, 1, Source::Again(worker, offset), page_size);
                                by[why[page]] += page_size as u64;
                            }
                        }
                        index = end;
                    }
                }
            }
        }
        output.pieces.push(PlannedPiece { index, runs, by });
    }
    Ok(output)
}

fn size(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{:.1} GB", bytes as f64 / (1 << 30) as f64)
    } else {
        format!("{:.1} MB", bytes as f64 / (1 << 20) as f64)
    }
}
