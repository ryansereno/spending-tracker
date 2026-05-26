// CSV parsing for both Wells Fargo and Amex exports.
// Each bank has its own row struct (Serde maps columns by name via
// #[serde(rename)]), then we convert into the shared `Transaction` model.

use anyhow::{Context, Result, anyhow};
use chrono::NaiveDate;
use rusqlite::Connection;
use serde::Deserialize;
use std::path::Path;

use crate::db;
use crate::model::{Source, Transaction};

// --- Bank-specific row shapes ---------------------------------------------

// Wells Fargo headers are SCREAMING CASE. We only declare the columns we
// care about — Serde happily ignores extras (CHECK #, STATUS) as long as
// we tell the CSV reader to be `flexible` about the trailing empty field.
#[derive(Debug, Deserialize)]
struct WellsRow {
    #[serde(rename = "DATE")]
    date: String,
    #[serde(rename = "DESCRIPTION")]
    description: String,
    #[serde(rename = "AMOUNT")]
    amount: String,
}

#[derive(Debug, Deserialize)]
struct AmexRow {
    #[serde(rename = "Date")]
    date: String,
    #[serde(rename = "Description")]
    description: String,
    #[serde(rename = "Card Member")]
    card_member: String,
    #[serde(rename = "Amount")]
    amount: String,
}

// --- Parsing helpers ------------------------------------------------------

// Both banks use MM/DD/YYYY. Centralizing this means a future format change
// only touches one place.
fn parse_date(s: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(s.trim(), "%m/%d/%Y")
        .with_context(|| format!("could not parse date {s:?}"))
}

// Money parsing: take a string like "-33.21" or "11.35", return integer cents.
// Going through f64 is fine for ≤2-decimal bank exports; the `.round() as i64`
// at the end avoids the classic 33.21 * 100 = 3320.999... pitfall.
fn parse_cents(s: &str) -> Result<i64> {
    let cleaned = s.trim().replace(',', ""); // tolerate "1,234.56"
    let value: f64 = cleaned
        .parse()
        .with_context(|| format!("could not parse amount {s:?}"))?;
    Ok((value * 100.0).round() as i64)
}

// --- The main import loop -------------------------------------------------

pub fn run_import(path: &Path, source: Source, conn: &Connection) -> Result<(usize, usize)> {
    // `flexible(true)` lets Wells's empty `CHECK #` column not blow up parsing.
    let mut reader = csv::ReaderBuilder::new()
        .flexible(true)
        .from_path(path)
        .with_context(|| format!("could not open {}", path.display()))?;

    let mut inserted = 0usize;
    let mut skipped = 0usize;

    // We dispatch on source once, outside the loop, so the per-row work is tight.
    // `match` here returns... well, nothing — each arm runs its own loop.
    match source {
        Source::WellsFargo => {
            for row in reader.deserialize::<WellsRow>() {
                let row = row.context("malformed Wells Fargo row")?;
                let txn = Transaction {
                    source,
                    txn_date: parse_date(&row.date)?,
                    description: row.description.trim().to_string(),
                    // Wells already uses negative = charge, positive = payment.
                    amount_cents: parse_cents(&row.amount)?,
                    card_member: None,
                };
                if db::insert_transaction(conn, &txn)? {
                    inserted += 1;
                } else {
                    skipped += 1;
                }
            }
        }
        Source::Amex => {
            for row in reader.deserialize::<AmexRow>() {
                let row = row.context("malformed Amex row")?;
                let txn = Transaction {
                    source,
                    txn_date: parse_date(&row.date)?,
                    description: row.description.trim().to_string(),
                    // Amex publishes charges as POSITIVE. Flip the sign so our
                    // DB convention (negative = money out) holds across sources.
                    amount_cents: -parse_cents(&row.amount)?,
                    card_member: Some(row.card_member.trim().to_string()),
                };
                if db::insert_transaction(conn, &txn)? {
                    inserted += 1;
                } else {
                    skipped += 1;
                }
            }
        }
    }

    Ok((inserted, skipped))
}

// Parse the user's `--source` flag. clap could do this via #[derive(ValueEnum)],
// but doing it by hand keeps the model-layer enum dependency-free.
pub fn parse_source(s: &str) -> Result<Source> {
    match s.to_ascii_lowercase().as_str() {
        "wells" | "wells_fargo" | "wellsfargo" => Ok(Source::WellsFargo),
        "amex" | "american_express" => Ok(Source::Amex),
        other => Err(anyhow!("unknown source {other:?} (use 'wells' or 'amex')")),
    }
}
