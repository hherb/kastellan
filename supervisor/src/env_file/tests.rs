use super::*;

#[test]
fn parse_env_file_skips_comments_blanks_and_keeps_embedded_equals() {
    let parsed = parse_env_file("# header\n\nFOO=bar\n  BAZ =qux=zap\nnokey\n");
    assert_eq!(
        parsed,
        vec![
            ("FOO".to_string(), "bar".to_string()),
            // key trimmed; value taken verbatim after the first '=' (so an
            // embedded '=', e.g. a URL query, is preserved). Lines without '='
            // ("nokey") and '#' comments are skipped.
            ("BAZ".to_string(), "qux=zap".to_string()),
        ]
    );
}

#[test]
fn merge_env_file_values_override_inline_env_keeping_position() {
    let mut env = vec![("A".into(), "1".into()), ("B".into(), "2".into())];
    merge_env(&mut env, vec![("B".into(), "override".into()), ("C".into(), "3".into())]);
    assert_eq!(
        env,
        vec![
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "override".to_string()), // overridden in place
            ("C".to_string(), "3".to_string()),        // new key appended
        ]
    );
}
