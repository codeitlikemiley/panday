//! M15.5 — the reduce-then-solve eval and its regression gate (docs/15 §retention
//! tests, docs/19 `reduce-bench`).
//!
//! The corpus is the same five recorded outputs the retention fixtures use, which
//! is deliberate: those tests assert a fact survived *the compressor*, and these
//! assert the task stayed solvable through *the whole loop* — assembly, spilling
//! and compaction included. A fact can survive the first and be lost in the
//! second, and only this arm would notice.

use panday_harness::eval::{production_reducer, recorded_corpus, run, run_corpus, Scenario};
use panday_reducer::Reducer;

/// The corpus and the shipping reducer now live in `panday_harness::eval` (M12.4), so
/// `cargo xtask scorecard` reports the same numbers this suite gates on — one definition, two
/// callers.
fn corpus() -> Vec<Scenario> {
    recorded_corpus(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/eval_corpus"))
}

#[tokio::test]
async fn the_shipping_reducer_keeps_every_task_solvable() {
    // This is the gate. A regression here blocks release (docs/15): "a reducer
    // that loses the plot is negative value at any compression ratio."
    let report = run_corpus(&corpus(), &production_reducer).await;
    println!("{}", report.scorecard());

    let regressions = report.regressions();
    assert!(
        regressions.is_empty(),
        "reduction made {} task(s) unsolvable:\n{}",
        regressions.len(),
        report.scorecard()
    );
    assert_eq!(report.success_rate(), 1.0);
}

#[tokio::test]
async fn every_scenario_is_solvable_without_reduction() {
    // Without this the gate above passes trivially: a scenario that fails both
    // ways is not a regression, so a broken corpus would look like a clean run.
    let report = run_corpus(&corpus(), &production_reducer).await;
    for row in &report.rows {
        assert!(
            row.without_reduction.solved,
            "{} is not solvable even with reduction off — the scenario is broken, \
             and a broken scenario silently exempts itself from the gate",
            row.scenario
        );
    }
}

#[tokio::test]
async fn the_eval_actually_detects_a_reducer_that_eats_the_facts() {
    // The test that makes the gate trustworthy. A harness that cannot fail is a
    // harness that says nothing — and this one's whole claim is that it notices
    // when compression destroys task success.
    struct Vandal;
    impl Reducer for Vandal {
        fn reduce(
            &self,
            raw: &str,
            _ctx: &panday_reducer::ReduceCtx,
        ) -> panday_types::event::ReducedOutput {
            // 95% "compression", zero retention: keep the first two lines. This
            // is the shape of an impressive ratio that is worth negative money.
            let kept: String = raw.lines().take(2).collect::<Vec<_>>().join("\n");
            panday_types::event::ReducedOutput {
                tokens_raw: panday_reducer::approx_tokens(raw),
                tokens_kept: panday_reducer::approx_tokens(&kept),
                text: kept,
                strategy: "vandal".into(),
            }
        }
    }

    let report = run_corpus(&corpus(), &|| Box::new(Vandal)).await;
    let regressions = report.regressions();
    // Exactly the three failure diagnoses break. The other two survive because
    // their fact happens to sit in the first line — which is the real lesson
    // about head-keeping strategies: they look fine on a file read and destroy
    // a test failure, because the interesting part of a diagnostic is at the end.
    assert_eq!(
        regressions.len(),
        3,
        "expected the three diagnoses to break:\n{}",
        report.scorecard()
    );
    assert!(
        regressions
            .iter()
            .all(|r| r.scenario.contains("failure") || r.scenario.contains("error")),
        "{}",
        report.scorecard()
    );
    // And it names what was lost, because "it failed" is not actionable.
    assert!(
        regressions.iter().any(|r| r
            .with_reduction
            .missing_facts
            .contains(&"error[E0308]".to_string())),
        "{}",
        report.scorecard()
    );
    // Its ratio looks excellent, which is the entire point of ADR-007.
    assert!(report.mean_ratio() > 0.8, "{}", report.scorecard());
}

#[tokio::test]
async fn the_scorecard_reports_dollars_shaped_facts_not_just_a_ratio() {
    let report = run_corpus(&corpus(), &production_reducer).await;
    let card = report.scorecard();
    assert!(card.contains("solved"), "{card}");
    assert!(card.contains("regression(s)"), "{card}");
    // Success comes before reduction in the summary line: the order is the
    // argument (ADR-007).
    let summary = card.lines().find(|l| l.contains("success")).unwrap();
    assert!(
        summary.find("success").unwrap() < summary.find("mean reduction").unwrap(),
        "{summary}"
    );
}

#[tokio::test]
async fn a_missing_fact_is_reported_by_name() {
    let scenario = Scenario::new(
        "synthetic",
        "bash",
        format!(
            "{}\nerror[E0999]: the one line that matters\n",
            "noise\n".repeat(200)
        ),
        &["error[E0999]"],
    );
    struct HeadOnly;
    impl Reducer for HeadOnly {
        fn reduce(
            &self,
            raw: &str,
            _ctx: &panday_reducer::ReduceCtx,
        ) -> panday_types::event::ReducedOutput {
            let kept: String = raw.lines().take(3).collect::<Vec<_>>().join("\n");
            panday_types::event::ReducedOutput {
                tokens_raw: panday_reducer::approx_tokens(raw),
                tokens_kept: panday_reducer::approx_tokens(&kept),
                text: kept,
                strategy: "head_only".into(),
            }
        }
    }

    let outcome = run(&scenario, Box::new(HeadOnly)).await;
    assert!(!outcome.solved);
    assert_eq!(outcome.missing_facts, vec!["error[E0999]".to_string()]);

    // The production reducer floats error lines into the kept region, so the
    // same scenario stays solvable — this is `error_context_lines` earning its
    // place rather than a claim in a comment.
    let outcome = run(&scenario, production_reducer()).await;
    assert!(outcome.solved, "missing: {:?}", outcome.missing_facts);
    assert!(
        outcome.ratio() > 0.5,
        "it should still compress: {outcome:?}"
    );
}
