use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use file_id::get_file_id;
use fs4::{FileExt, TryLockError};
use windows::{
    Win32::Storage::FileSystem::{REPLACE_FILE_FLAGS, ReplaceFileW},
    core::PCWSTR,
};

use crate::support::{SpikeResult, TempDir, fail, require, unique_name};

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn sync_file(path: &Path) -> SpikeResult {
    OpenOptions::new().write(true).open(path)?.sync_all()?;
    Ok(())
}

fn atomic_replace(target: &Path, contents: &[u8]) -> SpikeResult {
    let file_name = target
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or("replacement target has no UTF-8 filename")?;
    let staging = target.with_file_name(format!(".{file_name}.{}.stage", unique_name("replace")));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    let target_wide = wide(target);
    let staging_wide = wide(&staging);
    let result = unsafe {
        ReplaceFileW(
            PCWSTR(target_wide.as_ptr()),
            PCWSTR(staging_wide.as_ptr()),
            PCWSTR::null(),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    };
    if let Err(error) = result {
        let _ = fs::remove_file(&staging);
        return Err(error.into());
    }
    sync_file(target)?;
    Ok(())
}

pub fn run_lock_holder(lock_path: PathBuf, ready_path: PathBuf) -> SpikeResult {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    FileExt::lock(&file)?;
    fs::write(ready_path, b"locked\n")?;
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}

fn wait_for_file(path: &Path) -> SpikeResult {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.exists() {
        if Instant::now() >= deadline {
            return fail(format!("timed out waiting for {}", path.display()));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

fn lock_spike(temp: &Path) -> SpikeResult {
    let lock_path = temp.join("worker.lock");
    let first = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    let second = OpenOptions::new().read(true).write(true).open(&lock_path)?;
    FileExt::try_lock(&first)?;
    require(
        matches!(FileExt::try_lock(&second), Err(TryLockError::WouldBlock)),
        "fs4 allowed concurrent exclusive lock owners",
    )?;
    FileExt::unlock(&first)?;
    FileExt::try_lock(&second)?;
    FileExt::unlock(&second)?;

    let ready = temp.join("lock.ready");
    let mut holder = Command::new(std::env::current_exe()?)
        .arg("--lock-holder")
        .arg(&lock_path)
        .arg(&ready)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    wait_for_file(&ready)?;
    let contender = OpenOptions::new().read(true).write(true).open(&lock_path)?;
    require(
        matches!(FileExt::try_lock(&contender), Err(TryLockError::WouldBlock)),
        "cross-process lock holder did not exclude a contender",
    )?;
    holder.kill()?;
    holder.wait()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match FileExt::try_lock(&contender) {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    FileExt::unlock(&contender)?;

    fs::write(&lock_path, b"permanent lock inode\n")?;
    let original = OpenOptions::new().read(true).write(true).open(&lock_path)?;
    FileExt::try_lock(&original)?;
    let staging = temp.join("replacement.lock");
    fs::write(&staging, b"replacement lock inode\n")?;
    let lock_wide = wide(&lock_path);
    let staging_wide = wide(&staging);
    match unsafe {
        ReplaceFileW(
            PCWSTR(lock_wide.as_ptr()),
            PCWSTR(staging_wide.as_ptr()),
            PCWSTR::null(),
            REPLACE_FILE_FLAGS(0),
            None,
            None,
        )
    } {
        Ok(()) => {
            let replacement = OpenOptions::new().read(true).write(true).open(&lock_path)?;
            FileExt::try_lock(&replacement)?;
            FileExt::unlock(&replacement)?;
            println!(
                "[persistence] lock replacement bypassed the old file lock; permanent lock paths are required"
            );
        }
        Err(error) => {
            println!("[persistence] Windows denied replacement of a locked file: {error}");
        }
    }
    FileExt::unlock(&original)?;
    Ok(())
}

pub fn run_spike() -> SpikeResult {
    println!("[persistence] ReplaceFileW, FlushFileBuffers, file identity, and fs4 locks");
    let temp = TempDir::new("prism-windows-persistence")?;
    let target = temp.path().join("prism.db.state");
    let old_contents = b"generation=old\n";
    let new_contents = "generation=new\nunicode=\u{2603}\n".as_bytes();
    fs::write(&target, old_contents)?;
    sync_file(&target)?;

    let old_identity = get_file_id(&target)?;
    let mut old_handle = File::open(&target)?;
    atomic_replace(&target, new_contents)?;
    let new_identity = get_file_id(&target)?;
    require(
        old_identity != new_identity,
        "atomic replacement retained stale file identity",
    )?;
    require(
        fs::read(&target)? == new_contents,
        "replacement path does not contain new bytes",
    )?;
    let mut old_observation = Vec::new();
    old_handle.read_to_end(&mut old_observation)?;
    require(
        old_observation == old_contents,
        "open pre-replacement handle stopped observing the old file",
    )?;

    for generation in 0..32 {
        let before = get_file_id(&target)?;
        let contents = format!("generation={generation}\n");
        atomic_replace(&target, contents.as_bytes())?;
        require(
            get_file_id(&target)? != before,
            format!("generation {generation} did not replace file identity"),
        )?;
        require(
            fs::read(&target)? == contents.as_bytes(),
            format!("generation {generation} replacement bytes mismatch"),
        )?;
    }

    lock_spike(temp.path())?;
    println!("[persistence] PASS");
    Ok(())
}
