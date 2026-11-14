use super::*;

/// #960, found in log #8 after it was filed (DGX tasks 316, 321): the
/// body quoted `kept` (45, 8) above a table of 30 and 6 rows, because the
/// table shows rows alike in every field once. `distinct_kept` counts the table's rows by the same rule.
#[test]
fn distinct_kept_counts_rows_as_the_table_merges_them() {
    let a = json!({"vendor": "A", "amount": "10"});
    let b = json!({"vendor": "B", "amount": "10"});
    let empty = json!({"vendor": null, "amount": ""});
    let kept = |f: &[&serde_json::Value]| -> Vec<Row> { f.iter().enumerate().map(|(i, f)| row(i as i32 + 1, RowKind::Kept, (*f).clone())).collect() };
    assert_eq!(distinct_kept(&kept(&[&a, &a, &b, &a])), 2);
    assert_eq!(distinct_kept(&kept(&[&empty, &empty])), 2, "fields with nothing in them never merge");
    assert_eq!(distinct_kept(&[]), 0);
    let withheld = [row(1, RowKind::Withheld, json!({})), row(2, RowKind::Kept, a.clone())];
    assert_eq!(distinct_kept(&withheld), 1, "only kept rows count");
    let rows = [
        row(1, RowKind::Kept, a.clone()),
        row(2, RowKind::Kept, a.clone()),
        row(3, RowKind::Kept, b.clone()),
        row(4, RowKind::Kept, empty.clone()),
        row(5, RowKind::Kept, empty.clone()),
    ];
    let t = render(&rows, &cols(&["vendor", "amount"]), None, &counts(5, 5));
    assert_eq!(t.lines().filter(|l| l.starts_with("| ") && !l.contains("---")).count() - 1, 4, "the table agrees: {t}");
}

/// #1022 part 1: one message in two links of a paging chain (overlapping
/// pages), its fields extracted differently in each, is one table row — so
/// the chain's `distinct` is 1, not 2.
#[test]
fn distinct_kept_merges_one_source_across_sifts_as_the_table_does() {
    let rows = [
        Row { sift: 1, ..sourced(1, "m1", json!({"amount": "$10"})) },
        Row { sift: 2, ..sourced(1, "m1", json!({"amount": "10"})) },
        Row { sift: 2, ..sourced(2, "m2", json!({"amount": "7"})) },
    ];
    assert_eq!(distinct_kept(&rows), 2);
    let t = render(&rows, &cols(&["amount"]), None, &counts(3, 3));
    assert_eq!(t.lines().skip(2).take_while(|l| l.starts_with('|')).count(), 2, "the table agrees: {t}");

    // A sift's rows in hand (`chain_distinct`'s own) join as stored ones do.
    let own = |source: &str, amount: &str| kastellan_db::scrapbook::NewRow {
        kind: RowKind::Kept,
        source: json!({"message_id": source}),
        fields: json!({"amount": amount}),
        classification: "Personal".into(),
    };
    let mut chain = vec![rows[0].clone()];
    chain.extend(as_stored(1, 2, &[own("m1", "10"), own("m2", "7")]));
    assert_eq!(distinct_kept(&chain), 2);
    assert_eq!((chain[1].task_id, chain[1].sift, chain[1].row, chain[2].row), (1, 2, 1, 2));
}

/// Issue 1029: `alike` is greedy, so grouping rows after sorting them could
/// group a chain's rows otherwise than `distinct_kept` does in stored order.
/// One booking emailed twice (m1, m2) in sift 1, then m2 again in sift 2's
/// overlapping page with a newer date: stored order makes one group of all
/// three; sorted newest first, m2's newer row went first and m1 was left
/// alone, so the table showed 2 rows under `distinct: 1`. The table groups in
/// stored order too, then orders the groups by the cell each one shows.
#[test]
fn a_sorted_table_groups_rows_as_distinct_kept_counts_them() {
    let booking = |d: &str| json!({"date": d, "to": "X"});
    let rows = [
        sourced(1, "m1", booking("2026-11-01")),
        sourced(2, "m2", booking("2026-11-01")),
        Row { sift: 2, ..sourced(1, "m2", booking("2026-11-05")) },
        sourced(3, "m3", json!({"date": "2026-11-03", "to": "Y"})),
    ];
    assert_eq!(distinct_kept(&rows), 2);
    let order = OrderBy { column: "date".into(), desc: true };
    let t = render(&rows, &cols(&["date", "to", "source"]), Some(&order), &counts(4, 4));
    let lines: Vec<&str> = t.lines().skip(2).take_while(|l| l.starts_with('|')).collect();
    // A group sorts by the date it shows (its first row's), so m3's row goes first.
    assert_eq!(lines, ["| 2026-11-03 | Y | m3 |", "| 2026-11-01 | X | m1, m2 |"], "{t}");
    let up = OrderBy { column: "date".into(), desc: false };
    let t = render(&rows, &cols(&["date", "to", "source"]), Some(&up), &counts(4, 4));
    let lines: Vec<&str> = t.lines().skip(2).take_while(|l| l.starts_with('|')).collect();
    assert_eq!(lines, ["| 2026-11-01 | X | m1, m2 |", "| 2026-11-03 | Y | m3 |"], "{t}");
}

/// PR #1030 review: a group sorts by the cell it shows, its first non-empty
/// one. Merged on source across sifts, a group's first row can lack the
/// order column (the earlier sift named the field otherwise); sorting by that
/// row's empty cell would put the newest booking last.
#[test]
fn a_group_sorts_by_its_first_non_empty_cell() {
    let rows = [
        sourced(1, "m1", json!({"to": "X"})),
        sourced(2, "m2", json!({"date": "2026-11-01", "to": "Y"})),
        Row { sift: 2, ..sourced(1, "m1", json!({"date": "2026-12-01", "to": "X"})) },
    ];
    let order = OrderBy { column: "date".into(), desc: true };
    let table = |limit| {
        let t = render_table(&rows, &cols(&["date", "to"]), Some(&order), limit, &counts(3, 3)).0;
        t.lines().skip(2).take_while(|l| l.starts_with('|')).map(String::from).collect::<Vec<_>>()
    };
    assert_eq!(table(None), ["| 2026-12-01 | X |", "| 2026-11-01 | Y |"]);
    assert_eq!(table(Some(1)), ["| 2026-12-01 | X |"]);
}

/// PR #1030 review: groups tied on the order column keep their stored order
/// either way, and the note counts the duplicates of the groups shown after
/// the sort, not of the first ones stored.
#[test]
fn tied_groups_keep_stored_order_and_the_note_counts_the_shown_ones() {
    let rows = [
        sourced(1, "m1", json!({"d": "2026-01-01", "v": "a"})),
        sourced(2, "m2", json!({"d": "2026-01-01", "v": "a"})),
        sourced(3, "m3", json!({"d": "2026-02-01", "v": "b"})),
        sourced(4, "m4", json!({"d": "2026-01-01", "v": "c"})),
    ];
    let render_by = |desc, limit| {
        let order = OrderBy { column: "d".into(), desc };
        render_table(&rows, &cols(&["d", "v"]), Some(&order), limit, &counts(4, 4)).0
    };
    let lines = |t: &str| t.lines().skip(2).take_while(|l| l.starts_with('|')).map(String::from).collect::<Vec<_>>();
    let down = render_by(true, None);
    assert_eq!(lines(&down), ["| 2026-02-01 | b |", "| 2026-01-01 | a |", "| 2026-01-01 | c |"], "{down}");
    let up = render_by(false, None);
    assert_eq!(lines(&up), ["| 2026-01-01 | a |", "| 2026-01-01 | c |", "| 2026-02-01 | b |"], "{up}");
    let one = render_by(true, Some(1));
    assert!(one.ends_with("4 kept of 4 looked at; showing the first 1 of 3 rows, by `d` descending."), "{one}");
}
