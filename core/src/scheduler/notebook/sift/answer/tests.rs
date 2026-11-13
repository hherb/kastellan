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

/// #1015: looked-at and not-yet-looked-at items are counted once per
/// list. A chain's links add up; a chain cut short (the cap, an unreadable
/// link) leaves its tail unread; separate sifts over one list overlap; a sift
/// over another list, or an earlier turn's ref, counts on its own.
#[test]
fn coverage_counts_each_list_once() {
    let list = |step| Some((Over::Step(step), None));
    let link = |step, from, looked_at, remaining| Link { list: list(step), from, looked_at, remaining };
    let chain = [link(1, 0, 30, 50), link(1, 30, 30, 20), link(1, 60, 20, 0)];
    assert_eq!(coverage(&chain), (80, 0));
    assert_eq!(coverage(&chain[..2]), (60, 20), "the last link left out: its span is unread");
    assert_eq!(coverage(&[link(1, 0, 30, 50), link(1, 0, 30, 50)]), (30, 50), "two sifts over one list's first 30");
    assert_eq!(coverage(&[link(1, 0, 30, 50), link(1, 50, 10, 20)]), (40, 40), "a gap stays unread");
    assert_eq!(coverage(&[link(1, 0, 30, 50), link(2, 0, 5, 75)]), (35, 125), "two lists");
    let earlier = Link { list: None, from: 0, looked_at: 4, remaining: 0 };
    assert_eq!(coverage(&[link(1, 0, 30, 50), earlier]), (34, 50), "an earlier turn's ref: its own");
    assert_eq!(coverage(&[]), (0, 0));
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
    assert!(table_left_out(&plain, &ctx(vec![kept(21), kept(0)])).is_err(), "an older sift's rows still count (#1012)");
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

/// #1012 (DGX task 398): four separate sifts kept 12, 11, 30 and 0; the
/// last stopped on the ceiling, and the forced answer got no table because
/// only the newest sift was looked at. Kept rows accumulate over the run:
/// every sift that kept rows is named, newest first, and a chain once, by
/// its last link (which brings the rest).
#[test]
fn kept_rows_accumulate_over_the_run_s_sifts() {
    use super::super::chain::tests::{params, sift_record};
    let plain = serde_json::from_value::<NotebookReply>(serde_json::json!({"answer": {"body": "none"}})).unwrap();
    let sift = |n: u32, kept: u64| sift_record(params(0, "x"), serde_json::json!({"scrapbook": format!("sb:9:{n}"), "kept": kept}));
    let task_398 = vec![sift(2, 12), sift(4, 11), sift(6, 30), sift(8, 0)];
    let refs = |plans: &[PlanRecord]| forced_table(&plain, plans).map(|t| t.scrapbook.refs().to_vec());
    let names = |ns: &[&str]| Some(ns.iter().map(|n| n.to_string()).collect::<Vec<_>>());
    assert_eq!(refs(&task_398), names(&["sb:9:6", "sb:9:4", "sb:9:2"]), "every sift that kept rows, newest first");
    assert_eq!(refs(&[sift(2, 0), sift(4, 0)]), None, "none kept anything");
    // #1015: task 398's last sift kept nothing but stopped with items unread:
    // it joins, after the sifts with rows, for its `remaining`.
    let stopped = || sift_record(params(0, "y"), serde_json::json!({"scrapbook": "sb:9:9", "kept": 0, "remaining": 40}));
    let with_stop = vec![sift(2, 12), sift(4, 11), sift(6, 30), sift(8, 0), stopped()];
    assert_eq!(refs(&with_stop), names(&["sb:9:6", "sb:9:4", "sb:9:2", "sb:9:9"]));
    assert_eq!(refs(&[sift(2, 0), stopped()]), None, "nothing kept: still no table");
    let why = table_left_out(&plain, &ctx(task_398)).unwrap_err();
    assert!(why.contains("sift sb:9:6 kept 30 items") && why.contains("sift sb:9:2 kept 12 items"), "{why}");
    assert!(why.contains(r#"{"scrapbook": ["sb:9:6","sb:9:4","sb:9:2"]}"#), "the fix names them all: {why}");
    // A chain is named once, by its last link: the table expands it.
    let chain = vec![
        sift_record(params(0, "x"), serde_json::json!({"scrapbook": "sb:9:2", "kept": 5, "next_from": 30})),
        sift_record(params(30, "x"), serde_json::json!({"scrapbook": "sb:9:3", "kept": 0, "continues": "sb:9:2", "chain": {"kept": 5}})),
        sift(5, 2),
    ];
    assert_eq!(refs(&chain), names(&["sb:9:5", "sb:9:3"]), "the link sb:9:2 comes with sb:9:3");
    // #1016: the fix never names more refs than a table takes, newest first.
    let nine: Vec<PlanRecord> = (1..=9).map(|n| sift(n, 1)).collect();
    let why = table_left_out(&plain, &ctx(nine)).unwrap_err();
    assert!(why.contains("sb:9:9") && why.contains("sb:9:2") && !why.contains("sb:9:1"), "the oldest is not named: {why}");
    let one = table_left_out(&plain, &ctx(vec![sift(3, 4)])).unwrap_err();
    assert!(one.contains(r#"{"scrapbook": "sb:9:3"}"#), "one sift: a plain ref, as before: {one}");
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
