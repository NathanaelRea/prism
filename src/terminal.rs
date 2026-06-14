use std::ffi::c_ulong;
use std::io::{self, ErrorKind, Write};
use std::os::fd::RawFd;
use std::process::Command;

use crate::process::run_status;

pub const WNOHANG: i32 = 1;

pub struct RawTerminal {
    stdin_flags: i32,
}

impl RawTerminal {
    pub fn enter() -> Result<Self, String> {
        let stdin_flags = get_fd_flags(0)?;
        run_status(Command::new("stty").args(raw_stty_args()))?;
        set_fd_flags(0, stdin_flags | O_NONBLOCK)?;
        write_stdout("\x1b[?1049h")?;
        Ok(Self { stdin_flags })
    }

    pub fn suspend(&mut self) -> Result<(), String> {
        set_fd_flags(0, self.stdin_flags)?;
        run_status(Command::new("stty").arg("sane"))?;
        write_stdout("\x1b[?1049l\x1b[?25h")
    }

    pub fn resume(&mut self) -> Result<(), String> {
        run_status(Command::new("stty").args(raw_stty_args()))?;
        set_fd_flags(0, self.stdin_flags | O_NONBLOCK)?;
        write_stdout("\x1b[?1049h\x1b[?25l")
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = set_fd_flags(0, self.stdin_flags);
        let _ = Command::new("stty").arg("sane").status();
        let _ = write_stdout("\x1b[?1049l\x1b[?25h");
    }
}

pub fn write_stdout(text: &str) -> Result<(), String> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    write_all_retrying_would_block(&mut stdout, text.as_bytes())
}

fn write_all_retrying_would_block(writer: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    let mut written = 0;
    while written < bytes.len() {
        match writer.write(&bytes[written..]) {
            Ok(0) => return Err("write stdout: wrote zero bytes".to_string()),
            Ok(count) => written += count,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    loop {
        match writer.flush() {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

pub fn terminal_size() -> (u16, u16) {
    terminal_size_from_ioctl()
        .or_else(terminal_size_from_env)
        .unwrap_or((100, 30))
}

pub fn stdin_is_tty() -> bool {
    Command::new("test")
        .args(["-t", "0"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

#[allow(dead_code)]
pub fn set_nonblocking(fd: RawFd) -> Result<(), String> {
    let flags = get_fd_flags(fd)?;
    set_fd_flags(fd, flags | O_NONBLOCK)
}

fn get_fd_flags(fd: RawFd) -> Result<i32, String> {
    let flags = unsafe { fcntl(fd, F_GETFL, 0) };
    if flags < 0 {
        Err("fcntl(F_GETFL) failed".to_string())
    } else {
        Ok(flags)
    }
}

fn set_fd_flags(fd: RawFd, flags: i32) -> Result<(), String> {
    let result = unsafe { fcntl(fd, F_SETFL, flags) };
    if result < 0 {
        Err("fcntl(F_SETFL) failed".to_string())
    } else {
        Ok(())
    }
}

const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const O_NONBLOCK: i32 = 0o4000;
const TIOCGWINSZ: c_ulong = 0x5413;

#[repr(C)]
#[derive(Default)]
struct WinSize {
    rows: u16,
    cols: u16,
    _x_pixels: u16,
    _y_pixels: u16,
}

fn terminal_size_from_ioctl() -> Option<(u16, u16)> {
    [1, 0, 2].into_iter().find_map(terminal_size_from_fd)
}

fn terminal_size_from_fd(fd: RawFd) -> Option<(u16, u16)> {
    let mut size = WinSize::default();
    let result = unsafe { ioctl(fd, TIOCGWINSZ, &mut size) };
    valid_terminal_size(size.cols, size.rows).filter(|_| result >= 0)
}

fn terminal_size_from_env() -> Option<(u16, u16)> {
    let cols = std::env::var("COLUMNS").ok()?.parse().ok()?;
    let rows = std::env::var("LINES").ok()?.parse().ok()?;
    valid_terminal_size(cols, rows)
}

fn valid_terminal_size(cols: u16, rows: u16) -> Option<(u16, u16)> {
    if cols > 0 && rows > 0 {
        Some((cols, rows))
    } else {
        None
    }
}

fn raw_stty_args() -> [&'static str; 4] {
    ["raw", "-echo", "opost", "onlcr"]
}

unsafe extern "C" {
    fn fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
    fn ioctl(fd: i32, request: c_ulong, ...) -> i32;
}

#[cfg(test)]
mod tests {
    use std::io::{self, ErrorKind, Write};

    use super::{raw_stty_args, terminal_size_from_fd, valid_terminal_size};

    #[test]
    fn valid_terminal_size_rejects_zero_dimensions() {
        assert_eq!(valid_terminal_size(0, 24), None);
        assert_eq!(valid_terminal_size(80, 0), None);
    }

    #[test]
    fn valid_terminal_size_preserves_column_row_order() {
        assert_eq!(valid_terminal_size(132, 43), Some((132, 43)));
    }

    #[test]
    fn terminal_size_from_fd_ignores_invalid_fds() {
        assert_eq!(terminal_size_from_fd(-1), None);
    }

    #[test]
    fn raw_mode_keeps_newline_translation_enabled() {
        let args = raw_stty_args();
        assert!(args.contains(&"opost"));
        assert!(args.contains(&"onlcr"));
    }

    #[test]
    fn stdout_writer_retries_would_block() {
        let mut writer = WouldBlockOnce::default();

        super::write_all_retrying_would_block(&mut writer, b"hello").unwrap();

        assert_eq!(writer.output, b"hello");
    }

    #[derive(Default)]
    struct WouldBlockOnce {
        blocked: bool,
        output: Vec<u8>,
    }

    impl Write for WouldBlockOnce {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if !self.blocked {
                self.blocked = true;
                return Err(io::Error::new(ErrorKind::WouldBlock, "would block"));
            }
            self.output.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
}
