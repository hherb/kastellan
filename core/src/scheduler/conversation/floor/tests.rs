//! Unit tests for [`super::inherit_floor`].

use super::*;
use crate::scheduler::conversation::record::TurnRecord;

fn turn_with_class(task_id: i64, data_class: DataClass) -> Turn {
    Turn {
        task_id,
        finished_at: time::OffsetDateTime::from_unix_timestamp(1_757_000_000).expect("timestamp"),
        user: "q".into(),
        answer: "a".into(),
        data_class: Some(data_class),
        record: Some(TurnRecord { calls: vec![], omitted_calls: 0, data_class }),
    }
}

#[test]
fn the_floor_rises_to_the_most_sensitive_turn_loaded() {
    let turns = [turn_with_class(1, DataClass::Public), turn_with_class(2, DataClass::Personal)];
    let (floor, source) =
        inherit_floor(DataClass::Public, ClassificationFloorSource::Default, &turns);

    assert_eq!(floor, DataClass::Personal);
    assert_eq!(source, ClassificationFloorSource::ConversationInherited);
    assert_eq!(source.as_snake_str(), "conversation_inherited");
}

#[test]
fn the_most_sensitive_turn_wins_regardless_of_its_position() {
    // Taking the first (or the last) rather than the max would silently plan a
    // follow-up below the class of the conversation it continues.
    let oldest_is_worst =
        [turn_with_class(1, DataClass::Secret), turn_with_class(2, DataClass::Public)];
    let (floor, _) =
        inherit_floor(DataClass::Public, ClassificationFloorSource::Default, &oldest_is_worst);
    assert_eq!(floor, DataClass::Secret);

    let newest_is_worst =
        [turn_with_class(1, DataClass::Public), turn_with_class(2, DataClass::Secret)];
    let (floor, _) =
        inherit_floor(DataClass::Public, ClassificationFloorSource::Default, &newest_is_worst);
    assert_eq!(floor, DataClass::Secret);
}

#[test]
fn a_floor_already_higher_is_left_alone_with_its_own_provenance() {
    let turns = [turn_with_class(1, DataClass::Personal)];
    let (floor, source) = inherit_floor(
        DataClass::ClinicalConfidential,
        ClassificationFloorSource::Operator,
        &turns,
    );

    assert_eq!(floor, DataClass::ClinicalConfidential);
    assert_eq!(
        source,
        ClassificationFloorSource::Operator,
        "inheritance never lowers a floor, and never relabels one it did not set",
    );
}

#[test]
fn an_equal_floor_keeps_its_own_provenance() {
    // Relabelling an equal floor would attribute an operator's decision to the
    // conversation, which is a false provenance in the audit row.
    let turns = [turn_with_class(1, DataClass::Personal)];
    let (floor, source) =
        inherit_floor(DataClass::Personal, ClassificationFloorSource::Operator, &turns);

    assert_eq!(floor, DataClass::Personal);
    assert_eq!(source, ClassificationFloorSource::Operator);
}

#[test]
fn a_turn_with_no_record_contributes_nothing() {
    let mut t = turn_with_class(1, DataClass::Secret);
    t.record = None;
    let (floor, source) = inherit_floor(DataClass::Public, ClassificationFloorSource::Default, &[t]);

    assert_eq!(floor, DataClass::Public);
    assert_eq!(source, ClassificationFloorSource::Default);
}

#[test]
fn no_turns_means_no_change() {
    let (floor, source) = inherit_floor(DataClass::Public, ClassificationFloorSource::Default, &[]);
    assert_eq!(floor, DataClass::Public);
    assert_eq!(source, ClassificationFloorSource::Default);
}
