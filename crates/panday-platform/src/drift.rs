//! Ledger-drift monitoring against provider usage reports (docs/21 M21.4).
//!
//! Two different questions, and this module answers the second:
//!
//! - **Internal drift** — does the cached balance match the sum of the entries? `pg::balance_drift`
//!   and `pg::repair_balance` (M17.2). A bookkeeping question with a mechanical answer.
//! - **External drift** — does what we recorded as COGS match what the provider actually billed?
//!   That is this file, and it has no mechanical answer: a difference means either our metering is
//!   wrong or the invoice is, and finding out which is a person's job. What the monitor owes them
//!   is a number, a direction, and enough breakdown to start.
//!
//! **Direction is carried separately from magnitude**, because the two failures are different
//! incidents. Recording *more* than the provider billed means we may have over-charged a customer:
//! a refund and an apology. Recording *less* means we are eating the difference: a margin problem.
//! Same absolute number, opposite response, and an alert that could not tell them apart would page
//! the wrong person.
//!
//! **A missing model is louder than a wrong number.** A model that appears on the invoice and not
//! in our ledger is traffic we did not meter at all — reported as its own class, because a
//! percentage comparison would quietly average it away against everything that did match.

use crate::pg::PgError;
use sqlx::PgPool;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum DriftError {
    #[error(transparent)]
    Db(#[from] PgError),
    #[error("usage report: {0}")]
    Report(String),
}

/// One line of a provider's usage report, normalised.
///
/// Providers each publish a different CSV; the adapter for each one is a mapping to this, so the
/// comparison below never learns a vendor's column names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderLine {
    pub model: String,
    /// What they charged, in micro-dollars. Positive.
    pub cost_micros: i64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// Parse a normalised usage-report CSV: `model,cost_usd,input_tokens,output_tokens`.
///
/// Hand-parsed, because the format is four columns and no quoting — and because the alternative is
/// a CSV dependency to read a file an operator exports once a month. A provider's own export is
/// converted to this shape by whoever exports it; that conversion is deliberately outside the
/// monitor, so a vendor changing their column names is a one-line change in a shell script rather
/// than a release of this binary.
pub fn parse_report(csv: &str) -> Result<Vec<ProviderLine>, DriftError> {
    let mut out = Vec::new();
    for (i, line) in csv.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // A header is recognised rather than assumed: an operator who exports without one should
        // not silently lose their first row.
        if i == 0 && line.to_lowercase().starts_with("model,") {
            continue;
        }

        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        if fields.len() < 4 {
            return Err(DriftError::Report(format!(
                "line {}: expected model,cost_usd,input_tokens,output_tokens",
                i + 1
            )));
        }
        let dollars: f64 = fields[1].trim_start_matches('$').parse().map_err(|_| {
            DriftError::Report(format!("line {}: `{}` is not a cost", i + 1, fields[1]))
        })?;

        out.push(ProviderLine {
            model: fields[0].to_string(),
            // Rounded once, here, at the edge. Everything downstream is integers (ADR-009).
            cost_micros: (dollars * 1_000_000.0).round() as i64,
            input_tokens: fields[2].parse().unwrap_or(0),
            output_tokens: fields[3].parse().unwrap_or(0),
        });
    }
    Ok(out)
}

/// What we recorded as provider cost, per model, over a period.
///
/// Reads `quantity->>'provider_cost_micros'` — the number the gateway metered, not the number the
/// customer was charged. Comparing an invoice against *credits* would be comparing a cost to a
/// price, and the margin between them would look exactly like drift.
pub async fn recorded_cogs(
    pool: &PgPool,
    from: time::OffsetDateTime,
    to: time::OffsetDateTime,
) -> Result<BTreeMap<String, i64>, DriftError> {
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "-- tenant-scoping: cross-tenant — COGS is what we paid a provider across all accounts;
         -- the result is keyed by model and names no customer.
         SELECT quantity->>'model', sum((quantity->>'provider_cost_micros')::bigint)::bigint
         FROM ledger_entries
         WHERE kind = 'usage.model' AND at >= $1 AND at < $2
           AND quantity ? 'provider_cost_micros' AND quantity ? 'model'
         GROUP BY 1",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .map_err(|e| PgError::Query(e.to_string()))?;

    Ok(rows.into_iter().collect())
}

/// One model's comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftRow {
    pub model: String,
    pub ours_micros: i64,
    pub theirs_micros: i64,
    /// Ours minus theirs. Positive means we recorded more than we were billed.
    pub drift_micros: i64,
    pub class: Class,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Within tolerance.
    Ok,
    /// Outside tolerance, both sides present.
    Drifted,
    /// On the invoice, absent from our ledger — traffic we did not meter.
    Unmetered,
    /// In our ledger, absent from the invoice — usually a model whose invoice line has not
    /// arrived yet, occasionally something worse.
    Unbilled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriftReport {
    pub rows: Vec<DriftRow>,
    pub total_ours_micros: i64,
    pub total_theirs_micros: i64,
}

impl DriftReport {
    pub fn total_drift_micros(&self) -> i64 {
        self.total_ours_micros - self.total_theirs_micros
    }

    /// Rows that need a person. Empty means the month reconciles.
    pub fn alarms(&self) -> Vec<&DriftRow> {
        self.rows.iter().filter(|r| r.class != Class::Ok).collect()
    }

    /// A one-screen summary for a cron job's output — the thing that ends up in an e-mail.
    pub fn to_text(&self) -> String {
        let mut out = format!(
            "ledger vs provider: ours ${:.2}, theirs ${:.2}, drift ${:.2}\n",
            self.total_ours_micros as f64 / 1e6,
            self.total_theirs_micros as f64 / 1e6,
            self.total_drift_micros() as f64 / 1e6,
        );
        for row in &self.rows {
            let mark = match row.class {
                Class::Ok => "ok       ",
                Class::Drifted => "DRIFT    ",
                Class::Unmetered => "UNMETERED",
                Class::Unbilled => "UNBILLED ",
            };
            out.push_str(&format!(
                "{mark} {:<32} ours ${:>10.4}  theirs ${:>10.4}  Δ ${:>10.4}\n",
                row.model,
                row.ours_micros as f64 / 1e6,
                row.theirs_micros as f64 / 1e6,
                row.drift_micros as f64 / 1e6,
            ));
        }
        out
    }
}

/// Compare, with a tolerance in basis points (100 bp = 1%).
///
/// A tolerance exists because rounding is real: we price per token from a table, the provider bills
/// from theirs, and the two round differently on every call. A tolerance of zero would page somebody
/// every month over a cent. `absolute_floor_micros` keeps a tiny line from tripping the percentage
/// test — 40% of two cents is not an incident.
pub fn compare(
    ours: &BTreeMap<String, i64>,
    theirs: &[ProviderLine],
    tolerance_bp: i64,
    absolute_floor_micros: i64,
) -> DriftReport {
    let mut by_model: BTreeMap<String, (i64, i64)> = BTreeMap::new();
    for (model, cost) in ours {
        by_model.entry(model.clone()).or_default().0 = *cost;
    }
    for line in theirs {
        by_model.entry(line.model.clone()).or_default().1 = line.cost_micros;
    }

    let mut rows = Vec::new();
    let (mut total_ours, mut total_theirs) = (0i64, 0i64);
    for (model, (ours_micros, theirs_micros)) in by_model {
        total_ours += ours_micros;
        total_theirs += theirs_micros;
        let drift = ours_micros - theirs_micros;

        let class = if ours_micros == 0 && theirs_micros != 0 {
            Class::Unmetered
        } else if theirs_micros == 0 && ours_micros != 0 {
            Class::Unbilled
        } else {
            // Two tolerances, and either one clears the row: an absolute floor for lines too small
            // for a percentage to mean anything (40% of two cents is not an incident), and a
            // percentage for everything else.
            let within_floor = drift.abs() <= absolute_floor_micros;
            let within_percentage =
                theirs_micros != 0 && drift.abs() * 10_000 / theirs_micros.abs() <= tolerance_bp;
            if within_floor || within_percentage {
                Class::Ok
            } else {
                Class::Drifted
            }
        };

        rows.push(DriftRow {
            model,
            ours_micros,
            theirs_micros,
            drift_micros: drift,
            class,
        });
    }

    DriftReport {
        rows,
        total_ours_micros: total_ours,
        total_theirs_micros: total_theirs,
    }
}

/// Publish the result where an alert can see it (docs/21 §alarm plumbing).
///
/// A gauge and a log line, nothing else. The alerting rule belongs in the monitoring system, not
/// in this binary: a threshold compiled into a release is a threshold nobody can change at 3am.
pub fn publish(provider: &str, report: &DriftReport) {
    let drift = report.total_drift_micros();
    let metrics = panday_sdk::metrics::metrics();
    metrics
        .ledger_drift_micros
        .set(&[provider], drift.unsigned_abs() as f64);
    metrics
        .ledger_drift_direction
        .set(&[provider], f64::from(drift > 0));

    let alarms = report.alarms();
    if alarms.is_empty() {
        tracing::info!(provider, drift_micros = drift, "ledger reconciles");
        return;
    }
    for row in alarms {
        tracing::error!(
            provider,
            model = %row.model,
            ours_micros = row.ours_micros,
            theirs_micros = row.theirs_micros,
            drift_micros = row.drift_micros,
            class = ?row.class,
            "ledger drift"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ours(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
        pairs.iter().map(|(m, c)| (m.to_string(), *c)).collect()
    }

    fn theirs(pairs: &[(&str, i64)]) -> Vec<ProviderLine> {
        pairs
            .iter()
            .map(|(m, c)| ProviderLine {
                model: m.to_string(),
                cost_micros: *c,
                input_tokens: 0,
                output_tokens: 0,
            })
            .collect()
    }

    #[test]
    fn a_month_that_reconciles_raises_nothing() {
        let report = compare(
            &ours(&[("anthropic/claude-sonnet-4-5", 1_000_000)]),
            &theirs(&[("anthropic/claude-sonnet-4-5", 1_000_000)]),
            100,
            10_000,
        );
        assert!(report.alarms().is_empty());
        assert_eq!(report.total_drift_micros(), 0);
    }

    #[test]
    fn rounding_noise_is_not_an_incident() {
        // We price per token from our table, they bill from theirs, and the two round differently
        // on every call. A tolerance of zero pages somebody every month over a cent.
        let report = compare(
            &ours(&[("m", 1_000_000)]),
            &theirs(&[("m", 1_004_000)]),
            100, // 1%
            10_000,
        );
        assert!(report.alarms().is_empty(), "{:?}", report.rows);
    }

    #[test]
    fn a_real_difference_is_reported_with_its_direction() {
        // Over-recording may mean we over-charged a customer; under-recording means we are eating
        // cost. Same magnitude, opposite response.
        let over = compare(
            &ours(&[("m", 2_000_000)]),
            &theirs(&[("m", 1_000_000)]),
            100,
            10_000,
        );
        assert_eq!(over.rows[0].class, Class::Drifted);
        assert!(
            over.total_drift_micros() > 0,
            "we recorded more than we were billed"
        );

        let under = compare(
            &ours(&[("m", 1_000_000)]),
            &theirs(&[("m", 2_000_000)]),
            100,
            10_000,
        );
        assert!(under.total_drift_micros() < 0);
    }

    #[test]
    fn a_model_on_the_invoice_and_not_in_the_ledger_is_its_own_class() {
        // Traffic we did not meter at all. A percentage comparison would average it away against
        // everything that did match, which is exactly the case worth shouting about.
        let report = compare(&ours(&[]), &theirs(&[("m", 5_000_000)]), 100, 10_000);
        assert_eq!(report.rows[0].class, Class::Unmetered);
        assert_eq!(report.alarms().len(), 1);
    }

    #[test]
    fn a_model_in_the_ledger_and_not_on_the_invoice_is_flagged_but_named_differently() {
        // Usually an invoice line that has not arrived yet; occasionally something worse. Either
        // way it is not the same finding as unmetered traffic.
        let report = compare(&ours(&[("m", 5_000_000)]), &theirs(&[]), 100, 10_000);
        assert_eq!(report.rows[0].class, Class::Unbilled);
    }

    #[test]
    fn a_tiny_line_does_not_trip_the_percentage_test() {
        // 40% of two cents is not an incident.
        let report = compare(
            &ours(&[("m", 20_000)]),
            &theirs(&[("m", 12_000)]),
            100,
            10_000,
        );
        assert_eq!(report.rows[0].class, Class::Ok);
    }

    #[test]
    fn a_report_parses_with_or_without_a_header() {
        let with =
            parse_report("model,cost_usd,input_tokens,output_tokens\nm,1.50,100,20\n").unwrap();
        let without = parse_report("m,1.50,100,20\n").unwrap();
        assert_eq!(with, without);
        assert_eq!(with[0].cost_micros, 1_500_000);
        assert_eq!(with[0].input_tokens, 100);
    }

    #[test]
    fn a_dollar_sign_and_blank_lines_are_tolerated() {
        // Exports are messy; the monitor is not the place to be precious about it.
        let lines = parse_report("\n# exported 2026-08-01\nm,$2.25,0,0\n\n").unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].cost_micros, 2_250_000);
    }

    #[test]
    fn a_malformed_line_is_an_error_rather_than_a_skipped_row() {
        // Skipping would under-count the invoice and report drift in our favour, which is the
        // direction nobody double-checks.
        assert!(parse_report("m,1.50\n").is_err());
        assert!(parse_report("m,not-a-number,0,0\n").is_err());
    }
}
