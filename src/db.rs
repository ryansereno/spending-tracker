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
