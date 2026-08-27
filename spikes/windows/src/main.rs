#[cfg(not(windows))]
compile_error!("the Prism Windows feasibility spikes only build for Windows");

#[cfg(windows)]
mod ipc;
#[cfg(windows)]
mod persistence;
#[cfg(windows)]
mod process;
#[cfg(windows)]
mod psmux;
#[cfg(windows)]
mod recorder;
#[cfg(windows)]
mod security;
#[cfg(windows)]
mod support;

#[cfg(windows)]
use std::{ffi::OsString, path::PathBuf};

#[cfg(windows)]
use support::{SpikeResult, fail};

#[cfg(windows)]
fn required_path(args: &[OsString], index: usize, name: &str) -> SpikeResult<PathBuf> {
    args.get(index)
        .map(PathBuf::from)
        .ok_or_else(|| format!("missing {name}").into())
}

#[cfg(windows)]
fn required_string(args: &[OsString], index: usize, name: &str) -> SpikeResult<String> {
    args.get(index)
        .and_then(|value| value.to_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("missing or non-Unicode {name}").into())
}

#[cfg(windows)]
fn required_u32(args: &[OsString], index: usize, name: &str) -> SpikeResult<u32> {
    required_string(args, index, name)?
        .parse()
        .map_err(|error| format!("invalid {name}: {error}").into())
}

#[cfg(windows)]
async fn run_named_spike(name: &str) -> SpikeResult {
    match name {
        "acl" => security::run_spike(),
        "process" => process::run_spike().await,
        "worker-ipc" => ipc::run_spike().await,
        "recorder" => recorder::run_spike().await,
        "persistence" => persistence::run_spike(),
        "psmux" => psmux::run_spike(),
        "all" => {
            security::run_spike()?;
            process::run_spike().await?;
            ipc::run_spike().await?;
            recorder::run_spike().await?;
            persistence::run_spike()?;
            psmux::run_spike()
        }
        _ => fail(format!("unknown spike {name:?}")),
    }
}

#[cfg(windows)]
async fn run(args: Vec<OsString>) -> SpikeResult {
    match args.get(1).and_then(|argument| argument.to_str()) {
        Some("--process-root") => {
            process::run_root(
                required_string(&args, 2, "containment job name")?,
                required_path(&args, 3, "PID file")?,
                required_path(&args, 4, "graceful marker")?,
            )
            .await
        }
        Some("--process-nested-child") => {
            process::run_nested_child(
                required_string(&args, 2, "containment job name")?,
                required_path(&args, 3, "PID file")?,
            )
            .await
        }
        Some("--process-leaf") => process::run_leaf(required_path(&args, 2, "PID file")?).await,
        Some("--process-signal") => {
            process::run_signal_sender(required_u32(&args, 2, "process-group ID")?)
        }
        Some("--lock-holder") => persistence::run_lock_holder(
            required_path(&args, 2, "lock path")?,
            required_path(&args, 3, "ready path")?,
        ),
        Some("--spike") => {
            let spike = required_string(&args, 2, "spike name")?;
            run_named_spike(&spike).await
        }
        Some(command) if command.starts_with("--") => {
            fail(format!("unknown helper mode {command}"))
        }
        Some(_) => fail("the Windows spikes do not accept positional arguments"),
        None => {
            println!("Prism native Windows phase 0 feasibility spikes");
            run_named_spike("all").await?;
            println!("All native Windows phase 0 feasibility spikes passed");
            Ok(())
        }
    }
}

#[cfg(windows)]
#[tokio::main(flavor = "multi_thread")]
async fn main() {
    if let Err(error) = run(std::env::args_os().collect()).await {
        eprintln!("Windows feasibility spike failed: {error}");
        std::process::exit(1);
    }
}
