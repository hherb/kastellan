//! kastellan-protocol: minimal JSON-RPC 2.0 over stdio for tool workers.
//!
//! One JSON object per line on stdin / stdout. This is compatible with the
//! Model Context Protocol's stdio transport and intentionally trivial — no
//! frameworks, no async, no codegen, just std + serde_json. We can swap in
//! a richer MCP implementation later without changing the trust boundary.
//!
//! Server side: workers implement [`Handler`] and call [`serve_stdio`].
//! Client side: the agent core spawns a worker (under sandbox) and talks to
//! it through [`Client::from_child`].

pub mod client;
pub mod server;

use std::io::{self, BufRead, Read};

use serde::{Deserialize, Serialize};

/// Maximum bytes buffered for a single `\n`-terminated JSON-RPC record before
/// the read is abandoned with an error.
///
/// The transport is line-delimited, so a peer that never emits a newline would
/// otherwise drive `read_line` to allocate without bound — a compromised or
/// malfunctioning worker could OOM the core this way (security audit
/// 2026-07-02, finding #2). This ceiling is deliberately far above any
/// legitimate single response: workers self-cap their outputs well below it
/// (web-fetch ~100 KiB text, python-exec 256 KiB captures) and the largest
/// per-task handoff budget is 64 MiB. A record strictly larger than this is
/// not a valid message and is rejected rather than buffered.
pub const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;

/// Outcome of reading one `\n`-terminated record with a byte ceiling, via
/// [`read_capped_record`].
#[derive(Debug)]
pub enum Record {
    /// EOF before any byte was read (the peer closed the stream).
    Eof,
    /// A complete record, including any trailing `\n`, within the cap.
    Line(Vec<u8>),
    /// The record would exceed the cap without a newline — a flood or a
    /// desynced stream. The caller decides how to surface it (the client
    /// treats it as a dead worker; the server answers `INVALID_REQUEST`).
    TooLarge,
}

/// Read one `\n`-terminated record from `r`, buffering at most `cap` bytes.
///
/// Shared by the client and server so both agree, byte-for-byte, on what a
/// valid record is (the cap counts the whole record, newline included).
/// `take(cap + 1)` bounds the allocation so a peer that never emits `\n`
/// cannot drive the reader to OOM (security audit 2026-07-02, finding #2).
pub fn read_capped_record<R: BufRead>(r: &mut R, cap: usize) -> io::Result<Record> {
    let mut buf: Vec<u8> = Vec::new();
    let n = (&mut *r).take(cap as u64 + 1).read_until(b'\n', &mut buf)?;
    if n == 0 {
        return Ok(Record::Eof);
    }
    if n > cap {
        return Ok(Record::TooLarge);
    }
    Ok(Record::Line(buf))
}

#[cfg(test)]
mod record_tests {
    use super::*;

    #[test]
    fn read_capped_record_returns_a_normal_line() {
        let mut r: &[u8] = b"{\"ok\":true}\nnext";
        match read_capped_record(&mut r, 1024).unwrap() {
            Record::Line(buf) => assert_eq!(&buf, b"{\"ok\":true}\n"),
            other => panic!("expected Line, got {other:?}"),
        }
    }

    #[test]
    fn read_capped_record_eof_is_eof() {
        let mut r: &[u8] = b"";
        assert!(matches!(read_capped_record(&mut r, 1024).unwrap(), Record::Eof));
    }

    #[test]
    fn read_capped_record_rejects_over_cap_line_without_ooming() {
        // A 10 KiB line with NO newline against a 16-byte cap: the reader must
        // buffer at most cap+1 bytes and report TooLarge, not grow unbounded.
        let flood = vec![b'a'; 10 * 1024];
        let mut r: &[u8] = &flood;
        assert!(matches!(read_capped_record(&mut r, 16).unwrap(), Record::TooLarge));
    }

    #[test]
    fn read_capped_record_accepts_record_at_cap() {
        // 15 content bytes + newline = 16 total; the cap counts the whole
        // record (newline included), so this is exactly at budget and accepted.
        let mut r: &[u8] = b"aaaaaaaaaaaaaaa\n";
        match read_capped_record(&mut r, 16).unwrap() {
            Record::Line(buf) => assert_eq!(buf.len(), 16),
            other => panic!("expected Line, got {other:?}"),
        }
    }
}

/// JSON-RPC 2.0 error codes used by kastellan. Subset of the spec plus our own
/// app-level codes in the -32000..-32099 reserved range.
pub mod codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;

    /// Tool-call rejected by the worker's local policy (e.g. argv not in allowlist).
    pub const POLICY_DENIED: i32 = -32001;
    /// The underlying operation failed (worker reached the system call but it errored).
    pub const OPERATION_FAILED: i32 = -32002;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub jsonrpc: String,
    pub id: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("jsonrpc error {code}: {message}")]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

impl RpcError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.data = Some(data);
        self
    }
}

/// Build a successful response for the given request id.
pub fn ok_response(id: serde_json::Value, result: serde_json::Value) -> Response {
    Response {
        jsonrpc: "2.0".into(),
        id,
        result: Some(result),
        error: None,
    }
}

/// Build an error response for the given request id.
pub fn err_response(id: serde_json::Value, err: RpcError) -> Response {
    Response {
        jsonrpc: "2.0".into(),
        id,
        result: None,
        error: Some(err),
    }
}
