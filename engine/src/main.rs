//! shopping-engine: the Shopping Sessions pipeline. See docs/CONTRACT.md and rules.md.

mod fast;
mod hop;
mod links;
mod parquet;
mod person;
mod scan;
mod slow;
mod ts;
mod util;

use std::path::PathBuf;

/// glibc malloc settings for this workload (the Vec-backed tables and output buffers of the
/// dedup and people phases): heaps madvised for transparent huge pages and grown 32 MiB at a
/// time instead of 128 KiB. Measured on the benchmark VM: 4K page faults 31K -> 11K at 20M
/// events, wall -3%. glibc only takes these from the environment at start-up, so the image
/// sets the variable (Dockerfile); started without it, the engine starts itself again with it.
#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn malloc_tunables() {
    use std::os::unix::process::CommandExt;
    if std::env::var_os("GLIBC_TUNABLES").is_some() || std::env::var_os("SE_REEXEC").is_some() {
        return;
    }
    let mut args = std::env::args_os();
    let Some(arg0) = args.next() else { return };
    // Returns only if the exec failed: then carry on with the defaults.
    let _ = std::process::Command::new("/proc/self/exe")
        .arg0(arg0)
        .args(args)
        .env("GLIBC_TUNABLES", "glibc.malloc.hugetlb=1:glibc.malloc.top_pad=33554432")
        .env("SE_REEXEC", "1")
        .exec();
}

fn main() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    malloc_tunables();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let timing = args.iter().any(|a| a == "--timing") || util::env_flag("SE_TIMING");
    let pos: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let (data, out) = if pos.len() >= 2 {
        (PathBuf::from(pos[pos.len() - 2]), PathBuf::from(pos[pos.len() - 1]))
    } else {
        (
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into())),
            PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into())),
        )
    };
    let env_n = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<usize>().ok()).filter(|&n| n > 0);
    let nthreads = env_n("SE_THREADS")
        .or(env_n("NCPU"))
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, usize::from))
        .min(256);
    util::thp_knob();
    // Diagnostic knob SE_REPEAT=N: first run the whole pipeline N-1 times in child processes
    // that write to $SCRATCH_DIR, then once for real. The wall time grows by N-1 warm runs,
    // which separates the pipeline's own time from the fixed cost of starting the container.
    if let Some(n) = env_n("SE_REPEAT") {
        let scratch = PathBuf::from(std::env::var("SCRATCH_DIR").unwrap_or_else(|_| "/scratch".into())).join("se-repeat");
        if let Ok(exe) = std::env::current_exe() {
            for _ in 1..n {
                let _ = std::process::Command::new(&exe).arg(&data).arg(&scratch).env_remove("SE_REPEAT").status();
                let _ = std::fs::remove_dir_all(&scratch);
            }
        }
    }
    let mut tm = util::Timer::new(timing);
    for t in ["user_daily", "campaign_daily"] {
        if let Err(e) = std::fs::create_dir_all(out.join(t)) {
            fast::die(&format!("cannot create {}: {e}", out.join(t).display()));
        }
        // Leftovers of an earlier run with more threads would be read as part of the table.
        if let Ok(rd) = std::fs::read_dir(out.join(t)) {
            for e in rd.flatten() {
                let name = e.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("part-") && name.ends_with(".parquet") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    let inp = fast::load_inputs(&data);
    tm.lap("list+mmap");
    if args.iter().any(|a| a == "--kernel-bench") {
        fast::kernel_bench(&inp, nthreads);
        unsafe { util::_exit(0) }
    }
    let done = !util::env_flag("SE_SLOW") && fast::run(&inp, &out, nthreads, &mut tm);
    if !done {
        slow::run(&inp, &out);
        tm.lap("slow path");
    }
    // Give the arenas back from all threads at once; exit would do it on one (SE_RELEASE=0: off).
    if util::env_on("SE_RELEASE") {
        util::release_all(nthreads);
        tm.lap("release");
    }
    unsafe { util::_exit(0) }
}
