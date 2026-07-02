//! Worker-side helper: read JSON-RPC requests from stdin, dispatch to a
//! [`Handler`], write JSON-RPC responses to stdout. Synchronous, line-delimited.

use std::io::{self, BufReader, Read, Write};

use crate::{
    codes, err_response, ok_response, read_capped_record, Record, Request, RpcError,
    MAX_RECORD_BYTES,
};

pub trait Handler {
    /// Handle one method call. Returning `Ok(value)` becomes a JSON-RPC
    /// success result; returning `Err(rpc_err)` becomes an error response.
    /// Workers should not panic; convert failures to [`RpcError`] instead.
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError>;
}

/// Run a request/response loop over [`io::stdin`] / [`io::stdout`].
/// Returns when stdin reaches EOF (i.e. the parent closed the pipe).
pub fn serve_stdio<H: Handler>(handler: &mut H) -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve(handler, &mut stdin.lock(), &mut stdout.lock())
}

/// Same as [`serve_stdio`] but takes generic reader/writer for unit-testing
/// the dispatch loop without touching real stdio.
pub fn serve<H, R, W>(handler: &mut H, reader: &mut R, writer: &mut W) -> io::Result<()>
where
    H: Handler,
    R: Read,
    W: Write,
{
    serve_capped(handler, reader, writer, MAX_RECORD_BYTES)
}

/// [`serve`] with an explicit per-record byte cap. Separated out so the OOM
/// guard (audit finding #2) can be unit-tested with a small cap instead of a
/// 64 MiB flood.
fn serve_capped<H, R, W>(
    handler: &mut H,
    reader: &mut R,
    writer: &mut W,
    cap: usize,
) -> io::Result<()>
where
    H: Handler,
    R: Read,
    W: Write,
{
    let mut br = BufReader::new(reader);
    loop {
        // Bounded read, shared with the client: a single record is never
        // buffered beyond `cap`. An over-cap record is a protocol error, not
        // something to keep buffering.
        let buf = match read_capped_record(&mut br, cap)? {
            Record::Eof => return Ok(()), // parent closed stdin
            Record::TooLarge => {
                let response = err_response(
                    serde_json::Value::Null,
                    RpcError::new(codes::INVALID_REQUEST, "request exceeded record cap"),
                );
                serde_json::to_writer(&mut *writer, &response)?;
                writer.write_all(b"\n")?;
                writer.flush()?;
                return Ok(());
            }
            Record::Line(buf) => buf,
        };
        if buf.iter().all(u8::is_ascii_whitespace) {
            continue; // blank line (incl. the trailing newline of an empty record)
        }
        // serde_json tolerates the trailing `\n` (surrounding whitespace is skipped).
        let response = match serde_json::from_slice::<Request>(&buf) {
            Ok(req) => match handler.call(&req.method, req.params) {
                Ok(result) => ok_response(req.id, result),
                Err(e) => err_response(req.id, e),
            },
            Err(e) => err_response(
                serde_json::Value::Null,
                RpcError::new(codes::PARSE_ERROR, format!("parse error: {e}")),
            ),
        };
        serde_json::to_writer(&mut *writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;
    impl Handler for Echo {
        fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value, RpcError> {
            if method == "echo" {
                Ok(params)
            } else {
                Err(RpcError::new(codes::METHOD_NOT_FOUND, format!("no such method: {method}")))
            }
        }
    }

    #[test]
    fn dispatches_method_and_returns_result() {
        let req = br#"{"jsonrpc":"2.0","id":1,"method":"echo","params":{"hi":"world"}}"#.to_vec();
        let mut input = req.as_slice();
        let mut output: Vec<u8> = Vec::new();
        let mut h = Echo;
        // Append a newline so read_line terminates the record.
        let mut buf = Vec::from(input);
        buf.push(b'\n');
        input = &buf[..];
        serve(&mut h, &mut input.to_vec().as_slice(), &mut output).unwrap();
        let line = String::from_utf8(output).unwrap();
        assert!(line.contains("\"result\""), "expected result, got {line}");
        assert!(line.contains("\"hi\":\"world\""), "expected echoed params, got {line}");
    }

    #[test]
    fn unknown_method_yields_method_not_found() {
        let mut input: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"nope\"}\n";
        let mut output: Vec<u8> = Vec::new();
        let mut h = Echo;
        serve(&mut h, &mut input, &mut output).unwrap();
        let line = String::from_utf8(output).unwrap();
        assert!(line.contains("\"error\""), "expected error, got {line}");
        assert!(line.contains("-32601"), "expected -32601, got {line}");
    }

    #[test]
    fn malformed_json_yields_parse_error_with_null_id() {
        let mut input: &[u8] = b"not json at all\n";
        let mut output: Vec<u8> = Vec::new();
        let mut h = Echo;
        serve(&mut h, &mut input, &mut output).unwrap();
        let line = String::from_utf8(output).unwrap();
        assert!(line.contains("\"error\""));
        assert!(line.contains("-32700"));
        assert!(line.contains("\"id\":null"));
    }

    #[test]
    fn over_cap_record_is_rejected_without_ooming() {
        // A 4 KiB record with no newline against a 16-byte cap: the loop must
        // reject it (INVALID_REQUEST) and stop, never buffering the flood.
        let flood = vec![b'x'; 4096];
        let mut input: &[u8] = &flood;
        let mut output: Vec<u8> = Vec::new();
        let mut h = Echo;
        super::serve_capped(&mut h, &mut input, &mut output, 16).unwrap();
        let line = String::from_utf8(output).unwrap();
        assert!(line.contains("-32600"), "expected INVALID_REQUEST, got {line}");
    }

    #[test]
    fn record_within_cap_still_dispatches() {
        let mut input: &[u8] = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"echo\",\"params\":42}\n";
        let mut output: Vec<u8> = Vec::new();
        let mut h = Echo;
        // Cap comfortably above the record length.
        super::serve_capped(&mut h, &mut input, &mut output, 4096).unwrap();
        let line = String::from_utf8(output).unwrap();
        assert!(line.contains("\"result\":42"), "expected echoed result, got {line}");
    }
}
