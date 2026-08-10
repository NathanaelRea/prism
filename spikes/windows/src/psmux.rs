use std::{
    ffi::{OsStr, OsString},
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use wait_timeout::ChildExt;

use crate::support::{SpikeResult, TempDir, fail, require, unique_name};

const PSMUX_VERSION: &str = "3.3.7";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

fn bounded_output(
    mut command: Command,
    description: &str,
    timeout: Duration,
) -> SpikeResult<Output> {
    // psmux descendants can inherit standard handles. Pipe-backed `output()` then waits for EOF
    // after the command process has exited, so capture into ordinary files instead.
    let capture_root = std::env::temp_dir();
    let stdout_path = capture_root.join(unique_name("prism-psmux-stdout"));
    let stderr_path = capture_root.join(unique_name("prism-psmux-stderr"));
    command
        .stdout(Stdio::from(fs::File::create(&stdout_path)?))
        .stderr(Stdio::from(fs::File::create(&stderr_path)?));
    let mut child = command.spawn()?;
    let Some(status) = child.wait_timeout(timeout)? else {
        let kill_error = child.kill().err();
        drop(child);
        let _ = fs::remove_file(stdout_path);
        let _ = fs::remove_file(stderr_path);
        return fail(format!(
            "timed out after {}s running {description}; kill error: {kill_error:?}",
            timeout.as_secs()
        ));
    };
    drop(child);

    let output = Output {
        status,
        stdout: fs::read(&stdout_path)?,
        stderr: fs::read(&stderr_path)?,
    };
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
    Ok(output)
}

fn run_mux(psmux: &OsStr, namespace: &str, args: &[&OsStr]) -> SpikeResult<Output> {
    let mut command = Command::new(psmux);
    command.arg("-L").arg(namespace).args(args);
    let output = bounded_output(command, &format!("psmux {args:?}"), COMMAND_TIMEOUT)?;
    if output.status.success() {
        Ok(output)
    } else {
        fail(format!(
            "psmux {:?} exited with {}: {}",
            args,
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_mux_strings(psmux: &OsStr, namespace: &str, args: &[&str]) -> SpikeResult<Output> {
    let args: Vec<_> = args.iter().map(OsStr::new).collect();
    run_mux(psmux, namespace, &args)
}

struct ServerCleanup {
    psmux: OsString,
    namespace: String,
}

impl Drop for ServerCleanup {
    fn drop(&mut self) {
        let mut command = Command::new(&self.psmux);
        command.arg("-L").arg(&self.namespace).arg("kill-server");
        if let Err(error) = bounded_output(command, "psmux kill-server cleanup", COMMAND_TIMEOUT) {
            eprintln!("[psmux] cleanup warning: {error}");
        }
    }
}

fn poll_capture(psmux: &OsStr, namespace: &str, target: &str, marker: &str) -> SpikeResult<String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = run_mux_strings(psmux, namespace, &["capture-pane", "-p", "-t", target])?;
        let capture = String::from_utf8(output.stdout)?;
        if capture.contains(marker) {
            return Ok(capture);
        }
        if Instant::now() >= deadline {
            return fail(format!(
                "psmux capture never contained {marker:?}: {capture:?}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn attached_count(psmux: &OsStr, namespace: &str, session: &str) -> SpikeResult<u32> {
    let output = run_mux_strings(
        psmux,
        namespace,
        &["list-sessions", "-F", "#{session_name}|#{session_attached}"],
    )?;
    let output = String::from_utf8(output.stdout)?;
    output
        .lines()
        .find_map(|line| {
            let (name, count) = line.split_once('|')?;
            if name == session {
                count.parse::<u32>().ok()
            } else {
                None
            }
        })
        .ok_or_else(|| format!("psmux omitted attached count for {session}: {output:?}").into())
}

fn window_size(psmux: &OsStr, namespace: &str, session: &str) -> SpikeResult<(u16, u16)> {
    let output = run_mux_strings(
        psmux,
        namespace,
        &[
            "list-windows",
            "-t",
            session,
            "-F",
            "#{window_width}x#{window_height}",
        ],
    )?;
    let output = String::from_utf8(output.stdout)?;
    let size = output.lines().next().unwrap_or_default();
    let (width, height) = size
        .split_once('x')
        .ok_or_else(|| format!("invalid psmux window size {size:?}"))?;
    Ok((width.parse()?, height.parse()?))
}

fn attach_resize_and_detach(
    psmux: &OsStr,
    namespace: &str,
    session: &str,
    marker: &str,
) -> SpikeResult {
    println!("[psmux] opening headless ConPTY client");
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    let mut command = CommandBuilder::new(psmux);
    command.args(["-L", namespace, "attach-session", "-t", session]);
    let mut child = pair.slave.spawn_command(command)?;
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader()?;
    let (output_tx, output_rx) = mpsc::sync_channel(1);
    let reader_thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        let result = reader.read_to_end(&mut output).map(|_| output);
        let _ = output_tx.send(result);
    });
    let mut writer = pair.master.take_writer()?;

    let interaction = (|| -> SpikeResult {
        // psmux queries the host terminal for colors for up to 500 ms before its input pump
        // starts. Give it time to attach before resizing or sending detach input. In-process
        // ConPTY output below proves the attachment without depending on a polling command.
        std::thread::sleep(Duration::from_secs(2));
        require(
            child.try_wait()?.is_none(),
            "psmux attach client exited before terminal interaction",
        )?;
        println!("[psmux] attach client running; resizing ConPTY client");
        pair.master.resize(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        std::thread::sleep(Duration::from_millis(250));

        println!("[psmux] resized; sending prefix+d");
        writer.write_all(&[0x02, b'd'])?;
        writer.flush()?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if child.try_wait()?.is_some() {
                println!("[psmux] attach client exited after detach");
                return Ok(());
            }
            if Instant::now() >= deadline {
                return fail("psmux attach client did not detach after prefix+d");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    })();

    if interaction.is_err() && matches!(child.try_wait(), Ok(None)) {
        let _ = child.kill();
    }
    drop(writer);
    drop(pair.master);
    interaction?;

    let output = output_rx
        .recv_timeout(Duration::from_secs(2))
        .map_err(|_| "psmux ConPTY output did not close after detach")??;
    reader_thread
        .join()
        .map_err(|_| "psmux ConPTY reader thread panicked")?;
    require(
        String::from_utf8_lossy(&output).contains(marker),
        "psmux attached client did not render the captured pane marker",
    )?;
    require(
        attached_count(psmux, namespace, session)? == 0,
        "psmux still reported an attached client after detach",
    )?;
    let (width, height) = window_size(psmux, namespace, session)?;
    require(
        width == 100 && (29..=30).contains(&height),
        format!("psmux did not retain the attached terminal size: {width}x{height}"),
    )
}

fn buffer_path(temp: &Path) -> std::path::PathBuf {
    temp.join("prompt with spaces and \u{96ea}.txt")
}

pub fn run_spike() -> SpikeResult {
    println!("[psmux] create, capture, command resize, ConPTY attach/resize/detach, and kill");
    let psmux =
        std::env::var_os("PRISM_WINDOWS_SPIKE_PSMUX").unwrap_or_else(|| OsString::from("psmux"));
    let mut version_command = Command::new(&psmux);
    version_command.arg("--version");
    let version = bounded_output(version_command, "psmux --version", COMMAND_TIMEOUT)?;
    require(version.status.success(), "psmux --version failed")?;
    let version = String::from_utf8(version.stdout)?;
    require(
        version.contains(PSMUX_VERSION),
        format!("expected psmux {PSMUX_VERSION}, got {version:?}"),
    )?;

    let namespace = unique_name("prism-spike");
    let old_session = "prism-phase0-old";
    let session = "prism-phase0-snow";
    let _cleanup = ServerCleanup {
        psmux: psmux.clone(),
        namespace: namespace.clone(),
    };
    run_mux_strings(
        &psmux,
        &namespace,
        &[
            "new-session",
            "-d",
            "-s",
            old_session,
            "-x",
            "80",
            "-y",
            "24",
        ],
    )?;
    run_mux_strings(
        &psmux,
        &namespace,
        &["rename-session", "-t", old_session, session],
    )?;

    let temp = TempDir::new("prism-windows-psmux")?;
    let prompt_path = buffer_path(temp.path());
    let marker = "PRISM_PSMUX_CAPTURE_\u{2603}";
    fs::write(&prompt_path, format!("Write-Output '{marker}'"))?;
    let load_args = [
        OsStr::new("load-buffer"),
        OsStr::new("-b"),
        OsStr::new("prism-phase0"),
        prompt_path.as_os_str(),
    ];
    run_mux(&psmux, &namespace, &load_args)?;
    run_mux_strings(
        &psmux,
        &namespace,
        &[
            "paste-buffer",
            "-d",
            "-b",
            "prism-phase0",
            "-t",
            &format!("{session}:0"),
        ],
    )?;
    run_mux_strings(
        &psmux,
        &namespace,
        &["send-keys", "-t", &format!("{session}:0"), "Enter"],
    )?;
    poll_capture(&psmux, &namespace, &format!("{session}:0"), marker)?;

    run_mux_strings(
        &psmux,
        &namespace,
        &[
            "resize-window",
            "-t",
            &format!("{session}:0"),
            "-x",
            "100",
            "-y",
            "30",
        ],
    )?;
    attach_resize_and_detach(&psmux, &namespace, session, marker)?;

    run_mux_strings(&psmux, &namespace, &["kill-session", "-t", session])?;
    let mut has_session = Command::new(&psmux);
    has_session
        .arg("-L")
        .arg(&namespace)
        .args(["has-session", "-t", session]);
    let output = bounded_output(has_session, "psmux has-session", COMMAND_TIMEOUT)?;
    require(
        !output.status.success(),
        "psmux session survived kill-session",
    )?;
    println!("[psmux] PASS");
    Ok(())
}
