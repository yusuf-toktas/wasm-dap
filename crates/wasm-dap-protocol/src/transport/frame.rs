use std::io::{self, BufRead, Read, Write};

use crate::message::Message;

const MAX_CONTENT_LENGTH: usize = 16 * 1024 * 1024;
const MAX_HEADER_LINE: u64 = 4096;

#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    #[error("missing Content-Length header")]
    MissingContentLength,

    #[error("invalid Content-Length header: {0:?}")]
    InvalidContentLength(String),

    #[error("message body too large: {0} bytes")]
    MessageTooLarge(usize),

    #[error("header line longer than {MAX_HEADER_LINE} bytes")]
    HeaderTooLong,

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

pub struct FrameReader<R> {
    inner: R,
}

impl<R: BufRead> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    pub fn read_message(&mut self) -> Result<Option<Message>, TransportError> {
        let content_length = match self.read_headers()? {
            Some(len) => len,
            None => return Ok(None),
        };

        if content_length > MAX_CONTENT_LENGTH {
            return Err(TransportError::MessageTooLarge(content_length));
        }

        let mut body = vec![0u8; content_length];
        self.inner.read_exact(&mut body)?;

        let msg = serde_json::from_slice(&body)?;
        Ok(Some(msg))
    }

    fn read_headers(&mut self) -> Result<Option<usize>, TransportError> {
        let mut content_length: Option<usize> = None;
        let mut line = String::new();
        let mut saw_header = false;

        loop {
            line.clear();
            let bytes_read = (&mut self.inner)
                .take(MAX_HEADER_LINE)
                .read_line(&mut line)?;
            if bytes_read == 0 {
                if saw_header {
                    return Err(TransportError::MissingContentLength);
                }
                return Ok(None);
            }
            if !line.ends_with('\n') && bytes_read as u64 == MAX_HEADER_LINE {
                return Err(TransportError::HeaderTooLong);
            }

            let header = line.trim_end_matches(['\r', '\n']);
            if header.is_empty() {
                return content_length
                    .map(Some)
                    .ok_or(TransportError::MissingContentLength);
            }

            saw_header = true;

            if let Some(value) = header.strip_prefix("Content-Length:") {
                let value = value.trim();
                let parsed = value
                    .parse::<usize>()
                    .map_err(|_| TransportError::InvalidContentLength(value.to_string()))?;
                content_length = Some(parsed);
            }
        }
    }
}

pub struct FrameWriter<W> {
    inner: W,
}

impl<W: Write> FrameWriter<W> {
    pub fn new(inner: W) -> Self {
        Self { inner }
    }

    pub fn write_message(&mut self, msg: &Message) -> Result<(), TransportError> {
        let body = serde_json::to_vec(msg)?;
        write!(self.inner, "Content-Length: {}\r\n\r\n", body.len())?;
        self.inner.write_all(&body)?;
        self.inner.flush()?;
        Ok(())
    }
}
