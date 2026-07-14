fn main() {
    // Run on a thread we size ourselves. The command tree is ~120 subcommands,
    // and clap's derive builds it in one enormous stack frame — enough to blow
    // Windows' 1 MiB main-thread stack in a debug build. This is a real crash
    // (STATUS_STACK_OVERFLOW, before `main` gets to do anything), not a
    // theoretical one, and no amount of care inside the program can prevent it.
    let worker = std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(bd_cli::run)
        .expect("cannot spawn the main worker thread");

    // A panic in the worker has already printed itself; just carry out the code.
    let code = worker.join().unwrap_or(bd_cli::exit::FAILURE);
    std::process::exit(code);
}
