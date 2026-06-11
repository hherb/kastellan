use super::looks_like_tls;

#[test]
fn tls_handshake_record_byte_is_recognised() {
    // 0x16 == TLS ContentType::Handshake — the first byte of a ClientHello.
    assert!(looks_like_tls(0x16));
}

#[test]
fn plaintext_http_first_bytes_are_not_tls() {
    // 'G', 'C', etc. — none are 0x16.
    assert!(!looks_like_tls(b'G'));
    assert!(!looks_like_tls(b'C'));
    assert!(!looks_like_tls(0x00));
    assert!(!looks_like_tls(0x17)); // application-data, not handshake
}
