use super::*;

fn advertised() -> Vec<(String, String)> {
    [("mail", "mail.get_message"), ("mail", "mail.get_attachment_text"), ("mail", "mail.search"), ("sift", "sift.run")]
        .iter()
        .map(|(t, m)| (t.to_string(), m.to_string()))
        .collect()
}

#[test]
fn unset_knobs_give_the_defaults() {
    let cfg = sift_config(None, None, &advertised()).unwrap();
    assert_eq!(cfg, SiftConfig::default());
    assert_eq!(cfg.max_reads, 64);
    assert_eq!(cfg.read_methods, vec!["mail.get_message", "mail.get_attachment_text", "handoff.fetch"]);
}

#[test]
fn a_bad_max_reads_names_its_key() {
    for bad in ["0", "-1", "abc", ""] {
        let e = sift_config(Some(bad), None, &advertised()).unwrap_err();
        assert!(e.contains("KASTELLAN_SIFT_MAX_READS"), "{bad:?}: {e}");
    }
    assert_eq!(sift_config(Some(" 128 "), None, &advertised()).unwrap().max_reads, 128);
}

#[test]
fn an_unknown_read_method_names_its_key() {
    let e = sift_config(None, Some("mail.nope"), &advertised()).unwrap_err();
    assert!(e.contains("KASTELLAN_SIFT_READ_METHODS") && e.contains("mail.nope"), "{e}");
}

#[test]
fn sift_and_skills_are_refused_even_when_advertised() {
    for bad in ["sift.run", "mail.get_message,sift.run", "x.invoke_skill"] {
        let e = sift_config(None, Some(bad), &advertised()).unwrap_err();
        assert!(e.contains("KASTELLAN_SIFT_READ_METHODS"), "{bad}: {e}");
    }
}

#[test]
fn the_list_is_trimmed_and_handoff_fetch_is_always_known() {
    let cfg = sift_config(None, Some(" mail.search , handoff.fetch "), &advertised()).unwrap();
    assert_eq!(cfg.read_methods, vec!["mail.search", "handoff.fetch"]);
    assert!(sift_config(None, Some(" , "), &advertised()).is_err(), "an empty list");
}
