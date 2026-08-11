use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(unix)]
use std::os::unix::net::{UnixListener as NativeListener, UnixStream as NativeStream};

#[cfg(windows)]
use interprocess::TryClone;
#[cfg(windows)]
use interprocess::local_socket::traits::{Listener as _, Stream as _};
#[cfg(windows)]
use interprocess::local_socket::{
    GenericNamespaced, Listener as NativeListener, ListenerNonblockingMode, ListenerOptions,
    Stream as NativeStream, ToNsName,
};
#[cfg(windows)]
use interprocess::os::windows::local_socket::ListenerOptionsExt;

#[cfg(unix)]
pub(super) const UNIX_SOCKET_PATH_BUDGET: usize = 103;
const SECRET_BYTES: usize = 32;

pub(super) type WorkerStream = NativeStream;
pub(super) type WorkerListener = NativeListener;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkerEndpoint {
    runtime: PathBuf,
    #[cfg(unix)]
    address: PathBuf,
    #[cfg(windows)]
    address: String,
}

impl WorkerEndpoint {
    pub(super) fn for_runtime(runtime: &Path) -> Result<Self, String> {
        #[cfg(unix)]
        {
            let address = runtime.join("worker.sock");
            let bytes = address.as_os_str().as_bytes();
            if bytes.contains(&0) {
                return Err(format!(
                    "Prism worker runtime directory {} produces a socket path containing a NUL byte; set PRISM_RUNTIME_DIR to a shorter valid private directory",
                    runtime.display()
                ));
            }
            if bytes.len() > UNIX_SOCKET_PATH_BUDGET {
                return Err(format!(
                    "Prism worker runtime directory {} produces a {}-byte socket path, exceeding the supported maximum of {UNIX_SOCKET_PATH_BUDGET} bytes; set PRISM_RUNTIME_DIR to a shorter private directory such as /tmp/prism-$UID",
                    runtime.display(),
                    bytes.len()
                ));
            }
            Ok(Self {
                runtime: runtime.to_path_buf(),
                address,
            })
        }
        #[cfg(windows)]
        {
            let hash = crate::util::stable_hash(runtime);
            Ok(Self {
                runtime: runtime.to_path_buf(),
                address: format!("prism-worker-{hash:016x}"),
            })
        }
    }

    #[cfg(unix)]
    pub(super) fn path(&self) -> &Path {
        &self.address
    }

    #[cfg(all(test, unix))]
    pub(super) fn as_path(&self) -> &Path {
        self.path()
    }

    pub(super) fn display(&self) -> String {
        #[cfg(unix)]
        return self.address.display().to_string();
        #[cfg(windows)]
        return format!(r"\\.\pipe\{}", self.address);
    }

    pub(super) fn secret_path(&self) -> PathBuf {
        self.runtime.join("worker.secret")
    }

    pub(super) fn connect(&self) -> io::Result<WorkerStream> {
        #[cfg(unix)]
        return WorkerStream::connect(&self.address);
        #[cfg(windows)]
        {
            let name = self.address.clone().to_ns_name::<GenericNamespaced>()?;
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            loop {
                match WorkerStream::connect(name.clone()) {
                    Ok(stream) => return Ok(stream),
                    Err(error)
                        if matches!(
                            error.kind(),
                            io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                        ) && std::time::Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error),
                }
            }
        }
    }

    pub(super) fn bind(&self) -> io::Result<WorkerListener> {
        #[cfg(unix)]
        return WorkerListener::bind(&self.address);
        #[cfg(windows)]
        {
            let deadline = Instant::now() + Duration::from_secs(1);
            loop {
                let name = self.address.clone().to_ns_name::<GenericNamespaced>()?;
                let descriptor =
                    crate::system::windows_security::private_pipe_security_descriptor()
                        .map_err(io::Error::other)?;
                match ListenerOptions::new()
                    .name(name)
                    .security_descriptor(descriptor)
                    .create_sync()
                {
                    Err(error)
                        if error.kind() == io::ErrorKind::PermissionDenied
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    result => return result,
                }
            }
        }
    }

    pub(super) fn remove_stale_address(&self) -> io::Result<()> {
        #[cfg(unix)]
        match fs::remove_file(&self.address) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
        #[cfg(windows)]
        Ok(())
    }

    pub(super) fn address_exists(&self) -> io::Result<bool> {
        #[cfg(unix)]
        return match fs::symlink_metadata(&self.address) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        };
        #[cfg(windows)]
        return match self.connect() {
            Ok(_) => Ok(true),
            Err(error) if endpoint_unavailable(&error) => Ok(false),
            Err(error) => Err(error),
        };
    }
}

pub(super) fn endpoint_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    )
}

pub(super) fn connection_closed(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

pub(super) fn accept(listener: &WorkerListener) -> io::Result<WorkerStream> {
    #[cfg(unix)]
    return listener.accept().map(|(stream, _)| stream);
    #[cfg(windows)]
    return listener.accept();
}

pub(super) fn set_listener_nonblocking(listener: &WorkerListener) -> io::Result<()> {
    #[cfg(unix)]
    return listener.set_nonblocking(true);
    #[cfg(windows)]
    return listener.set_nonblocking(ListenerNonblockingMode::Accept);
}

pub(super) fn try_clone_stream(stream: &WorkerStream) -> io::Result<WorkerStream> {
    #[cfg(unix)]
    return stream.try_clone();
    #[cfg(windows)]
    return TryClone::try_clone(stream);
}

pub(super) fn set_read_timeout(stream: &WorkerStream, timeout: Duration) -> io::Result<()> {
    #[cfg(unix)]
    return stream.set_read_timeout(Some(timeout));
    #[cfg(windows)]
    return stream.set_recv_timeout(Some(timeout));
}

pub(super) fn set_write_timeout(stream: &WorkerStream, timeout: Duration) -> io::Result<()> {
    #[cfg(unix)]
    return stream.set_write_timeout(Some(timeout));
    #[cfg(windows)]
    return stream.set_send_timeout(Some(timeout));
}

pub(super) fn prepare_runtime(runtime: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(runtime).ok();
    if metadata
        .as_ref()
        .is_some_and(|metadata| metadata.file_type().is_symlink())
    {
        return Err(format!(
            "Prism worker runtime directory is a symlink: {}",
            runtime.display()
        ));
    }
    #[cfg(unix)]
    if metadata.is_some_and(|metadata| metadata.uid() != unsafe { libc::geteuid() }) {
        return Err(format!(
            "Prism worker runtime directory is owned by another user: {}",
            runtime.display()
        ));
    }
    fs::create_dir_all(runtime).map_err(|error| format!("create worker runtime dir: {error}"))?;
    #[cfg(unix)]
    fs::set_permissions(runtime, fs::Permissions::from_mode(0o700))
        .map_err(|error| format!("secure worker runtime dir: {error}"))?;
    #[cfg(windows)]
    crate::system::windows_security::secure_path(runtime, true)
        .map_err(|error| format!("secure worker runtime dir: {error}"))?;
    Ok(())
}

pub(super) fn secure_listener(endpoint: &WorkerEndpoint) -> Result<(), String> {
    #[cfg(unix)]
    fs::set_permissions(endpoint.path(), fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure Prism worker socket: {error}"))?;
    #[cfg(windows)]
    let _ = endpoint;
    Ok(())
}

pub(super) fn create_secret(endpoint: &WorkerEndpoint) -> Result<String, String> {
    let mut bytes = [0_u8; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| format!("generate worker secret: {error}"))?;
    let secret = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let path = endpoint.secret_path();
    let mut options = OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .map_err(|error| format!("create worker authentication secret: {error}"))?;
    file.write_all(secret.as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| format!("write worker authentication secret: {error}"))?;
    #[cfg(unix)]
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
        .map_err(|error| format!("secure worker authentication secret: {error}"))?;
    #[cfg(windows)]
    crate::system::windows_security::secure_path(&path, false)
        .map_err(|error| format!("secure worker authentication secret: {error}"))?;
    Ok(secret)
}

pub(super) fn read_secret(endpoint: &WorkerEndpoint) -> Result<String, String> {
    let mut secret = String::new();
    fs::File::open(endpoint.secret_path())
        .and_then(|mut file| file.read_to_string(&mut secret))
        .map_err(|error| format!("read worker authentication secret: {error}"))?;
    let secret = secret.trim();
    if secret.len() != SECRET_BYTES * 2 || !secret.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("worker authentication secret is invalid".to_string());
    }
    Ok(secret.to_string())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn runtime(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "prism-worker-ipc-{label}-{}-{}",
            std::process::id(),
            crate::util::timestamp_nanos()
        ))
    }

    #[test]
    fn windows_named_pipe_supports_concurrent_authenticated_clients_and_rebind() {
        const CLIENTS: usize = 8;
        let runtime = runtime("concurrent");
        prepare_runtime(&runtime).unwrap();
        let endpoint = WorkerEndpoint::for_runtime(&runtime).unwrap();
        let secret = create_secret(&endpoint).unwrap();
        let listener = endpoint.bind().unwrap();
        assert!(
            endpoint.bind().is_err(),
            "a second listener stole the pipe name"
        );
        let barrier = Arc::new(Barrier::new(CLIENTS + 1));
        let server_secret = secret.clone();
        let server_barrier = Arc::clone(&barrier);
        let server = thread::spawn(move || {
            server_barrier.wait();
            for _ in 0..CLIENTS {
                let mut stream = accept(&listener).unwrap();
                let mut request = String::new();
                BufReader::new(&mut stream).read_line(&mut request).unwrap();
                let prefix = format!("auth {server_secret} ");
                let body = request.trim().strip_prefix(&prefix).unwrap();
                stream.write_all(format!("ok {body}\n").as_bytes()).unwrap();
            }
        });
        let clients = (0..CLIENTS)
            .map(|index| {
                let endpoint = endpoint.clone();
                let secret = secret.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    let mut stream = endpoint.connect().unwrap();
                    stream
                        .write_all(format!("auth {secret} request-{index}\n").as_bytes())
                        .unwrap();
                    let mut response = String::new();
                    BufReader::new(stream).read_line(&mut response).unwrap();
                    assert_eq!(response, format!("ok request-{index}\n"));
                })
            })
            .collect::<Vec<_>>();
        for client in clients {
            client.join().unwrap();
        }
        server.join().unwrap();

        let rebound = endpoint.bind().unwrap();
        drop(rebound);
        fs::remove_dir_all(runtime).unwrap();
    }

    #[test]
    fn windows_secret_is_random_and_validated() {
        let runtime = runtime("secret");
        prepare_runtime(&runtime).unwrap();
        let endpoint = WorkerEndpoint::for_runtime(&runtime).unwrap();
        let first = create_secret(&endpoint).unwrap();
        let second = create_secret(&endpoint).unwrap();
        assert_eq!(read_secret(&endpoint).unwrap(), second);
        assert_ne!(first, second);
        assert_eq!(second.len(), SECRET_BYTES * 2);
        fs::remove_dir_all(runtime).unwrap();
    }
}
