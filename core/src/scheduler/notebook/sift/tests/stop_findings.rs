//! The sift loop's cases for how a sift ends or what it went over, when that
//! was silent (#956), run by [`super::the_sift_loop`] on its cluster.

use serde_json::json;

use super::*;

/// #956: a database error mid-sift ended the task with `sift.started`
/// standing alone, and the rows kept so far went unrecorded. A row already
/// at this sift's number makes the scrapbook write fail: the task still
/// fails, and `sift.finished` says why and how many rows were lost.
pub(super) async fn a_db_error_mid_sift_writes_its_finished_row(pool: &sqlx::PgPool) {
    let ctx = new_task(pool, 5).await;
    let task = ctx.task_id;
    let squatter = scrapbook::NewRow {
        kind: RowKind::Withheld,
        source: json!({"id": "x"}),
        fields: json!({}),
        classification: "Personal".into(),
    };
    // The sift runs on turn 2, so it writes sift 2.
    scrapbook::append_rows(pool, task, 2, &[squatter]).await.unwrap();
    let script = Script::new(&three_turns(&["m1", "m2"], None)).sorting(&[keep(1), keep(2)]);
    let err = try_run(pool, &script, ctx).await.expect_err("the scrapbook write failed");
    assert!(err.to_string().starts_with("db: "), "{err}");
    let finished = finished_row(pool, task).await;
    assert_eq!(finished["stopped"], json!("error"), "{finished}");
    assert_eq!((finished["kept"].clone(), finished["rows_unwritten"].clone()), (json!(2), json!(2)), "{finished}");
    assert!(finished["error"].as_str().is_some_and(|e| e.starts_with("db: ")), "{finished}");
    assert_eq!(rows(pool, task, 2).await.len(), 1, "only the squatter: the sift's rows were not written");
}

/// #956: `over: {"step": 1}` is step 1's newest *success*. When a newer call
/// for step 1 failed, the sift went over the older list without a word; its
/// summary (and `sift.finished`) now names the failed call's code.
pub(super) async fn a_sift_over_a_step_whose_newest_call_failed_says_so(pool: &sqlx::PgPool) {
    let mut again = search(1, hits(&["m9"]));
    again["method"] = json!("fail");
    let answer = json!({"tick": tick(2, "sorted"), "infeasible": {"reason": "none kept"}});
    let script = Script::new(&[
        json!({"steps": steps(&[1, 2]), "call": search(1, hits(&["m1"]))}),
        json!({"call": again}),
        json!({"tick": tick(1, "listed"), "call": sift(2, None)}),
        answer,
    ])
    .sorting(&[no()]);
    let r = run(pool, &script, new_task(pool, 5).await).await;
    let seen = summary_seen(&script, 3);
    let note = seen["over_older"].as_str().unwrap_or_else(|| panic!("no `over_older`: {seen}"));
    assert!(note.contains("POLICY_DENIED") && note.contains("step 1"), "{note}");
    assert_eq!(seen["looked_at"], json!(1), "it went over m1, the older list: {seen}");
    assert_eq!(finished_row(pool, r.task_id).await["over_older"], json!(note));
}
