use heap_core::live_dump::{self, CaptureOptions};
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

fn capture() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let pid = args
        .next()
        .ok_or("missing PID")?
        .to_str()
        .ok_or("invalid PID")?
        .parse()?;
    let path = PathBuf::from(args.next().ok_or("missing output path")?);
    let cancel_while_stopped = args.any(|arg| arg == "--cancel-while-stopped");
    let cancellation_stage = AtomicUsize::new(0);
    let progress = |phase: &str| {
        if phase == "pausing the process for the final copy" {
            println!("before-stop");
            io::stdout().flush().unwrap();
            let mut line = String::new();
            io::stdin().read_line(&mut line).unwrap();
            if cancel_while_stopped {
                cancellation_stage.store(1, Ordering::SeqCst);
            }
        }
    };
    let cancelled = || {
        cancellation_stage.load(Ordering::SeqCst) != 0
            && cancellation_stage.fetch_add(1, Ordering::SeqCst) >= 2
    };
    let result = live_dump::write(
        pid,
        &path,
        &CaptureOptions {
            progress: Some(&progress),
            cancelled: Some(&cancelled),
            ..Default::default()
        },
    )?;
    println!("complete {}", result.threads);
    Ok(())
}

fn main() {
    if let Err(error) = capture() {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
