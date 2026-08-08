use super::*;

#[test]
fn the_action_and_actor_are_stable_audit_contract() {
    // Renaming either breaks operator queries against audit_log.
    assert_eq!(ACTION_FORCE_ROUTING_DISABLED, "egress.force_routing_disabled");
    assert_eq!(ACTOR, "daemon");
}

#[test]
fn the_payload_names_the_env_var_that_controls_it() {
    let p = force_routing_disabled_payload();
    assert_eq!(p["env_var"], "KASTELLAN_EGRESS_FORCE_ROUTING");
    // The operator-visible phrase travels WITH the row, so an audit reader and
    // a log grepper are looking at the same string.
    assert_eq!(p["phrase"], FORCE_ROUTING_DISABLED_LOG_PHRASE);
}

#[test]
fn the_log_phrase_is_a_const_not_a_literal() {
    // #516/#524/#525 all shipped an operator-facing phrase as a bare literal
    // beside a const that existed for exactly that purpose. Assert through the
    // const so any rename moves both sides at once.
    assert!(FORCE_ROUTING_DISABLED_LOG_PHRASE.contains("FORCE-ROUTING DISABLED"));
}
