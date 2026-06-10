//! `ProxyConnectGet`: an `HttpGet` that reaches origins **only** through the
//! per-worker egress proxy's UDS via HTTP CONNECT. Used when force-routing is
//! active (`KASTELLAN_EGRESS_PROXY_UDS` set) — the worker has no other route
//! out. TLS stays end-to-end worker↔origin (the proxy tunnels ciphertext).

/// Build the CONNECT request head for `host:port`. Host is passed verbatim
/// (a name, never a resolved IP — the proxy resolves + range-checks). Pass the
/// host exactly as `url::Url::host_str()` yields it: IPv6 literals arrive
/// already bracketed (`[2606:4700::1111]`), which is the form both this request
/// line and the proxy's bracketed-IPv6 parser require — do not re-bracket.
fn build_connect_request(host: &str, port: u16) -> String {
    format!("CONNECT {host}:{port} HTTP/1.1\r\nHost: {host}:{port}\r\n\r\n")
}

/// Parse the proxy's status line, returning the numeric status code.
fn parse_status_line(line: &str) -> Result<u16, String> {
    let code = line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| format!("malformed status line: {line:?}"))?;
    code.parse::<u16>().map_err(|e| format!("bad status code: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_line_has_host_port_and_host_header() {
        let line = build_connect_request("example.com", 443);
        assert_eq!(
            line,
            "CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"
        );
    }

    #[test]
    fn connect_line_brackets_ipv6_literal() {
        // `url::Url::host_str()` returns IPv6 WITH brackets, so a bracketed host
        // is what we receive and what the proxy's request-line parser (slice #1,
        // bracketed-IPv6 aware) expects. Pass it through verbatim — do NOT
        // double-bracket and do NOT strip.
        let line = build_connect_request("[2606:4700::1111]", 443);
        assert_eq!(
            line,
            "CONNECT [2606:4700::1111]:443 HTTP/1.1\r\nHost: [2606:4700::1111]:443\r\n\r\n"
        );
    }

    #[test]
    fn parse_status_accepts_200() {
        assert_eq!(parse_status_line("HTTP/1.1 200 Connection Established\r\n").unwrap(), 200);
    }

    #[test]
    fn parse_status_rejects_403() {
        assert_eq!(parse_status_line("HTTP/1.1 403 Forbidden\r\n").unwrap(), 403);
    }

    #[test]
    fn parse_status_errors_on_garbage() {
        assert!(parse_status_line("garbage").is_err());
    }
}
