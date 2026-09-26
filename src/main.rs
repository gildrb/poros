fn main() {
    // glibc gives every allocating thread its own arena (64 MiB reserved
    // each, kept after the thread exits). Poros allocates little and rarely;
    // one shared arena keeps its footprint flat as connections come and go.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
    }
    let exit_code = poros::runner::run(std::env::args().skip(1).collect());
    std::process::exit(exit_code);
}
