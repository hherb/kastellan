//! The sift loop's cases added for the DGX measures (tasks 301–307, 322), run by
//! [`super::the_sift_loop`] on its cluster.

use serde_json::{json, Value};

use super::*;

/// DGX tasks 302 and 307: a find-all took a chain of sifts, each `from` the
/// last one's `next_from`, and the answer tabled only the last. A sift that
/// goes on says `continues`, and a table naming the last shows them all,
/// with the counts of the whole chain.
pub(super) async fn a_table_naming_the_last_of_a_chain_shows_every_link(pool: &sqlx::PgPool) {
    let ctx = new_task(pool, 5).await;
    let sift1 = |from: usize| {
        let mut c = sift(1, None);
        c["parameters"]["from"] = json!(from);
        c
    };
    let table = json!({"scrapbook": format!("sb:{}:3", ctx.task_id), "order_by": {"column": "amount", "desc": true}});
    let mut script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": search(1, hits(&["m1", "m2", "m3"]))}),
        json!({"call": sift1(0)}),
        json!({"call": sift1(2)}),
        json!({"tick": tick(1, "sorted"), "answer": {"body": "Your invoices:", "table": table}}),
    ])
    // The last link looks at 1 and keeps 0, so swapped counts would show.
    .sorting(&[keep(10), keep(20), no()]);
    script.sift_cfg.max_reads = 2;
    let r = run(pool, &script, ctx).await;
    let second = summary_seen(&script, 3);
    assert_eq!(second["continues"], json!(format!("sb:{}:2", r.task_id)), "{second}");
    assert_eq!(second["chain"], json!({"sifts": 2, "looked_at": 3, "kept": 2, "distinct": 2}), "the planner sees the chain's counts: {second}");
    let first = summary_seen(&script, 2);
    assert!(first.get("continues").is_none() && first.get("chain").is_none(), "the first sift is no chain: {first}");
    let b = body(&r.result);
    let (hi, lo) = (b.find("| 20 | m2 |").expect(&b), b.find("| 10 | m1 |").expect(&b));
    assert!(hi < lo, "both links' rows, ordered: {b}");
    assert!(b.ends_with("2 kept of 3 looked at."), "the chain's counts, nothing left over: {b}");
}

/// DGX task 322: a sift over a 27-row scrapbook `from` 64 (a search's
/// `next_from`) ran as an empty success. A `from` past the end is refused,
/// naming the list's length, before anything is sorted.
pub(super) async fn a_sift_from_past_the_end_is_refused_before_sorting(pool: &sqlx::PgPool) {
    let mut late = sift(2, None);
    late["parameters"]["from"] = json!(5);
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": search(1, hits(&["m1", "m2"]))}),
        json!({"tick": tick(1, "listed"), "call": late}),
        json!({"infeasible": {"reason": "nothing left"}}),
    ]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert!(kinds(pool, r.task_id).await.contains(&EntryKind::FailedStep));
    let v: Value = serde_json::from_str(&script.messages()[2]).unwrap();
    let blocks = v["blocks"].to_string();
    assert!(blocks.contains("sift_refused") && blocks.contains("(2 items)"), "{blocks}");
    assert!(script.sorted.lock().unwrap().is_empty(), "nothing was sorted");
}

/// #960, found in log #8 after it was filed (DGX tasks 316, 321): the
/// body quoted `kept` above a table that showed duplicates once. One row
/// kept in both links of a chain is one table row, and the chain's
/// `distinct` says so; each link's own too.
pub(super) async fn a_row_kept_in_two_links_counts_once_in_distinct(pool: &sqlx::PgPool) {
    let sift1 = |from: usize| {
        let mut c = sift(1, None);
        c["parameters"]["from"] = json!(from);
        c
    };
    // One item per link: 10, 10 again, then 20. The chain shows 2 rows —
    // not 3 (each link's `distinct` summed) nor 1 (this link's rows left out).
    let mut script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": search(1, hits(&["m1", "m2", "m3"]))}),
        json!({"call": sift1(0)}),
        json!({"call": sift1(1)}),
        json!({"call": sift1(2)}),
        json!({"tick": tick(1, "sorted"), "infeasible": {"reason": "counted"}}),
    ])
    .sorting(&[keep(10), keep(10), keep(20)]);
    script.sift_cfg.max_reads = 1;
    run(pool, &script, new_task(pool, 5).await).await;
    let (second, third) = (summary_seen(&script, 3), summary_seen(&script, 4));
    assert_eq!(second["chain"], json!({"sifts": 2, "looked_at": 2, "kept": 2, "distinct": 1}), "{second}");
    assert_eq!((third["kept"].clone(), third["distinct"].clone()), (json!(1), json!(1)), "the link's own: {third}");
    assert_eq!(third["chain"], json!({"sifts": 3, "looked_at": 3, "kept": 3, "distinct": 2}), "{third}");
}

/// #960 item 2 (DGX task 314): an answer that names no table after a sift
/// that kept rows is shown back once, naming the scrapbook. The re-asked
/// reply is used as it is: with the table, or the same answer again.
pub(super) async fn an_answer_without_the_table_is_shown_back_once(pool: &sqlx::PgPool) {
    // The re-ask's reply: with the table, the same answer again, or none
    // usable (out of script) — which keeps the first answer, not fails the
    // task (review finding), and only that failed re-ask is a failed call.
    for again in ["table", "same", "unusable"] {
        let ctx = new_task(pool, 5).await;
        let name = format!("sb:{}:2", ctx.task_id);
        let mut turns = vec![
            json!({"steps": steps(&[1, 2]), "call": search(1, hits(&["m1", "m2"]))}),
            json!({"tick": tick(1, "listed"), "call": sift(2, None)}),
            json!({"tick": tick(2, "sorted"), "answer": {"body": "Two invoices."}}),
        ];
        match again {
            "table" => turns.push(json!({"tick": tick(2, "sorted"), "answer": {"body": "Two invoices:", "table": {"scrapbook": name}}})),
            "same" => turns.push(json!({"tick": tick(2, "sorted"), "answer": {"body": "Two invoices."}})),
            _ => {}
        }
        let script = Script::new(&turns).sorting(&[keep(10), keep(20)]);
        let r = run(pool, &script, ctx).await;
        let reasks = script.reasks.lock().unwrap().clone();
        assert_eq!(reasks.len(), 1, "{reasks:?}");
        assert!(reasks[0].contains(&name) && reasks[0].contains("kept 2"), "{reasks:?}");
        let b = body(&r.result);
        match again {
            "table" => assert!(b.starts_with("Two invoices:") && b.contains("| 20 | m2 |"), "{b}"),
            _ => assert_eq!(b, "Two invoices.", "{again}: the answer is used"),
        }
        let failed = u32::from(again == "unusable");
        assert_eq!(r.result.failed_llm_calls, failed, "{again}");
    }
}

/// #960 item 1 (DGX tasks 308, 309, 316): a table named with no
/// `order_by` came out in the order the items were found. With a `date`
/// field it is newest first — and an `order_by` named still wins (review
/// finding).
pub(super) async fn a_table_with_no_order_named_is_newest_first(pool: &sqlx::PgPool) {
    for named in [false, true] {
        let ctx = new_task(pool, 5).await;
        let mut call = sift(2, None);
        call["parameters"]["fields"] = json!(["date"]);
        let dated = |d: &str| json!({"keep": true, "fields": {"date": d}});
        let mut table = json!({"scrapbook": format!("sb:{}:2", ctx.task_id)});
        if named {
            table["order_by"] = json!({"column": "date", "desc": false});
        }
        let script = Script::new(&[
            json!({"steps": steps(&[1, 2]), "call": search(1, hits(&["m1", "m2", "m3"]))}),
            json!({"tick": tick(1, "listed"), "call": call}),
            json!({"tick": tick(2, "sorted"), "answer": {"body": "Dated:", "table": table}}),
        ])
        .sorting(&[dated("2026-01-05"), dated("2026-03-01"), dated("2025-12-24")]);
        let b = body(&run(pool, &script, ctx).await.result);
        let at = |d: &str| b.find(d).expect(&b);
        let (new, mid, old) = (at("2026-03-01"), at("2026-01-05"), at("2025-12-24"));
        match named {
            false => assert!(new < mid && mid < old, "newest first: {b}"),
            true => assert!(old < mid && mid < new, "the order named: {b}"),
        }
    }
}

/// #960 item 3 (DGX task 315): 37 of 50 booking mails were `unread` — the
/// read named an attachment and they had none. With a list of reads each
/// item takes the first that applies, all in the one reviewed plan.
pub(super) async fn an_item_with_no_attachment_takes_the_next_read(pool: &sqlx::PgPool) {
    let hits = json!([{"id": "m1", "subject": "about m1", "files": ["a.pdf"]}, {"id": "m2", "subject": "about m2", "files": []}]);
    let mut call = sift(2, None);
    call["parameters"]["read"] = json!([
        {"tool": "mail", "method": "mail.read", "parameters": {"id": "{id}", "file": "{files.*}"}},
        {"tool": "mail", "method": "mail.get_message", "parameters": {"id": "{id}"}},
    ]);
    let mut script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": search(1, hits)}),
        json!({"tick": tick(1, "listed"), "call": call}),
        json!({"tick": tick(2, "sorted"), "infeasible": {"reason": "none kept"}}),
    ])
    .sorting(&[no(), no()]);
    script.sift_cfg.read_methods.push("mail.get_message".into());
    let r = run(pool, &script, new_task(pool, 5).await).await;
    assert_eq!(r.review.plans.lock().unwrap()[1], ["mail.read", "mail.get_message"], "one reviewed plan");
    assert_eq!(*r.tools.dispatched.lock().unwrap(), ["mail.search", "mail.read", "mail.get_message"]);
    let seen = summary_seen(&script, 2);
    assert_eq!((seen["reads"].clone(), seen["unread"].clone()), (json!(2), json!(0)), "{seen}");
    assert_eq!(seen["items_per_read"], json!([1, 1]), "a fall-through is visible: {seen}");
    assert!(script.sorted.lock().unwrap()[1].contains("text of m2"), "m2 was read with the message read");
}
