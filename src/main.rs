fn main() {
    if let Err(error) = prism::cli::run() {
        eprintln!("prism: {error}");
        std::process::exit(1);
    }
}
