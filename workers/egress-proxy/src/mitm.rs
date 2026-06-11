//! TLS interception: decide whether a tunnel is TLS, and if so terminate the
//! worker's TLS with a per-instance-CA leaf and re-originate a validated TLS
//! session to the pinned origin. The pure peek predicate is split from the
//! async I/O so the branch logic is unit-testable without sockets.

/// True iff `first_byte` is the TLS record ContentType for `handshake` (0x16),
/// i.e. the first byte of a ClientHello. Anything else is treated as an
/// already-plaintext tunnel (plain-HTTP-over-CONNECT) and passed through.
pub fn looks_like_tls(first_byte: u8) -> bool {
    first_byte == 0x16
}

#[cfg(test)]
mod tests;
