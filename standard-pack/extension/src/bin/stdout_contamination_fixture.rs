use std::io::{BufRead, Write};

fn main() {
    let mut input = std::io::BufReader::new(std::io::stdin());
    let mut hello = String::new();
    let _ = input.read_line(&mut hello);
    let mut output = std::io::stdout();
    let _ = writeln!(output, "stdout is protocol-only");
    let _ = output.flush();
}
