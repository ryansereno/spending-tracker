// Library root. Re-exports the core modules so that any frontend
// (the current CLI in main.rs, a future TUI, a future axum API) can
// just `use finance_tracker_api::{db, categorize, ...};`.
//
// The rule of thumb: anything that touches data or implements business
// logic lives here. Anything that's presentation — argv parsing,
// terminal rendering, HTTP routes — lives in its own binary file.

pub mod categorize;
pub mod csv_import;
pub mod db;
pub mod model;
