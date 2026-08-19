//! M20.3 — the tenant-scoping lint, and the workspace scan that arms it (docs/20 T5).
//!
//! Two things are being tested: that the rule catches what it claims to, and that the
//! rule is actually *applied* to every SQL statement in the repo. The second is the
//! part that decays — a lint nobody runs is a comment — so it walks the tree rather
//! than taking a list of files as given.

use panday_platform::tenancy::{
    claims_single_tenant, table_declares_account_id, unscoped_statements, GLOBAL_TABLES,
    SINGLE_TENANT_MARKER, TENANT_TABLES,
};
use std::path::{Path, PathBuf};

// ── The rule ─────────────────────────────────────────────────────────────────

#[test]
fn a_query_on_a_tenant_table_without_account_id_is_flagged() {
    let findings = unscoped_statements("SELECT amount_micros FROM ledger_entries WHERE at > $1");
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].table, "ledger_entries");
}

#[test]
fn a_scoped_query_passes() {
    assert!(unscoped_statements(
        "SELECT amount_micros FROM ledger_entries WHERE account_id = $1 AND at > $2"
    )
    .is_empty());
}

#[test]
fn every_statement_kind_is_covered_not_just_select() {
    // A `DELETE FROM sessions` with no tenant predicate deletes everyone's sessions,
    // which is worse than reading them.
    for sql in [
        "DELETE FROM sessions WHERE at < $1",
        "UPDATE api_keys SET revoked_at = now()",
        "INSERT INTO artifacts (hash, size) VALUES ($1, $2)",
        "WITH recent AS (SELECT * FROM events) SELECT count(*) FROM recent",
    ] {
        assert!(!unscoped_statements(sql).is_empty(), "not flagged: {sql}");
    }
}

#[test]
fn a_commented_out_scope_does_not_satisfy_the_rule() {
    // Exactly how a scoped query becomes unscoped: someone comments the predicate out
    // while debugging and forgets.
    let sql = "SELECT * FROM sessions WHERE true -- AND account_id = $1";
    assert_eq!(unscoped_statements(sql).len(), 1);

    let block = "SELECT * FROM sessions /* WHERE account_id = $1 */";
    assert_eq!(unscoped_statements(block).len(), 1);
}

#[test]
fn global_tables_are_exempt_and_say_so() {
    // A price table is the same for everyone. The exemption is a list rather than an
    // inference, so "this table is global" is a decision on the record.
    assert!(unscoped_statements("SELECT * FROM prices WHERE model = $1").is_empty());
    for table in GLOBAL_TABLES {
        assert!(
            !TENANT_TABLES.contains(table),
            "{table} cannot be both global and tenant-scoped"
        );
    }
}

#[test]
fn table_names_match_on_word_boundaries() {
    // `accounts` must not match `service_accounts_audit`, or the lint cries wolf and
    // gets deleted.
    assert!(unscoped_statements("SELECT * FROM service_accounts_audit").is_empty());
    assert!(unscoped_statements("SELECT * FROM event_sourcing_notes").is_empty());
    assert_eq!(unscoped_statements("SELECT * FROM accounts").len(), 1);
}

#[test]
fn ddl_is_judged_on_whether_the_column_exists_at_all() {
    // A query lint cannot help if there is no column to filter on.
    let missing = "CREATE TABLE sessions (id UUID PRIMARY KEY, at TIMESTAMPTZ)";
    assert_eq!(
        table_declares_account_id(missing).as_deref(),
        Some("sessions")
    );

    let present =
        "CREATE TABLE sessions (id UUID PRIMARY KEY, account_id UUID NOT NULL, at TIMESTAMPTZ)";
    assert_eq!(table_declares_account_id(present), None);

    // docs/17's own ledger DDL passes, which is the point of copying the rule from it.
    let ledger = "CREATE TABLE ledger_entries (id UUID PRIMARY KEY, account_id UUID NOT NULL, \
                  amount_micros BIGINT NOT NULL)";
    assert_eq!(table_declares_account_id(ledger), None);
}

#[test]
fn several_statements_in_one_file_are_all_checked() {
    let sql = "SELECT 1 FROM prices; \
               SELECT * FROM balances WHERE account_id = $1; \
               DELETE FROM events WHERE seq < $1;";
    let findings = unscoped_statements(sql);
    assert_eq!(findings.len(), 1, "{findings:?}");
    assert_eq!(findings[0].table, "events");
}

// ── The scan (what makes it a lint and not a library) ────────────────────────

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .to_path_buf()
}

fn files_with(extensions: &[&str], dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path
                .file_name()
                .is_some_and(|n| n == "target" || n == ".git")
            {
                continue;
            }
            files_with(extensions, &path, out);
        } else if path
            .extension()
            .is_some_and(|e| extensions.iter().any(|x| e == *x))
        {
            out.push(path);
        }
    }
}

/// Whether a path is test code.
///
/// Test files are skipped, and they have to be: a lint's own fixtures are examples of
/// the thing it forbids, so a scan that read them could never be armed. The rule this
/// enforces is about shipped queries — a test that reads a whole table is reading a
/// test database.
fn is_test_code(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == "tests" || c.as_os_str() == "fixtures")
}

/// Every SQL statement under `root` — in `.sql` files and in Rust string literals
/// handed to `sqlx`.
fn sql_under(root: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();

    let mut sql_files = Vec::new();
    files_with(&["sql"], root, &mut sql_files);
    for path in sql_files {
        if is_test_code(&path) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            out.push((path, text));
        }
    }

    let mut rust_files = Vec::new();
    files_with(&["rs"], root, &mut rust_files);
    for path in rust_files {
        if is_test_code(&path) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // A file that declares itself single-tenant, with a reason, is exempt. See
        // `SINGLE_TENANT_MARKER` — the offline store has no accounts to scope by.
        if claims_single_tenant(&text).is_some() {
            continue;
        }
        // The *contents* of string literals, not the lines holding them: the rule keys
        // on a statement's leading keyword, and `let sql = "DELETE FROM …"` does not
        // start with one. Extracting the literal is what makes a Rust file lintable by
        // the same function that lints a `.sql` file.
        for literal in string_literals(&text) {
            let lower = literal.to_lowercase();
            let looks_like_sql = ["select ", "insert into", "update ", "delete from"]
                .iter()
                .any(|kw| lower.contains(kw));
            if looks_like_sql {
                out.push((path.clone(), literal));
            }
        }
    }
    out
}

/// Double-quoted literals, one per element. Deliberately simple — it does not know
/// about raw strings or escapes, and it does not need to: SQL in Rust is written in
/// plain literals, and a missed exotic form is a gap the reviewer covers, not a false
/// pass on an ordinary query.
fn string_literals(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    let mut escaped = false;
    for c in text.chars() {
        match &mut current {
            Some(buf) => {
                if escaped {
                    escaped = false;
                    buf.push(c);
                } else if c == '\\' {
                    escaped = true;
                } else if c == '"' {
                    out.push(std::mem::take(buf));
                    current = None;
                } else {
                    buf.push(c);
                }
            }
            None => {
                if c == '"' {
                    current = Some(String::new());
                }
            }
        }
    }
    out
}

fn sql_in_the_repo() -> Vec<(PathBuf, String)> {
    sql_under(&workspace_root())
}

#[test]
fn no_sql_in_the_repo_touches_a_tenant_table_unscoped() {
    // Today this passes because there is no SQL — which is the moment to arm a lint.
    // The first unscoped query is the one written while someone is debugging something
    // else, and by the time there are fifty queries this becomes a migration project
    // instead of a guardrail.
    let mut violations = Vec::new();
    for (path, sql) in sql_in_the_repo() {
        for finding in unscoped_statements(&sql) {
            violations.push(format!(
                "{}: {} is tenant-scoped but this statement names no account_id: {}",
                path.display(),
                finding.table,
                finding.statement
            ));
        }
    }
    assert!(
        violations.is_empty(),
        "unscoped tenant queries (docs/20 T5):\n  {}",
        violations.join("\n  ")
    );
}

#[test]
fn every_tenant_table_declaration_carries_the_column() {
    let mut missing = Vec::new();
    for (path, sql) in sql_in_the_repo() {
        for statement in sql.split(';') {
            if let Some(table) = table_declares_account_id(statement) {
                missing.push(format!("{}: {table} has no account_id", path.display()));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}

#[test]
fn a_single_tenant_file_is_exempt_only_with_a_reason() {
    // The exemption exists because `panday local`'s SQLite database has no accounts in it
    // (docs/18: the free tier needs no account at all), and a constant column added to
    // satisfy a lint would be theatre. It must still cost something to claim.
    let with_reason = format!(
        "//! {SINGLE_TENANT_MARKER}: one laptop, one user; the filesystem is the boundary\n         SELECT * FROM events;"
    );
    assert_eq!(
        claims_single_tenant(&with_reason).as_deref(),
        Some("one laptop, one user; the filesystem is the boundary")
    );

    let bare = format!("//! {SINGLE_TENANT_MARKER}\nSELECT * FROM events;");
    assert!(
        claims_single_tenant(&bare).is_none(),
        "a marker with no reason is not an exemption"
    );

    // And it only counts near the top of the file, so it reads as a property of the module
    // rather than something buried next to the query it excuses.
    let buried = format!("{}\n//! {SINGLE_TENANT_MARKER}: too late", "\n".repeat(60));
    assert!(claims_single_tenant(&buried).is_none());
}

#[test]
fn the_offline_store_is_the_only_file_claiming_the_exemption() {
    // A count, so a second claim has to be noticed. This is the check that keeps an
    // exemption from becoming a habit.
    let root = workspace_root();
    let mut files = Vec::new();
    files_with(&["rs"], &root.join("crates"), &mut files);
    let claiming: Vec<String> = files
        .iter()
        .filter(|p| !is_test_code(p))
        .filter_map(|p| {
            let text = std::fs::read_to_string(p).ok()?;
            claims_single_tenant(&text).map(|reason| format!("{}: {reason}", p.display()))
        })
        .collect();
    assert_eq!(
        claiming.len(),
        1,
        "exactly one file should be single-tenant today: {claiming:?}"
    );
    assert!(claiming[0].contains("panday-local"), "{claiming:?}");
}

#[test]
fn the_scanner_finds_a_planted_violation() {
    // Without this, "no SQL in the repo" and "the scanner is broken" look identical —
    // and this lint spends most of its life in the first state, so the second would go
    // unnoticed for months.
    //
    // Planted in a temp tree rather than in the repo: writing into the repo would race
    // the scan test above, which runs concurrently and would see the violation and fail.
    let root = std::env::temp_dir().join(format!("panday-tenancy-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).expect("temp tree");
    std::fs::write(
        root.join("migrations.sql"),
        "SELECT amount_micros FROM ledger_entries WHERE at > now();\n",
    )
    .expect("write sql");
    std::fs::write(
        root.join("src/repo.rs"),
        "let sql = \"DELETE FROM sessions WHERE at < $1\";\n",
    )
    .expect("write rust");

    let findings: Vec<_> = sql_under(&root)
        .into_iter()
        .flat_map(|(_, sql)| unscoped_statements(&sql))
        .collect();

    std::fs::remove_dir_all(&root).ok();
    assert!(
        findings.iter().any(|f| f.table == "ledger_entries"),
        "the .sql walker missed a planted file: {findings:?}"
    );
    assert!(
        findings.iter().any(|f| f.table == "sessions"),
        "the Rust-literal walker missed a planted query: {findings:?}"
    );
}

#[test]
fn a_statement_may_exempt_itself_with_a_reason() {
    let sql = "-- tenant-scoping: cross-tenant — operator aggregate, no tenant data
               SELECT count(*) FROM ledger_entries";
    assert!(unscoped_statements(sql).is_empty());
}

#[test]
fn a_bare_marker_exempts_nothing() {
    // The reason is the whole point: an exemption that needs no argument is one that spreads.
    let sql = "-- tenant-scoping: cross-tenant
               SELECT count(*) FROM ledger_entries";
    assert_eq!(unscoped_statements(sql).len(), 1);
}

#[test]
fn an_exemption_covers_only_its_own_statement() {
    // The failure this prevents: one justified aggregate at the top of a file quietly
    // legitimising every query written under it.
    let sql = "-- tenant-scoping: cross-tenant — operator aggregate
               SELECT count(*) FROM ledger_entries;
               SELECT amount_micros FROM ledger_entries";
    let found = unscoped_statements(sql);
    assert_eq!(found.len(), 1, "{found:?}");
    assert!(found[0].statement.contains("amount_micros"));
}

#[test]
fn a_commented_out_account_id_still_fails() {
    // Splitting on `;` before stripping comments must not weaken this: it is exactly how a
    // scoped query becomes unscoped during a debugging session.
    let sql = "SELECT * FROM ledger_entries WHERE true -- AND account_id = $1";
    assert_eq!(unscoped_statements(sql).len(), 1);
}

#[test]
fn a_semicolon_inside_a_comment_does_not_split_a_statement() {
    // Found by the lint tripping on its own exemption (M21.4): a prose comment containing a
    // semicolon split the statement there, leaving a fragment that held the table name and not the
    // `account_id` that scoped it. A false positive on a correct query — and the same bug would
    // hide a genuinely unscoped one whose scope happened to land in the other half.
    let sql = "-- we read this across accounts; keyed by model
               SELECT amount_micros FROM ledger_entries WHERE account_id = $1";
    assert!(
        unscoped_statements(sql).is_empty(),
        "{:?}",
        unscoped_statements(sql)
    );

    // And the exemption survives a semicolon in its own reason.
    let exempt = "-- tenant-scoping: cross-tenant — summed across accounts; names no customer
                  SELECT sum(amount_micros) FROM ledger_entries";
    assert!(unscoped_statements(exempt).is_empty());
}
