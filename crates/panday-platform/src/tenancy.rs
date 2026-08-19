//! Tenant scoping as a checkable rule (M20.3, docs/20 T5).
//!
//! > "Every query is tenant-scoped by construction: `account_id` in every table,
//! > enforced via sqlx query review + a CI lint that rejects unscoped queries on
//! > tenant tables."
//!
//! ## Why this exists before the first query does
//!
//! Postgres is M3.5, so there is no SQL to lint yet — which is exactly when a lint is
//! worth writing. The first unscoped query is the one that gets written while someone
//! is debugging something else, and by the time there are fifty queries a lint becomes
//! a migration project instead of a guardrail. Armed now, it costs one test run and
//! the first violation fails a build.
//!
//! ## What counts as scoped
//!
//! A statement touching a tenant table must mention `account_id`. That is a coarse
//! rule and it is chosen deliberately over anything cleverer: a real parser would
//! judge *whether* the predicate is correct, which is a code review's job, while this
//! catches the mistake that actually happens — forgetting the tenant entirely.
//! `AND account_id = $1` in the wrong place is a review finding; no `account_id` at
//! all is a data breach, and only the second is decidable by a lint.

/// Tables whose rows belong to an account (docs/17 §schema, docs/03 §storage).
///
/// A table added here inherits the rule; a table *not* here is asserting it holds no
/// tenant data, which is a claim someone has to make on purpose.
pub const TENANT_TABLES: &[&str] = &[
    "accounts",
    "api_keys",
    "plans",
    "grants",
    "ledger_entries",
    "balances",
    "sessions",
    "events",
    "artifacts",
    "usage_records",
    "route_decisions",
    "webhook_inbox",
    "secrets",
    "plugin_installs",
];

/// Tables that deliberately hold no tenant data. Listed rather than implied, so
/// "this table is global" is a decision on the record.
pub const GLOBAL_TABLES: &[&str] = &[
    // The model catalog and price table are the same for everyone (docs/12, docs/17).
    "models",
    "prices",
    "migrations",
];

/// The marker a single-tenant store puts at the top of its file.
///
/// A store with no accounts in it — `panday local`'s SQLite database is the case that
/// forced this (docs/18: the free tier needs no account at all) — cannot scope by
/// `account_id`, and adding a constant column to satisfy a lint would be theatre.
///
/// The exemption is per file, must name a reason after the marker, and is greppable. That
/// combination is deliberate: an exemption nobody can find is an exemption nobody reviews,
/// and one that needs no reason is one that spreads.
pub const SINGLE_TENANT_MARKER: &str = "tenant-scoping: single-tenant";

/// Whether a file claims the exemption, and gives a reason for it.
///
/// A marker with nothing after it is *not* an exemption: the reason is the whole point,
/// and a bare marker would let "I was in a hurry" pass as an argument.
pub fn claims_single_tenant(source: &str) -> Option<String> {
    for line in source.lines().take(40) {
        if let Some(at) = line.find(SINGLE_TENANT_MARKER) {
            let reason = line[at + SINGLE_TENANT_MARKER.len()..]
                .trim_start_matches([':', '—', '-', ' '])
                .trim();
            return (!reason.is_empty()).then(|| reason.to_string());
        }
    }
    None
}

/// A statement the lint objected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unscoped {
    pub table: String,
    /// The statement, normalized to one line for a readable failure.
    pub statement: String,
}

/// Find statements that touch a tenant table without naming `account_id`.
///
/// `sql` may hold many statements; they are split on `;`. Comments are stripped first,
/// because a commented-out `account_id` must not satisfy the rule — that is precisely
/// how a scoped query becomes unscoped during a debugging session.
pub fn unscoped_statements(sql: &str) -> Vec<Unscoped> {
    let mut out = Vec::new();
    for statement in strip_comments(sql).split(';') {
        let normalized = statement.split_whitespace().collect::<Vec<_>>().join(" ");
        if normalized.is_empty() {
            continue;
        }
        let lower = normalized.to_lowercase();

        // DDL declares a table rather than reading it; a `CREATE TABLE` that forgot
        // `account_id` is caught by `table_declares_account_id` below, which is a
        // different and stronger check than "mentions the column".
        if lower.starts_with("create ") || lower.starts_with("alter ") {
            continue;
        }
        if !(lower.starts_with("select")
            || lower.starts_with("insert")
            || lower.starts_with("update")
            || lower.starts_with("delete")
            || lower.starts_with("with"))
        {
            continue;
        }
        if lower.contains("account_id") {
            continue;
        }
        for table in TENANT_TABLES {
            if mentions_table(&lower, table) {
                out.push(Unscoped {
                    table: (*table).to_string(),
                    statement: normalized.clone(),
                });
                break;
            }
        }
    }
    out
}

/// Whether a `CREATE TABLE` for a tenant table declares `account_id`.
///
/// The other half of "account_id in every table": a query lint cannot help if the
/// column is not there to filter on.
pub fn table_declares_account_id(create_statement: &str) -> Option<String> {
    let sql = strip_comments(create_statement);
    let lower = sql.to_lowercase();
    if !lower.contains("create table") {
        return None;
    }
    let table = TENANT_TABLES
        .iter()
        .find(|t| mentions_table(&lower, t))?
        .to_string();
    (!lower.contains("account_id")).then_some(table)
}

/// Word-boundary match, so `accounts` does not match `service_accounts_audit` and
/// `events` does not match `event_sourcing_notes`.
fn mentions_table(lower_sql: &str, table: &str) -> bool {
    lower_sql.match_indices(table).any(|(at, _)| {
        let before = lower_sql[..at].chars().next_back();
        let after = lower_sql[at + table.len()..].chars().next();
        let boundary = |c: Option<char>| match c {
            None => true,
            Some(c) => !c.is_alphanumeric() && c != '_',
        };
        boundary(before) && boundary(after)
    })
}

/// Remove `--` line comments and `/* */` blocks.
fn strip_comments(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'-') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            c => out.push(c),
        }
    }
    out
}
