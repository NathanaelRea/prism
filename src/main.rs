#[cfg(not(windows))]
#[tokio::main]
async fn main() {
    finish(prism::cli::run().await);
}

#[cfg(windows)]
fn main() {
    let thread = std::thread::Builder::new()
        .name("prism-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|error| format!("start async runtime: {error}"))?
                .block_on(prism::cli::run())
        });
    let result = match thread {
        Ok(thread) => thread.join(),
        Err(error) => {
            finish(Err(format!("start Prism main thread: {error}")));
            return;
        }
    };
    match result {
        Ok(result) => finish(result),
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

fn finish(result: Result<(), String>) {
    if let Err(error) = result {
        eprintln!("prism: {error}");
        std::process::exit(1);
    }
}
