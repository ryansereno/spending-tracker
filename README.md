Bank and Credit Card transaction tracker.

An attempt to clone the Monarch/ Mint finance tracking dashboard, in a simpler TUI app.

Import data via bank .csv export

## Usage

```bash
cargo build
```

### Import transactions

Download a .csv from your bank

Then import:
```bash
cargo run -- import --source wells <file.csv>
cargo run -- import --source amex  <file.csv>
```

### Categorizing

Seed categories from a Monarch .csv export (optional)
```bash
cargo run -- seed-monarch <monarch_export.csv>
```

Re-apply rules to uncategorized rows
```bash
cargo run -- categorize
```

Categorize a single transaction by id (overwrites)
```bash 
cargo run -- categorize-txn <id> "<category>"
```

Add a merchant rule (and apply it immediately)
```bash
cargo run -- rule add "<pattern>" "<category>"
```

### Run TUI app

```bash
cargo run --bin tui
# or against a different DB:
cargo run --bin tui -- /path/to/other.db
```
