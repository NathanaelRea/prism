use std::io::{self, ErrorKind, Write};
use std::os::fd::RawFd;
use std::process::{Command, Stdio};

use crate::process::{run_status, run_status_inherited};

pub const WNOHANG: i32 = 1;

pub struct RawTerminal {
    stdin_flags: i32,
}

impl RawTerminal {
    pub fn enter() -> Result<Self, String> {
        let stdin_flags = get_fd_flags(0)?;
        run_stty(raw_stty_args())?;
        set_fd_flags(0, stdin_flags | O_NONBLOCK)?;
        write_stdout("\x1b[?1049h")?;
        Ok(Self { stdin_flags })
    }

    pub fn suspend(&mut self) -> Result<(), String> {
        set_fd_flags(0, self.stdin_flags)?;
        run_stty(["sane"])?;
        write_stdout("\x1b[?1049l\x1b[?25h")
    }

    pub fn resume(&mut self) -> Result<(), String> {
        run_stty(raw_stty_args())?;
        set_fd_flags(0, self.stdin_flags | O_NONBLOCK)?;
        write_stdout("\x1b[?1049h\x1b[?25l")
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = set_fd_flags(0, self.stdin_flags);
        let _ = run_status_inherited(Command::new("stty").arg("sane"));
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
    terminal_size_from_sources(terminal_size_from_ioctl(), terminal_size_from_env())
}

pub fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

#[allow(dead_code)]
pub fn set_nonblocking(fd: RawFd) -> Result<(), String> {
    let flags = get_fd_flags(fd)?;
    set_fd_flags(fd, flags | O_NONBLOCK)
}

fn get_fd_flags(fd: RawFd) -> Result<i32, String> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL, 0) };
    if flags < 0 {
        Err("fcntl(F_GETFL) failed".to_string())
    } else {
        Ok(flags)
    }
}

fn set_fd_flags(fd: RawFd, flags: i32) -> Result<(), String> {
    let result = unsafe { libc::fcntl(fd, libc::F_SETFL, flags) };
    if result < 0 {
        Err("fcntl(F_SETFL) failed".to_string())
    } else {
        Ok(())
    }
}

const O_NONBLOCK: i32 = libc::O_NONBLOCK;

fn terminal_size_from_ioctl() -> Option<(u16, u16)> {
    [1, 0, 2].into_iter().find_map(terminal_size_from_fd)
}

fn terminal_size_from_fd(fd: RawFd) -> Option<(u16, u16)> {
    let mut size = libc::winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) };
    valid_terminal_size(size.ws_col, size.ws_row).filter(|_| result >= 0)
}

fn terminal_size_from_env() -> Option<(u16, u16)> {
    terminal_size_from_env_values(std::env::var("COLUMNS").ok(), std::env::var("LINES").ok())
}

fn terminal_size_from_env_values(cols: Option<String>, rows: Option<String>) -> Option<(u16, u16)> {
    let cols = cols?.parse().ok()?;
    let rows = rows?.parse().ok()?;
    valid_terminal_size(cols, rows)
}

fn terminal_size_from_sources(ioctl: Option<(u16, u16)>, env: Option<(u16, u16)>) -> (u16, u16) {
    ioctl.or(env).unwrap_or((100, 30))
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

fn run_stty<const N: usize>(args: [&str; N]) -> Result<(), String> {
    run_status(Command::new("stty").args(args).stdin(Stdio::inherit()))
}

#[cfg(test)]
mod tests {
    use std::io::{self, ErrorKind, Write};

    use super::{
        raw_stty_args, terminal_size_from_env_values, terminal_size_from_fd,
        terminal_size_from_sources, valid_terminal_size,
    };

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
    fn terminal_size_uses_ioctl_before_env_and_default() {
        assert_eq!(
            terminal_size_from_sources(Some((180, 50)), Some((80, 24))),
            (180, 50)
        );
        assert_eq!(terminal_size_from_sources(None, Some((80, 24))), (80, 24));
        assert_eq!(terminal_size_from_sources(None, None), (100, 30));
    }

    #[test]
    fn terminal_size_from_env_values_rejects_invalid_dimensions() {
        assert_eq!(
            terminal_size_from_env_values(Some("132".to_string()), Some("43".to_string())),
            Some((132, 43))
        );
        assert_eq!(
            terminal_size_from_env_values(Some("0".to_string()), Some("43".to_string())),
            None
        );
        assert_eq!(
            terminal_size_from_env_values(Some("wide".to_string()), Some("43".to_string())),
            None
        );
    }

    #[test]
    fn terminal_size_from_pty_tracks_configured_window_size() {
        let pty = TestPty::open();
        pty.set_size(173, 41);

        assert_eq!(terminal_size_from_fd(pty.slave), Some((173, 41)));

        pty.set_size(92, 27);

        assert_eq!(terminal_size_from_fd(pty.slave), Some((92, 27)));
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

    struct TestPty {
        master: libc::c_int,
        slave: libc::c_int,
    }

    impl TestPty {
        fn open() -> Self {
            let mut master = -1;
            let mut slave = -1;
            let result = unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            assert_eq!(result, 0, "openpty failed");
            Self { master, slave }
        }

        fn set_size(&self, cols: u16, rows: u16) {
            let size = libc::winsize {
                ws_row: rows,
                ws_col: cols,
                ws_xpixel: 0,
                ws_ypixel: 0,
            };
            let result = unsafe { libc::ioctl(self.slave, libc::TIOCSWINSZ, &size) };
            assert_eq!(result, 0, "TIOCSWINSZ failed");
        }
    }

    impl Drop for TestPty {
        fn drop(&mut self) {
            unsafe {
                libc::close(self.master);
                libc::close(self.slave);
            }
        }
    }
}
