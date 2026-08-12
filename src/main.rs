#[tokio::main]
async fn main() {
    if let Err(error) = prism::cli::run().await {
        eprintln!("prism: {error}");
        std::process::exit(1);
    }
}
