fn main() {
    if let Err(error) = prism::cli::run() {
        prism::cli::emit_fatal_error(&error);
        eprintln!("prism: {error}");
        std::process::exit(1);
    }
}
