use std::io::{self, BufReader, Write};
use std::net::TcpListener;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use wasm_dap_core::server::Server;

use crate::cli::ServeArgs;

pub fn run(args: ServeArgs) -> Result<ExitCode> {
    match args.port {
        None => {
            let mut server = Server::new(BufReader::new(io::stdin()), io::stdout().lock());
            server.run()?;
        }
        Some(port) => {
            // Any local user can connect to a loopback port, and a session can run
            // programs with access to the user's files, so TCP clients must prove
            // they know a secret that only the launching user has seen.
            let token = match args.token {
                Some(token) if token.trim().is_empty() => bail!("--token must not be empty"),
                Some(token) => {
                    if std::env::var("WASM_DAP_TOKEN").ok().as_deref() != Some(token.as_str()) {
                        eprintln!(
                            "wasm-dap: warning: other local users can read --token from the \
                             process list; set WASM_DAP_TOKEN instead"
                        );
                    }
                    token
                }
                None => generate_token()?,
            };
            let listener = TcpListener::bind(("127.0.0.1", port))
                .with_context(|| format!("cannot listen on 127.0.0.1:{port}"))?;
            let address = listener.local_addr()?;
            eprintln!("wasm-dap: listening for DAP clients on {address}");
            eprintln!("wasm-dap: add \"serverToken\": \"{token}\" to the launch configuration");
            for stream in listener.incoming() {
                // Accept errors concern a single connection (see accept(2)), and a
                // closed stderr must not stop the server, so neither ends the loop.
                let connection =
                    stream.and_then(|stream| Ok((BufReader::new(stream.try_clone()?), stream)));
                let (reader, stream) = match connection {
                    Ok(connection) => connection,
                    Err(e) => {
                        let _ = writeln!(io::stderr(), "wasm-dap: cannot accept a connection: {e}");
                        thread::sleep(Duration::from_millis(100));
                        continue;
                    }
                };
                let mut server = Server::new(reader, stream).with_token(token.clone());
                if let Err(e) = server.run() {
                    let _ = writeln!(io::stderr(), "wasm-dap: session ended with an error: {e}");
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// 128 random bits from the operating system's CSPRNG, hex encoded.
fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|e| anyhow!("cannot generate a session token: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
