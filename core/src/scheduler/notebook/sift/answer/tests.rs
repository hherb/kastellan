use super::*;

/// Task 9, whose conversation is task 5, with `plans` as this run's.
fn ctx(plans: Vec<PlanRecord>) -> TaskContext {
    TaskContext {
        task_id: 9,
        lane: kastellan_db::tasks::Lane::Fast,
        instruction: String::new(),
        classification_floor: DataClass::Public,
        classification_floor_source: crate::scheduler::inner_loop::ClassificationFloorSource::Default,
        classification_floor_signals: vec![],
        plans,
        advisories: vec![],
        blocks: vec![],
        plan_count: 0,
        max_plans: 5,
        deadline: None,
        resolved_asks: Vec::new(),
        origin: None,
        conversation: Vec::new(),
        conversation_task_ids: Some(vec![5]),
    }
}

/// Review finding: only an included continuer takes over a link's
/// `remaining`; a chain cut short (the cap, an unreadable link) says
/// what was not looked at.
#[test]
fn remaining_counts_only_the_links_nothing_included_continues() {
    let link = |name: &str, remaining, continues: Option<&str>| Link {
        name: name.into(),
        remaining,
        continues: continues.map(String::from),
    };
    let whole = [link("a", 70, None), link("b", 40, Some("a")), link("c", 0, Some("b"))];
    assert_eq!(remaining(&whole), 0);
    assert_eq!(remaining(&whole[..2]), 40, "c left out: b's remaining stands");
    assert_eq!(remaining(&[link("a", 70, None), link("x", 5, None)]), 75, "two separate sifts");
    assert_eq!(remaining(&[]), 0);
}

/// Review finding: the cap is on the refs named, so the last of a chain
/// of nine shows all nine; a ref past it is the one refused, and a ref
/// written `sb:9:04` finds its chain.
#[test]
fn the_cap_is_on_refs_named_and_a_chain_brings_every_link() {
    use super::super::chain::tests::{params, sift_record};
    let plans: Vec<PlanRecord> = (1..=9)
        .map(|n| {
            let continues = (n > 1).then(|| format!("sb:9:{}", n - 1));
            sift_record(params(30 * (n - 1), "x"), serde_json::json!({"scrapbook": format!("sb:9:{n}"), "continues": continues}))
        })
        .collect();
    let all: Vec<String> = (1..=9).map(|n| format!("sb:9:{n}")).collect();
    assert_eq!(table_refs(&plans, &["sb:9:9".into()]), (all.clone(), vec![]));
    assert_eq!(table_refs(&plans, &["sb:9:09".into(), "sb:9:9".into()]).0, all, "canonical, and once");
    let named: Vec<String> = (1..=9).map(|n| format!("sb:7:{n}")).collect();
    let (refs, refused) = table_refs(&plans, &named);
    assert_eq!(refs, named[..TABLE_REFS_MAX]);
    assert_eq!(refused, ["(table sb:7:9 could not be included: a table names at most 8)"]);
    assert_eq!(table_refs(&plans, &["bad".into()]).0, ["bad"], "kept as written for its refusal");
}

/// #960 item 2: an answer with no table after a sift that kept rows is
/// refused once, naming the scrapbook; a chain's `kept` counts, the
/// newest sift decides, and a table, a refusal or nothing kept passes.
#[test]
fn an_answer_without_the_table_of_a_sift_that_kept_rows_is_refused() {
    use super::super::chain::tests::{params, sift_record};
    let answer = |extra: serde_json::Value| {
        let mut a = serde_json::json!({"body": "21 payments"});
        a.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
        serde_json::from_value::<NotebookReply>(serde_json::json!({"answer": a})).unwrap()
    };
    let plain = answer(serde_json::json!({}));
    let kept = |n: u64| sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:9:3", "kept": n}));
    let why = table_left_out(&plain, &ctx(vec![kept(21)])).unwrap_err();
    assert!(why.contains("sb:9:3") && why.contains("kept 21") && why.contains("\"table\""), "{why}");
    assert_eq!(table_left_out(&plain, &ctx(vec![kept(0)])), Ok(()), "nothing kept: no table to show");
    assert_eq!(table_left_out(&plain, &ctx(vec![])), Ok(()), "no sift");
    assert_eq!(table_left_out(&plain, &ctx(vec![kept(21), kept(0)])), Ok(()), "the newest sift decides");
    let link = sift_record(params(2, "x"), serde_json::json!({"scrapbook": "sb:9:4", "kept": 0, "chain": {"kept": 5}}));
    assert!(table_left_out(&plain, &ctx(vec![link])).unwrap_err().contains("kept 5"), "a chain's kept");
    let link = sift_record(params(2, "x"), serde_json::json!({"scrapbook": "sb:9:4", "kept": 4, "distinct": 4, "chain": {"kept": 5, "distinct": 3}}));
    assert!(table_left_out(&plain, &ctx(vec![link])).unwrap_err().contains("kept 3"), "the chain's distinct: the table's rows");
    let own = sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:9:3", "kept": 8, "distinct": 6}));
    assert!(table_left_out(&plain, &ctx(vec![own])).unwrap_err().contains("kept 6"), "its own distinct");
    assert_eq!(table_left_out(&answer(serde_json::json!({"table": {"scrapbook": "sb:9:3"}})), &ctx(vec![kept(21)])), Ok(()));
    assert_eq!(table_left_out(&answer(serde_json::json!({"refused": {"principle": 1, "reason": "harm"}})), &ctx(vec![kept(21)])), Ok(()), "a refusal");
    let call: NotebookReply = serde_json::from_value(serde_json::json!({"infeasible": {"reason": "no"}})).unwrap();
    assert_eq!(table_left_out(&call, &ctx(vec![kept(21)])), Ok(()), "not an answer");
    // The forced-answer turn: the same rule, the table attached by code.
    let spec = forced_table(&plain, &[kept(21)]).expect("attached");
    assert_eq!((spec.scrapbook.refs(), spec.columns, spec.order_by, spec.limit), (&["sb:9:3".to_string()][..], None, None, None));
    assert_eq!(forced_table(&plain, &[kept(0)]), None, "nothing kept");
    assert_eq!(forced_table(&answer(serde_json::json!({"table": {"scrapbook": "sb:9:1"}})), &[kept(21)]), None, "its own table");
    assert_eq!(forced_table(&answer(serde_json::json!({"refused": {"principle": 1, "reason": "harm"}})), &[kept(21)]), None, "a refusal");
}

/// #973 (DGX task 359): no sift ran, and the answer's table named
/// `sb:emails_with_attachments`, so the operator got one refusal line and
/// no list. A table none of whose refs can be shown (malformed; #981: or
/// this task's but no sift of this run, or outside the conversation) is
/// shown back once, naming the newest sift when it kept rows. A refusal is not.
#[test]
fn an_answer_whose_table_names_no_scrapbook_ref_is_refused() {
    use super::super::chain::tests::{params, sift_record};
    let tabled = |refs: serde_json::Value| {
        serde_json::from_value::<NotebookReply>(serde_json::json!({"answer": {"body": "Here they are:", "table": {"scrapbook": refs}}})).unwrap()
    };
    let made_up = tabled(serde_json::json!("sb:emails_with_attachments"));
    let why = table_left_out(&made_up, &ctx(vec![])).unwrap_err();
    assert!(why.contains("sb:emails_with_attachments") && why.contains("`body`") && !why.contains("to show them"), "{why}");
    let kept = || sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:9:3", "kept": 4}));
    let why = table_left_out(&made_up, &ctx(vec![kept()])).unwrap_err();
    assert!(why.contains("sb:9:3") && why.contains("kept 4"), "names the sift that kept rows: {why}");
    assert!(table_left_out(&tabled(serde_json::json!(["bad", "worse"])), &ctx(vec![])).is_err(), "every ref malformed");
    assert_eq!(table_left_out(&tabled(serde_json::json!(["bad", "sb:9:3"])), &ctx(vec![kept()])), Ok(()), "one ref shows rows");
    // #981: a well-formed ref to no sift of this run, or to a task outside
    // the conversation, shows nothing either; an earlier turn's is with_table's.
    let why = table_left_out(&tabled(serde_json::json!("sb:9:7")), &ctx(vec![kept()])).unwrap_err();
    assert!(why.contains("sb:9:7") && why.contains("sb:9:3") && why.contains("kept 4"), "{why}");
    // Also a resumed run whose history would not decode (restored empty): a
    // real own ref costs one re-ask, the accepted price of a pure check.
    assert!(table_left_out(&tabled(serde_json::json!("sb:9:3")), &ctx(vec![])).is_err(), "its own task, no sift ran");
    assert!(table_left_out(&tabled(serde_json::json!(["bad", "sb:9:7"])), &ctx(vec![])).is_err(), "one malformed, one to nowhere");
    assert!(table_left_out(&tabled(serde_json::json!("sb:6:2")), &ctx(vec![])).is_err(), "not from this conversation");
    assert_eq!(table_left_out(&tabled(serde_json::json!("sb:5:2")), &ctx(vec![])), Ok(()), "an earlier turn's: with_table's to judge");
    assert_eq!(table_left_out(&tabled(serde_json::json!(["sb:9:7", "sb:9:03"])), &ctx(vec![kept()])), Ok(()), "one names a sift that ran, written loosely");
    assert_eq!(table_left_out(&tabled(serde_json::json!([])), &ctx(vec![])), Ok(()), "no refs and no sift: the body alone");
    // A refusal that names a made-up table is never told to list items instead.
    let refusal = serde_json::from_value::<NotebookReply>(serde_json::json!({"answer": {
        "body": "No.", "refused": {"principle": 1, "reason": "harm"}, "table": {"scrapbook": "sb:x"}}}))
    .unwrap();
    assert_eq!(table_left_out(&refusal, &ctx(vec![])), Ok(()), "a refusal");
    // The forced turn is never shown back; code attaches no table to an answer that names one.
    assert_eq!(forced_table(&made_up, &[kept()]), None, "its own table, though a sift kept rows");
    // `[]` names no table: after a sift that kept rows it is #960's case on both turns.
    let empty = tabled(serde_json::json!([]));
    assert!(table_left_out(&empty, &ctx(vec![kept()])).unwrap_err().contains("shows no `table`"), "[] after a sift that kept rows");
    assert_eq!(forced_table(&empty, &[kept()]).map(|t| t.scrapbook.refs().to_vec()), Some(vec!["sb:9:3".to_string()]), "attached by code");
}

#[test]
fn a_body_s_own_markdown_table_is_taken_out() {
    let body = "Six of them:\n\n| date | amount |\n|---|---:|\n| 1 May | 20 |\n\nSee below.";
    assert_eq!(without_own_tables(body), Some("Six of them:\n\nSee below.".into()), "one blank line where the table was (#970)");
    let indented = "a\n  | x |\n  | :-- |\n  | 1 |";
    assert_eq!(without_own_tables(indented), Some("a".into()), "indented, and at the end");
    assert_eq!(without_own_tables("| a |\n|---|\n| 1 |\nAfter."), Some("After.".into()), "prose right after the table stays");
    assert_eq!(without_own_tables("| a |\n|---|\n\nmid\n\n| b |\n|---|\nend"), Some("mid\n\nend".into()), "every table, not only the first");
    // #970: a table inside a fenced code block is the model's example, not a copy of the rows.
    let fenced = "Like this:\n\n```\n| a |\n|---|\n```\n\n| b |\n|---|\n| 1 |";
    assert_eq!(without_own_tables(fenced), Some("Like this:\n\n```\n| a |\n|---|\n```".into()), "only the table outside the fence");
    assert_eq!(without_own_tables("~~~md\n| a |\n|---|\n~~~"), None, "a tilde fence too");
    assert_eq!(without_own_tables("a | b\n| not | a table |\nc"), None, "no delimiter row");
    assert_eq!(without_own_tables("| x |\n| y |"), None, "a second row is not a delimiter");
    assert_eq!(without_own_tables("|---|"), None, "a delimiter alone");
    assert_eq!(without_own_tables("plain prose"), None);
    // The blank lines after a cut table close up only when one is before it.
    assert_eq!(without_own_tables("a\n| x |\n|---|\n\nb"), Some("a\n\nb".into()), "no blank before: the one after stays");
    assert_eq!(without_own_tables("x\n\n| t |\n|---|\n\n\ny"), Some("x\n\ny".into()), "every blank after, not only one");
    // A line opening with an inline code span is no fence.
    assert_eq!(without_own_tables("```sb:9:3``` is the list\n\n| a |\n|---|"), Some("```sb:9:3``` is the list".into()), "inline span");
    assert_eq!(without_own_tables("````\n| a |\n|---|\n````"), None, "a longer fence");
}

#[test]
fn a_row_class_that_does_not_parse_counts_as_secret() {
    let row = |class: &str| Row {
        task_id: 1,
        sift: 1,
        row: 1,
        kind: RowKind::Kept,
        source: serde_json::json!({}),
        fields: serde_json::json!({}),
        classification: class.into(),
    };
    assert_eq!(row_class(&row("Personal")), DataClass::Personal);
    assert_eq!(row_class(&row("personal-ish")), DataClass::Secret);
    assert_eq!(row_class(&row("")), DataClass::Secret);
}
