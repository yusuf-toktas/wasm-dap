//! Standard output that ends the process quietly when the reader goes away
//! (for example `wasm-dap disasm app.wasm | head`).

use std::io::{self, Write};

/// Writes one line. Names and paths come from the module being inspected, so
/// control characters other than newlines and tabs are escaped and cannot drive
/// the terminal.
pub fn write_line(args: std::fmt::Arguments<'_>) {
    let text = args.to_string();
    let mut line = String::with_capacity(text.len() + 1);
    for c in text.chars() {
        if c.is_control() && c != '\n' && c != '\t' {
            line.extend(c.escape_default());
        } else {
            line.push(c);
        }
    }
    line.push('\n');
    let mut out = io::stdout().lock();
    let result = out.write_all(line.as_bytes()).and_then(|()| out.flush());
    if let Err(error) = result {
        if error.kind() == io::ErrorKind::BrokenPipe {
            std::process::exit(0);
        }
        eprintln!("error: failed to write output: {error}");
        std::process::exit(1);
    }
}

#[macro_export]
macro_rules! outln {
    () => {
        $crate::output::write_line(format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::output::write_line(format_args!($($arg)*))
    };
}
