// Binary entrypoint. The `mod` declarations below tell the compiler to
// pull in src/db.rs, src/csv_import.rs, src/model.rs as child modules.

mod categorize;
mod csv_import;
mod db;
mod model;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

// clap's `derive` API turns this struct into a CLI parser.
// `#[command(...)]` adds metadata you see in --help.
#[derive(Parser)]
#[command(name = "finance-tracker-api", about = "Personal finance ingestion")]
struct Cli {
    // Path to the SQLite database file. Defaults to ./finance.db.
    #[arg(long, default_value = "finance.db", global = true)]
    db: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    // `finance-tracker-api import --source wells <file>`
    Import {
        #[arg(long)]
        source: String,
        file: PathBuf,
    },
    // `finance-tracker-api seed-monarch <file>`
    // Two-pass seed: (A) directly categorize any transactions whose normalized
    // description exactly matches a Monarch row's Original Statement;
    // (B) learn (merchant -> category) rules for future imports.
    SeedMonarch { file: PathBuf },
    // `finance-tracker-api categorize`
    // Apply merchant_rules to all uncategorized transactions.
    Categorize,
    // `finance-tracker-api categorize-txn <id> <category>`
    // Set category on a single transaction (always overwrites).
    CategorizeTxn { id: i64, category: String },
    // `finance-tracker-api rule add <pattern> <category>`
    // Nested subcommand — leaves room for `rule list` / `rule remove` later.
    Rule {
        #[command(subcommand)]
        action: RuleAction,
    },
}

#[derive(Subcommand)]
enum RuleAction {
    // Add a rule and immediately apply it to all uncategorized matches.
    Add { pattern: String, category: String },
}

// Returning `Result<()>` from main lets us use `?` throughout and get a
// reasonable error message on failure. anyhow renders the whole chain.
fn main() -> Result<()> {
    let cli = Cli::parse();
    let db_path = cli
        .db
        .to_str()
        .expect("db path must be valid UTF-8");
    let mut conn = db::open(db_path)?;

    match cli.command {
        Command::Import { source, file } => {
            let source = csv_import::parse_source(&source)?;
            let (inserted, skipped) = csv_import::run_import(&file, source, &conn)?;
            println!(
                "Imported {inserted} new rows, skipped {skipped} duplicates from {}",
                file.display()
            );
            // Auto-categorize after every import. Applying rules to all
            // uncategorized (not just newly-inserted) rows is slightly broader
            // than necessary but trivially fast at our scale and catches any
            // rows missed before a recent rule was added.
            let report = categorize::apply_rules(&mut conn)?;
            println!(
                "Auto-categorized {} transactions; {} still uncategorized",
                report.categorized, report.still_uncategorized,
            );
        }
        Command::SeedMonarch { file } => {
            let report = categorize::seed_from_monarch(&file, &mut conn)?;
            println!(
                "Seeded from {} ({} rows): {} transactions directly categorized, {} new merchant rules learned",
                file.display(),
                report.monarch_rows,
                report.direct_matches,
                report.rules_learned,
            );
        }
        Command::Categorize => {
            let report = categorize::apply_rules(&mut conn)?;
            println!(
                "Categorized {} transactions; {} still uncategorized",
                report.categorized, report.still_uncategorized,
            );
        }
        Command::CategorizeTxn { id, category } => {
            let updated = categorize::categorize_one(&conn, id, &category)?;
            if updated {
                println!("Transaction {id} categorized as {category:?}");
            } else {
                println!("No transaction with id {id}");
            }
        }
        Command::Rule {
            action: RuleAction::Add { pattern, category },
        } => {
            let report = categorize::add_rule(&mut conn, &pattern, &category)?;
            let rule_status = if report.rule_was_new { "added new rule" } else { "rule already existed" };
            println!(
                "{rule_status}: {pattern:?} -> {category:?}",
            );
            println!(
                "  {} newly categorized, {} reclassified from another category, {} already correct",
                report.newly_categorized, report.reclassified, report.already_correct,
            );
        }
    }

    Ok(())
}
