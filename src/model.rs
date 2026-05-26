// The canonical, internal representation of a transaction.
// Both Wells Fargo and Amex CSV rows get normalized into this shape
// before they touch the database.

use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct Transaction {
    pub source: Source,
    pub txn_date: chrono::NaiveDate, // date with no timezone — banks deal in calendar dates, not instants
    pub description: String,
    pub amount_cents: i64,           // negative = money out, positive = money in
    pub card_member: Option<String>, // Amex-only; None for Wells
}

// An enum is Rust's way of saying "exactly one of these variants."
// Much safer than a string: the compiler won't let us forget a case
// when we match on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    WellsFargo,
    Amex,
}

impl Source {
    // Returns the string we store in the DB. Centralizing this means
    // we can't accidentally write "wells_fargo" in one place and
    // "wellsfargo" in another.
    pub fn as_str(self) -> &'static str {
        match self {
            Source::WellsFargo => "wells_fargo",
            Source::Amex => "amex",
        }
    }
}

impl Transaction {
    // The dedup fingerprint. Hashing (source|date|cents|description) gives
    // each economically-distinct row a stable ID we can put a UNIQUE
    // constraint on. SQLite's INSERT OR IGNORE then drops duplicates for free.
    pub fn fingerprint(&self) -> String {
        let mut hasher = Sha256::new();
        // We use `|` as a separator — it doesn't appear in any of the
        // input fields, so there's no risk of two different rows hashing
        // the same way by accident.
        let payload = format!(
            "{}|{}|{}|{}",
            self.source.as_str(),
            self.txn_date.format("%Y-%m-%d"),
            self.amount_cents,
            self.description,
        );
        hasher.update(payload.as_bytes());
        hex::encode(hasher.finalize())
    }
}
