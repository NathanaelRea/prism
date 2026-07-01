pub fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}
