use std::io::{self, BufRead, BufWriter, Write};

fn main() -> io::Result<()> {
    let mut output = BufWriter::new(io::stdout().lock());
    for line in io::stdin().lock().lines() {
        let line = line?;
        let symbol = rustc_demangle::try_demangle(&line)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "unsupported Rust symbol"))?;
        writeln!(output, "{symbol:#}")?;
    }
    output.flush()
}
