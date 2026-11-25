//! Newline-delimited JSON-RPC transport.
//!
//! Buffering and backpressure policy:
//! * Strictly sequential: read one line, handle it, write at most one response, flush,
//!   then read the next line. There is no internal request or response queue.
//! * Input memory is bounded: a line longer than [`MAX_LINE_BYTES`] is discarded up to its
//!   newline and answered with `-32600`; the reusable line buffer never holds more than
//!   `MAX_LINE_BYTES + 1` bytes.
//! * Backpressure is the OS pipe: if the client stops reading stdout, `write_all` waits,
//!   so the server stops reading stdin, and the client's writes eventually block too.
//! * Each response is flushed immediately, so a client never waits on a partially
//!   buffered reply.
//! * EOF on the input ends the loop cleanly.

use std::io;

use serde_json::{json, Value};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::jsonrpc::{codes, parse_message, ErrorObject, Response};
use crate::server::{handle, Session};

/// Maximum accepted line length in bytes (1 MiB).
pub const MAX_LINE_BYTES: usize = 1 << 20;

enum LineRead {
    Line,
    TooLong,
    Eof,
}

async fn read_bounded_line<R>(reader: &mut R, buf: &mut Vec<u8>, max: usize) -> io::Result<LineRead>
where
    R: AsyncBufRead + Unpin,
{
    let limit = u64::try_from(max).unwrap_or(u64::MAX).saturating_add(1);
    buf.clear();
    let n = (&mut *reader).take(limit).read_until(b'\n', buf).await?;
    if n == 0 {
        return Ok(LineRead::Eof);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        return Ok(LineRead::Line);
    }
    if buf.len() <= max {
        // Final line without a trailing newline.
        return Ok(LineRead::Line);
    }
    // Over the limit: drain the rest of this line in bounded chunks.
    loop {
        buf.clear();
        let n = (&mut *reader).take(limit).read_until(b'\n', buf).await?;
        if n == 0 || buf.last() == Some(&b'\n') {
            break;
        }
    }
    buf.clear();
    Ok(LineRead::TooLong)
}

fn response_for_line(session: &mut Session<'_>, bytes: &[u8]) -> Option<Response> {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return Some(Response::failure(
            Value::Null,
            ErrorObject::new(codes::PARSE_ERROR, "parse error")
                .with_data(json!({ "detail": "input is not valid UTF-8" })),
        ));
    };
    // RFC 8259 section 8.1 allows ignoring a byte order mark; some Windows shells add one.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if text.trim().is_empty() {
        return None;
    }
    match parse_message(text) {
        Ok(request) => handle(session, request),
        Err(error_response) => Some(error_response),
    }
}

/// Serve one session until `reader` reaches EOF.
///
/// # Errors
///
/// Propagates I/O errors from the reader or writer (e.g. a closed stdout).
pub async fn serve<R, W>(session: &mut Session<'_>, mut reader: R, mut writer: W) -> io::Result<()>
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut line = Vec::with_capacity(4096);
    loop {
        let response = match read_bounded_line(&mut reader, &mut line, MAX_LINE_BYTES).await? {
            LineRead::Eof => return Ok(()),
            LineRead::TooLong => Some(Response::failure(
                Value::Null,
                ErrorObject::new(codes::INVALID_REQUEST, "invalid request")
                    .with_data(json!({ "detail": "line exceeds maximum length" })),
            )),
            LineRead::Line => response_for_line(session, &line),
        };
        if let Some(response) = response {
            let mut out = serde_json::to_vec(&response)?;
            out.push(b'\n');
            writer.write_all(&out).await?;
            writer.flush().await?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::scope::lookup_principal;

    async fn run_lines(input: &[u8]) -> Vec<Value> {
        let catalog = Catalog::synthetic();
        let mut session = Session::new(&catalog, lookup_principal("viewer-all").unwrap());
        let mut out = Vec::new();
        serve(&mut session, input, &mut out).await.unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn one_response_per_request_and_none_for_notifications() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n\
{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n\
\n\
{oops\n\
{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"ping\"}";
        let out = run_lines(input).await;
        assert_eq!(out.len(), 3);
        assert_eq!(out[0]["id"], 1);
        assert_eq!(out[1]["error"]["code"], codes::PARSE_ERROR);
        assert_eq!(out[2]["id"], 2);
    }

    #[tokio::test]
    async fn oversized_line_is_rejected_and_stream_continues() {
        let mut input = vec![b'x'; MAX_LINE_BYTES + 10];
        input.extend_from_slice(b"\n{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"ping\"}\n");
        let out = run_lines(&input).await;
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["error"]["code"], codes::INVALID_REQUEST);
        assert_eq!(out[1]["id"], 3);
    }

    #[tokio::test]
    async fn leading_byte_order_mark_is_ignored() {
        let out =
            run_lines(b"\xef\xbb\xbf{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n").await;
        assert_eq!(out[0]["id"], 1);
        assert!(out[0]["result"].is_object());
    }

    #[tokio::test]
    async fn invalid_utf8_is_parse_error() {
        let out = run_lines(b"\xff\xfe\n").await;
        assert_eq!(out[0]["error"]["code"], codes::PARSE_ERROR);
    }
}
