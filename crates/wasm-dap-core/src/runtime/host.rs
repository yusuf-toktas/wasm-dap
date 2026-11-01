//! Host state for debuggee stores: WASI context and captured standard streams.

use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use tokio::io::AsyncWrite;
use wasmtime_wasi::cli::{InputFile, IsTerminal, StdoutStream};
use wasmtime_wasi::p1::WasiP1Ctx;
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamResult};
use wasmtime_wasi::{FsPerms, WasiCtxBuilder};

use crate::error::{Error, Result};

/// Data stored in every debuggee `Store`.
pub struct HostState {
    pub wasi: WasiP1Ctx,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// Receives bytes written by the guest to stdout or stderr.
pub type OutputSink = Arc<dyn Fn(StreamKind, &[u8]) + Send + Sync>;

#[derive(Debug, Clone, Default)]
pub enum StdinSource {
    #[default]
    Empty,
    Inherit,
    File(PathBuf),
}

/// WASI environment of a debuggee.
#[derive(Clone, Default)]
pub struct WasiOptions {
    /// Program arguments, including `argv[0]`.
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub inherit_env: bool,
    /// Preopened directories as (host path, guest path).
    pub dirs: Vec<(PathBuf, String)>,
    pub stdin: StdinSource,
    /// Captures guest output; inherits the host streams when `None`.
    pub output: Option<OutputSink>,
}

impl std::fmt::Debug for WasiOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WasiOptions")
            .field("args", &self.args)
            .field("env", &self.env)
            .field("inherit_env", &self.inherit_env)
            .field("dirs", &self.dirs)
            .field("stdin", &self.stdin)
            .field("captured", &self.output.is_some())
            .finish()
    }
}

impl HostState {
    pub fn new(options: &WasiOptions) -> Result<Self> {
        let mut builder = WasiCtxBuilder::new();
        builder.args(&options.args);
        if options.inherit_env {
            builder.inherit_env();
        }
        builder.envs(&options.env);
        for (host, guest) in &options.dirs {
            builder
                .preopened_dir(host, guest, FsPerms::ReadWrite)
                .map_err(|e| {
                    Error::Config(format!(
                        "cannot preopen directory {} as {guest}: {e}",
                        host.display()
                    ))
                })?;
        }
        match &options.stdin {
            StdinSource::Empty => {
                builder.stdin(io::empty());
            }
            StdinSource::Inherit => {
                builder.inherit_stdin();
            }
            StdinSource::File(path) => {
                let file = std::fs::File::open(path).map_err(|e| {
                    Error::Config(format!("cannot open stdin file {}: {e}", path.display()))
                })?;
                builder.stdin(InputFile::new(file));
            }
        }
        match &options.output {
            Some(sink) => {
                builder.stdout(CaptureStream {
                    sink: sink.clone(),
                    kind: StreamKind::Stdout,
                });
                builder.stderr(CaptureStream {
                    sink: sink.clone(),
                    kind: StreamKind::Stderr,
                });
            }
            None => {
                builder.inherit_stdout();
                builder.inherit_stderr();
            }
        }
        Ok(Self {
            wasi: builder.build_p1(),
        })
    }
}

/// Forwards guest writes synchronously so output stays ordered with debug events.
#[derive(Clone)]
struct CaptureStream {
    sink: OutputSink,
    kind: StreamKind,
}

impl IsTerminal for CaptureStream {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for CaptureStream {
    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }

    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }
}

impl OutputStream for CaptureStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        (self.sink)(self.kind, &bytes);
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(1024 * 1024)
    }
}

#[async_trait::async_trait]
impl Pollable for CaptureStream {
    async fn ready(&mut self) {}
}

impl AsyncWrite for CaptureStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        (self.sink)(self.kind, buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
