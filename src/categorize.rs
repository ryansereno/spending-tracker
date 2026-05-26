// Categorization: seed rules from a Monarch export, and apply rules to
// uncategorized transactions in our DB.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use serde::Deserialize;

// --- Monarch export shape -------------------------------------------------

// We only declare the columns we care about. Monarch's CSV has Date, Notes,
// Amount, Tags, Owner too — Serde ignores fields we don't list.
#[derive(Debug, Deserialize)]
struct MonarchRow {
    #[serde(rename = "Merchant")]
    merchant: String,
    #[serde(rename = "Category")]
    category: String,
    #[serde(rename = "Original Statement")]
    original_statement: String,
}

// --- Text normalization ---------------------------------------------------

// Lowercase + collapse any whitespace run to a single space, trim.
// We use this on BOTH sides when comparing descriptions, so that
// "WEIS MARKETS  200" (two spaces, from Amex) and "WEIS MARKETS 200"
// (one space, from Monarch) compare equal.
fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

// --- The seed pass --------------------------------------------------------

pub struct SeedReport {
    pub direct_matches: usize, // transactions categorized via exact statement match
    pub rules_learned: usize,  // new (merchant, category) rules inserted
    pub monarch_rows: usize,   // total rows read from the Monarch CSV
}

pub fn seed_from_monarch(path: &Path, conn: &mut Connection) -> Result<SeedReport> {
    // We wrap the whole seed in one transaction. That's a 10–100x speedup
    // for many small writes in SQLite (one fsync at commit, not per-row),
    // and gives us all-or-nothing semantics if anything errors midway.
    let tx = conn.transaction()?;

    // Load all existing transactions ONCE into a lookup map:
    //   normalized_description -> Vec<row_id>
    // Building this in memory is cheaper than running a query per Monarch row.
    let mut desc_index: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    {
        let mut stmt = tx.prepare("SELECT id, description FROM transactions")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (id, desc) = row?;
            desc_index.entry(normalize(&desc)).or_default().push(id);
        }
    }

    // Read the Monarch CSV.
    let mut reader = csv::Reader::from_path(path)
        .with_context(|| format!("could not open {}", path.display()))?;

    let mut direct_matches = 0usize;
    let mut rules_learned = 0usize;
    let mut monarch_rows = 0usize;

    // De-dup the (pattern, category) pairs in-memory before hitting the DB —
    // the Monarch export has 1500 rows but only a few hundred unique merchants.
    let mut seen_rules: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();

    for row in reader.deserialize::<MonarchRow>() {
        let row = row.context("malformed Monarch row")?;
        monarch_rows += 1;

        let category = row.category.trim();
        if category.is_empty() {
            continue; // skip rows Monarch didn't categorize
        }

        // --- Pass A: direct exact-match categorization
        let key = normalize(&row.original_statement);
        if let Some(ids) = desc_index.get(&key) {
            for id in ids {
                // Only set category if currently NULL — never overwrite
                // a category that's already been assigned (manual edits stay safe).
                let changed = tx.execute(
                    "UPDATE transactions SET category = ?1
                     WHERE id = ?2 AND category IS NULL",
                    params![category, id],
                )?;
                direct_matches += changed;
            }
        }

        // --- Pass B: learn a merchant rule
        let pattern = normalize(&row.merchant);
        if pattern.is_empty() {
            continue;
        }
        let key = (pattern.clone(), category.to_string());
        if seen_rules.insert(key) {
            // INSERT OR IGNORE → the UNIQUE(pattern, category) constraint silently
            // drops a rule we've already learned in a previous seed run.
            let changed = tx.execute(
                "INSERT OR IGNORE INTO merchant_rules (pattern, category, origin)
                 VALUES (?1, ?2, 'monarch_seed')",
                params![pattern, category],
            )?;
            rules_learned += changed;
        }
    }

    tx.commit()?;

    Ok(SeedReport {
        direct_matches,
        rules_learned,
        monarch_rows,
    })
}

// --- The apply pass -------------------------------------------------------

pub struct ApplyReport {
    pub categorized: usize,
    pub still_uncategorized: usize,
}

// Report shape for `add_rule` — distinguishes how each match was affected.
pub struct AddRuleReport {
    pub rule_was_new: bool,
    pub newly_categorized: usize, // matching rows that had category = NULL
    pub reclassified: usize,      // matching rows whose category was different
    pub already_correct: usize,   // matching rows already on the target category
}

// Add a rule and apply it. Mirrors Monarch's "apply to all" semantics:
// every matching transaction ends up on the new category, regardless of
// what they were before. This is an explicit user action, so overwriting
// is the right default.
pub fn add_rule(
    conn: &mut Connection,
    pattern: &str,
    category: &str,
) -> Result<AddRuleReport> {
    let pattern = pattern.trim().to_lowercase();
    let category = category.trim();
    if pattern.is_empty() || category.is_empty() {
        anyhow::bail!("pattern and category must both be non-empty");
    }

    // Wrap the whole thing in a transaction so the rule table and the
    // transaction table can't get out of sync if anything fails partway.
    let tx = conn.transaction()?;

    // 1) Remove any conflicting rule for this pattern. If the user previously
    //    said "public storage -> Cloud Data" and now says "-> Storage", the
    //    old mapping should disappear so future runs don't reapply it.
    tx.execute(
        "DELETE FROM merchant_rules WHERE pattern = ?1 AND category != ?2",
        params![pattern, category],
    )?;

    // 2) Insert the new rule. INSERT OR IGNORE makes re-adding the same
    //    (pattern, category) pair a no-op.
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO merchant_rules (pattern, category, origin)
         VALUES (?1, ?2, 'manual')",
        params![pattern, category],
    )?;

    // 3) Count what we're about to do, so the user gets a useful report.
    //    Three buckets: rows where category IS NULL, where category differs,
    //    where category is already correct (UPDATE will skip these).
    let newly_categorized: usize = tx.query_row(
        "SELECT COUNT(*) FROM transactions
          WHERE INSTR(LOWER(description), ?1) > 0
            AND category IS NULL",
        params![pattern],
        |r| r.get(0),
    )?;
    let reclassified: usize = tx.query_row(
        "SELECT COUNT(*) FROM transactions
          WHERE INSTR(LOWER(description), ?1) > 0
            AND category IS NOT NULL
            AND category != ?2",
        params![pattern, category],
        |r| r.get(0),
    )?;
    let already_correct: usize = tx.query_row(
        "SELECT COUNT(*) FROM transactions
          WHERE INSTR(LOWER(description), ?1) > 0
            AND category = ?2",
        params![pattern, category],
        |r| r.get(0),
    )?;

    // 4) Apply. We restrict to rows that actually need updating so we
    //    don't pay write amplification on the no-op cases.
    tx.execute(
        "UPDATE transactions
            SET category = ?1
          WHERE INSTR(LOWER(description), ?2) > 0
            AND (category IS NULL OR category != ?1)",
        params![category, pattern],
    )?;

    tx.commit()?;

    Ok(AddRuleReport {
        rule_was_new: inserted == 1,
        newly_categorized,
        reclassified,
        already_correct,
    })
}

// Set the category on a single transaction by id. Always overwrites — this
// represents an explicit user action ("this row should be X"), so the
// previous value is intentionally clobbered.
pub fn categorize_one(conn: &Connection, id: i64, category: &str) -> Result<bool> {
    let category = category.trim();
    if category.is_empty() {
        anyhow::bail!("category must be non-empty");
    }
    let changed = conn.execute(
        "UPDATE transactions SET category = ?1 WHERE id = ?2",
        params![category, id],
    )?;
    Ok(changed == 1)
}

pub fn apply_rules(conn: &mut Connection) -> Result<ApplyReport> {
    // Load all rules into a Vec we can scan in Rust. For ~hundreds of rules
    // and thousands of transactions this is trivially fast.
    let rules: Vec<(String, String)> = {
        let mut stmt = conn.prepare("SELECT pattern, category FROM merchant_rules")?;
        let mapped = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // Load uncategorized transactions: (id, description).
    let candidates: Vec<(i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, description FROM transactions WHERE category IS NULL",
        )?;
        let mapped = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let tx = conn.transaction()?;
    let mut categorized = 0usize;

    for (id, description) in &candidates {
        let desc_lower = description.to_lowercase();
        // Find the longest pattern that's contained in this description.
        // Longest wins because more specific should beat more general
        // (e.g. "amazon prime video" beats "amazon").
        let best = rules
            .iter()
            .filter(|(pattern, _)| desc_lower.contains(pattern))
            .max_by_key(|(pattern, _)| pattern.len());

        if let Some((_, category)) = best {
            tx.execute(
                "UPDATE transactions SET category = ?1 WHERE id = ?2",
                params![category, id],
            )?;
            categorized += 1;
        }
    }

    tx.commit()?;

    Ok(ApplyReport {
        categorized,
        still_uncategorized: candidates.len() - categorized,
    })
}

const CATEGORY_EMOJI: &[(&str, &str)] = &[
    // Income
    ("Paychecks", "💵"),
    ("Interest", "💸"),
    ("Business Income", "💰"),
    ("Other Income", "💰"),

    // Gifts & Donations
    ("Charity", "🎗"),
    ("Gifts", "🎁"),

    // Auto & Transport
    ("Auto Payment", "🚗"),
    ("Public Transit", "🚃"),
    ("Gas", "⛽️"),
    ("Auto Maintenance", "🔧"),
    ("Parking & Tolls", "🏢"),
    ("Taxi & Ride Shares", "🚕"),

    // Housing
    ("Mortgage", "🏠"),
    ("Rent", "🏠"),
    ("Home Improvement", "🔨"),

    // Bills & Utilities
    ("Garbage", "🗑"),
    ("Water", "💧"),
    ("Gas & Electric", "⚡️"),
    ("Internet & Cable", "🌐"),
    ("Phone", "📱"),

    // Food & Dining
    ("Groceries", "🍏"),
    ("Restaurants & Bars", "🍽"),
    ("Coffee Shops", "☕️"),

    // Travel & Lifestyle
    ("Travel & Vacation", "🏝"),
    ("Entertainment & Recreation", "🎥"),
    ("Personal", "👑"),
    ("Pets", "🐶"),
    ("Fun Money", "😜"),

    // Shopping
    ("Shopping", "🛍"),
    ("Clothing", "👕"),
    ("Furniture & Housewares", "🪑"),
    ("Electronics", "🖥"),

    // Children
    ("Child Care", "👶"),
    ("Child Activities", "⚽️"),

    // Education
    ("Student Loans", "🎓"),
    ("Education", "🏫"),

    // Health & Wellness
    ("Medical", "💊"),
    ("Dentist", "🦷"),
    ("Fitness", "💪"),

    // Financial
    ("Loan Repayment", "💰"),
    ("Financial & Legal Services", "🗄"),
    ("Financial Fees", "🏦"),
    ("Cash & ATM", "🏧"),
    ("Insurance", "☂️"),
    ("Taxes", "🏛️"),

    // Other
    ("Uncategorized", "❓"),
    ("Check", "💸"),
    ("Miscellaneous", "💲"),

    // Business
    ("Advertising & Promotion", "📣"),
    ("Business Utilities & Communication", "📞"),
    ("Employee Wages & Contract Labor", "💵"),
    ("Business Travel & Meals", "🍴"),
    ("Business Auto Expenses", "🚖"),
    ("Business Insurance", "📁"),
    ("Office Supplies & Expenses", "📎"),
    ("Office Rent", "🏢"),
    ("Postage & Shipping", "📦"),

    // Transfers
    ("Transfer", "🔁"),
    ("Credit Card Payment", "💳"),
    ("Balance Adjustments", "⚖️"),
];
