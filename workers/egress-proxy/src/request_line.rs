//! Pure parse of an HTTP `CONNECT` request line into `(host, port)`.
//! The slice-#2 web-common connector issues `CONNECT` for both http and https,
//! so the proxy only ever parses CONNECT. Handles bracketed IPv6 authorities.

/// Parse `CONNECT host:port HTTP/1.1` (the leading line of a CONNECT tunnel
/// request). Returns the host (brackets stripped for IPv6) and port.
/// Returns `Err` for anything that isn't a well-formed CONNECT line.
pub fn parse_connect(line: &str) -> Result<(String, u16), String> {
    let line = line.trim_end_matches(['\r', '\n']);
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default();
    if method != "CONNECT" {
        return Err(format!("not a CONNECT request: {method:?}"));
    }
    let authority = parts.next().ok_or_else(|| "missing authority".to_string())?;
    // Reject a missing HTTP version (too-short line).
    if parts.next().is_none() {
        return Err("missing HTTP version".to_string());
    }
    split_authority(authority)
}

/// Split `host:port` — handling `[::1]:443` bracketed IPv6 — into parts.
fn split_authority(authority: &str) -> Result<(String, u16), String> {
    let (host, port_str) = if let Some(rest) = authority.strip_prefix('[') {
        // Bracketed IPv6: [host]:port
        let close = rest.find(']').ok_or_else(|| "unterminated [".to_string())?;
        let host = &rest[..close];
        let after = &rest[close + 1..];
        let port = after
            .strip_prefix(':')
            .ok_or_else(|| "missing :port after ]".to_string())?;
        (host.to_string(), port)
    } else {
        let (h, p) = authority
            .rsplit_once(':')
            .ok_or_else(|| "missing :port".to_string())?;
        (h.to_string(), p)
    };
    if host.is_empty() {
        return Err("empty host".to_string());
    }
    let port: u16 = port_str.parse().map_err(|_| format!("bad port {port_str:?}"))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hostname() {
        assert_eq!(
            parse_connect("CONNECT api.example.com:443 HTTP/1.1").unwrap(),
            ("api.example.com".to_string(), 443)
        );
    }

    #[test]
    fn parses_literal_v4() {
        assert_eq!(
            parse_connect("CONNECT 127.0.0.1:8888 HTTP/1.1\r\n").unwrap(),
            ("127.0.0.1".to_string(), 8888)
        );
    }

    #[test]
    fn parses_bracketed_v6() {
        assert_eq!(
            parse_connect("CONNECT [::1]:443 HTTP/1.1").unwrap(),
            ("::1".to_string(), 443)
        );
    }

    #[test]
    fn rejects_non_connect() {
        assert!(parse_connect("GET / HTTP/1.1").is_err());
    }

    #[test]
    fn rejects_missing_port() {
        assert!(parse_connect("CONNECT example.com HTTP/1.1").is_err());
    }

    #[test]
    fn rejects_missing_version() {
        assert!(parse_connect("CONNECT example.com:443").is_err());
    }

    #[test]
    fn rejects_bad_port() {
        assert!(parse_connect("CONNECT example.com:notaport HTTP/1.1").is_err());
    }
}
