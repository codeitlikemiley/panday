//! Tenant scoping as a checkable rule (M20.3, docs/20 T5).
//!
//! > "Every query is tenant-scoped by construction: `account_id` in every table,
//! > enforced via sqlx query review + a CI lint that rejects unscoped queries on
//! > tenant tables."
//!
//! ## Why this exists before the first query does
//!
//! This was armed before there was any SQL to lint — which is exactly when a lint is
//! worth writing. There are now 49 `sqlx::query` sites and 8 migrations here, so it
//! guards something real. The first unscoped query is the one that gets written while someone
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
    "subscriptions",
    "credit_grants",
    "grants",
    "ledger_entries",
    "balances",
    "sessions",
    "events",
    "artifacts",
    "usage_records",
    "route_decisions",
    // The exact-response cache (M11.10). Its key is `(account_id, digest)`; an unscoped read here
    // serves one tenant another tenant's answer, which is M20.3's finding in its live form.
    "exact_cache",
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
    // A plan is a catalogue row: the same plan means the same thing for every account, and an
    // account's relationship to it lives in `subscriptions` (which *is* tenant-scoped). Moved
    // here at M17.1 when the table was actually written — it had been guessed at as
    // tenant-scoped, and the migration made the guess wrong.
    "plans",
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

/// The marker one *statement* puts in its own SQL comment.
///
/// The file-level exemption is too blunt for a module that holds both kinds of query:
/// `panday_platform::routes` reads one account's decisions (must be scoped) and aggregates every
/// account's rule health for an operator (cannot be). Exempting the file would silently un-lint the
/// scoped query next to it — which is the query that matters.
///
/// A cross-tenant statement is legitimate only when it returns no tenant data: an aggregate keyed
/// by something that is not an account, or a retention delete keyed by age. If the result set can
/// name a customer, this marker is the wrong tool.
pub const CROSS_TENANT_MARKER: &str = "tenant-scoping: cross-tenant";

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
    // Split *before* stripping, so a statement's own comment can carry its exemption. The account_id
    // check still runs on the stripped text, so a commented-out column still fails.
    for raw in split_statements(sql) {
        if claims_cross_tenant(&raw).is_some() {
            continue;
        }
        let statement = strip_comments(&raw);
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

/// Split on `;`, ignoring semicolons inside comments.
///
/// A plain `split(';')` was the first version and it was wrong in a way only a prose comment
/// reveals: `-- COGS is what we paid across all accounts; keyed by model` splits *there*, leaving a
/// fragment that holds the table name and not the `account_id` that scoped it — a false positive on
/// a correct query, and a false negative waiting to happen on the other side of it.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '-' if chars.peek() == Some(&'-') => {
                current.push(c);
                for c in chars.by_ref() {
                    current.push(c);
                    if c == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                current.push(c);
                let mut last = ' ';
                for c in chars.by_ref() {
                    current.push(c);
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            ';' => {
                out.push(std::mem::take(&mut current));
            }
            c => current.push(c),
        }
    }
    if !current.trim().is_empty() {
        out.push(current);
    }
    out
}

/// Whether one statement claims the cross-tenant exemption, with a reason.
///
/// A bare marker is not an exemption, for the same reason a bare file marker is not: the reason is
/// the thing a reviewer reads.
pub fn claims_cross_tenant(statement: &str) -> Option<String> {
    let at = statement.find(CROSS_TENANT_MARKER)?;
    let rest = &statement[at + CROSS_TENANT_MARKER.len()..];
    let reason = rest
        .lines()
        .next()
        .unwrap_or_default()
        .trim_start_matches([':', '—', '-', ' '])
        .trim();
    (!reason.is_empty()).then(|| reason.to_string())
}

/// Whether a `CREATE TABLE` for a tenant table declares `account_id`.
///
/// The other half of "account_id in every table": a query lint cannot help if the
/// column is not there to filter on.
pub fn table_declares_account_id(create_statement: &str) -> Option<String> {
    let sql = strip_comments(create_statement);
    // `CREATE UNLOGGED TABLE` is still a CREATE TABLE. The gateway's exact cache (M11.10) is the
    // first unlogged table in this schema, and without this normalisation the DDL half of the lint
    // silently skipped the one table whose entire risk is cross-tenant serving.
    let lower = sql
        .to_lowercase()
        .replace("create unlogged table", "create table");
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
