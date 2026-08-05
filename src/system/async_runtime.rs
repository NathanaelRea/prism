use std::future::Future;
use std::io;
use std::sync::OnceLock;

use tokio::runtime::{Handle, Runtime, RuntimeFlavor};

static APPLICATION_RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();

/// Compatibility seam for synchronous application entry points while repository persistence is
/// cut over to async interfaces. A daemon reuses its existing multi-thread runtime; standalone
/// synchronous commands share one process-wide runtime instead of constructing one per query.
pub(crate) fn block_on<F: Future>(future: F) -> Result<F::Output, io::Error> {
    if let Ok(handle) = Handle::try_current() {
        return match handle.runtime_flavor() {
            RuntimeFlavor::MultiThread => {
                Ok(tokio::task::block_in_place(|| handle.block_on(future)))
            }
            _ => Err(io::Error::other(
                "a synchronous persistence adapter cannot run inside a current-thread Tokio runtime",
            )),
        };
    }

    let runtime = APPLICATION_RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_time()
            .build()
            .map_err(|error| error.to_string())
    });
    runtime
        .as_ref()
        .map_err(|error| io::Error::other(error.clone()))
        .map(|runtime| runtime.block_on(future))
}
