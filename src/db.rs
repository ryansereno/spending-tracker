// Thin wrapper around rusqlite: open a connection, apply the schema,
// and insert transactions while letting SQLite reject duplicates.

use anyhow::Result;
use rusqlite::{Connection, params};

use crate::model::Transaction;

// The schema lives here as a raw string. CREATE TABLE IF NOT EXISTS
// makes this idempotent — safe to run every startup.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS transactions (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    source       TEXT    NOT NULL,
    txn_date     TEXT    NOT NULL,
    description  TEXT    NOT NULL,
    amount_cents INTEGER NOT NULL,
    category     TEXT,
    card_member  TEXT,
    fingerprint  TEXT    NOT NULL UNIQUE,
    imported_at  TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP
);
CREATE INDEX IF NOT EXISTS idx_txn_date ON transactions(txn_date);

-- Pattern-to-category mapping. A 'pattern' is a lowercase substring;
-- a transaction matches if its lowercased description contains the pattern.
-- On match conflicts (multiple patterns hit the same description) we
-- prefer the LONGEST pattern, since longer = more specific.
CREATE TABLE IF NOT EXISTS merchant_rules (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    pattern    TEXT    NOT NULL,
    category   TEXT    NOT NULL,
    origin     TEXT    NOT NULL,   -- 'monarch_seed' | 'manual'
    created_at TEXT    NOT NULL DEFAULT CURRENT_TIMESTAMP,
    UNIQUE(pattern, category)
);
";

// Open (or create) the SQLite file at `path` and apply the schema.
// `&str` is a borrowed string slice — cheap to pass around, no allocation.
pub fn open(path: &str) -> Result<Connection> {
    let conn = Connection::open(path)?;
    // execute_batch runs multiple statements separated by `;` in one call.
    conn.execute_batch(SCHEMA)?;
    Ok(conn)
}

// Read-side row shape. We keep dates as `String` (already 'YYYY-MM-DD' in
// the DB) so the frontend can use them as-is — no chrono dependency leaks
// into the TUI/API layer just for display.
#[derive(Debug, Clone)]
pub struct TransactionRow {
    pub id: i64,
    pub source: String,
    pub txn_date: String,
    pub description: String,
    pub amount_cents: i64,
    pub category: Option<String>,
}

pub fn list_transactions(conn: &Connection) -> Result<Vec<TransactionRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, source, txn_date, description, amount_cents, category
           FROM transactions
          ORDER BY txn_date DESC, id DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(TransactionRow {
            id: row.get(0)?,
            source: row.get(1)?,
            txn_date: row.get(2)?,
            description: row.get(3)?,
            amount_cents: row.get(4)?,
            category: row.get(5)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

#[derive(Debug, Clone)]
pub struct CategorySummary {
    pub category: String, // '(uncategorized)' for NULL — done in SQL via COALESCE
    pub count: i64,
    pub total_cents: i64,
}

// Union of categories ever seen — used to populate the TUI's autocomplete
// picker. Pulls from both already-categorized transactions and rules so a
// brand-new rule-only category shows up before any rows have been assigned.
pub fn list_categories(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT category FROM (
             SELECT category FROM transactions WHERE category IS NOT NULL
             UNION
             SELECT category FROM merchant_rules
         )
         ORDER BY category",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn summary_by_category(conn: &Connection) -> Result<Vec<CategorySummary>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(category, '(uncategorized)') AS cat,
                COUNT(*),
                SUM(amount_cents)
           FROM transactions
          GROUP BY cat
          ORDER BY ABS(SUM(amount_cents)) DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(CategorySummary {
            category: row.get(0)?,
            count: row.get(1)?,
            total_cents: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// Insert one transaction. Returns true if the row was new, false if a
// row with the same fingerprint already existed (i.e. it was a duplicate).
//
// `INSERT OR IGNORE` is SQLite's way of saying "if a UNIQUE constraint
// would be violated, silently skip this row instead of erroring." We
// then ask rusqlite how many rows were actually changed: 1 = inserted,
// 0 = skipped.
pub fn insert_transaction(conn: &Connection, txn: &Transaction) -> Result<bool> {
    let rows_changed = conn.execute(
        "INSERT OR IGNORE INTO transactions
             (source, txn_date, description, amount_cents, card_member, fingerprint)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            txn.source.as_str(),
            txn.txn_date.format("%Y-%m-%d").to_string(),
            txn.description,
            txn.amount_cents,
            txn.card_member,
            txn.fingerprint(),
        ],
    )?;
    Ok(rows_changed == 1)
}
