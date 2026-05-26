// TUI binary. Runs against the same finance.db as the main CLI binary.
// Structure mirrors what a future axum server would look like:
//   1. Open a DB connection via the library
//   2. Pull data via library query functions (no SQL in this file!)
//   3. Hand the data to a presentation layer (here: ratatui; there: serde_json)
//
// Run with:  cargo run --bin tui
//        or: cargo run --bin tui -- /path/to/other.db

use std::{io, time::Duration};

use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, TableState, Tabs},
};

use finance_tracker_api::db;

// --- App state ------------------------------------------------------------

// Which screen the user is looking at. Adding a screen later means:
// (1) add a variant here, (2) handle it in `render` and `cur_state`.
#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Transactions,
    Categories,
}

// All in-memory state for the running TUI. Loaded once on startup; we don't
// re-query SQLite on every keypress because at our scale (~1000 rows) it's
// trivial to keep it all in memory and dirt-simple to reason about.
struct App {
    screen: Screen,
    transactions: Vec<db::TransactionRow>,
    txn_state: TableState,
    categories: Vec<db::CategorySummary>,
    cat_state: TableState,
    should_quit: bool,
}

impl App {
    fn new(conn: &rusqlite::Connection) -> Result<Self> {
        let transactions = db::list_transactions(conn)?;
        let categories = db::summary_by_category(conn)?;

        // Pre-select the first row of each table so highlight is visible
        // from the moment the TUI opens.
        let mut txn_state = TableState::default();
        if !transactions.is_empty() {
            txn_state.select(Some(0));
        }
        let mut cat_state = TableState::default();
        if !categories.is_empty() {
            cat_state.select(Some(0));
        }

        Ok(Self {
            screen: Screen::Transactions,
            transactions,
            txn_state,
            categories,
            cat_state,
            should_quit: false,
        })
    }

    // Returns whichever (TableState, row_count) pair belongs to the active
    // screen. Centralizing this means scroll/quit logic doesn't need to
    // know which screen it's operating on.
    fn cur_state(&mut self) -> (&mut TableState, usize) {
        match self.screen {
            Screen::Transactions => (&mut self.txn_state, self.transactions.len()),
            Screen::Categories => (&mut self.cat_state, self.categories.len()),
        }
    }

    fn move_by(&mut self, delta: i64) {
        let (state, len) = self.cur_state();
        if len == 0 {
            return;
        }
        let cur = state.selected().unwrap_or(0) as i64;
        let next = (cur + delta).clamp(0, len as i64 - 1) as usize;
        state.select(Some(next));
    }

    fn select_first(&mut self) {
        let (state, len) = self.cur_state();
        if len > 0 {
            state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        let (state, len) = self.cur_state();
        if len > 0 {
            state.select(Some(len - 1));
        }
    }

    // The single dispatch point for every keystroke. Keeps key bindings
    // discoverable — to add or change a shortcut, you edit one match arm.
    fn handle_key(&mut self, key: KeyCode) {
        match key {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Tab | KeyCode::BackTab => {
                self.screen = match self.screen {
                    Screen::Transactions => Screen::Categories,
                    Screen::Categories => Screen::Transactions,
                };
            }
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::PageDown => self.move_by(20),
            KeyCode::PageUp => self.move_by(-20),
            KeyCode::Char('g') | KeyCode::Home => self.select_first(),
            KeyCode::Char('G') | KeyCode::End => self.select_last(),
            _ => {}
        }
    }
}

// --- Formatting helpers ---------------------------------------------------

// `-3321` -> "-$33.21". Integer math only — no float weirdness.
fn format_amount(cents: i64) -> String {
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let frac = abs % 100;
    let sign = if cents < 0 { "-" } else { " " };
    format!("{sign}${dollars}.{frac:02}")
}

// Truncate by chars, not bytes — merchant strings can contain multi-byte
// characters and slicing by byte would panic mid-codepoint.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

// --- Rendering ------------------------------------------------------------

fn render_transactions(f: &mut Frame, area: Rect, app: &mut App) {
    let rows: Vec<Row> = app
        .transactions
        .iter()
        .map(|t| {
            let amount_style = if t.amount_cents < 0 {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            };
            let category_text = t.category.as_deref().unwrap_or("—").to_string();
            let category_style = if t.category.is_none() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(t.txn_date.clone()),
                Cell::from(t.source.clone()).style(Style::default().fg(Color::DarkGray)),
                Cell::from(truncate(&t.description, 45)),
                Cell::from(format_amount(t.amount_cents)).style(amount_style),
                Cell::from(category_text).style(category_style),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(10),  // date
        Constraint::Length(12),  // source
        Constraint::Min(20),     // description (flexes to fill)
        Constraint::Length(11),  // amount
        Constraint::Length(22),  // category
    ];
    let header = Row::new(vec!["Date", "Source", "Description", "Amount", "Category"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ")
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Transactions ({}) ", app.transactions.len())),
        );

    f.render_stateful_widget(table, area, &mut app.txn_state);
}

fn render_categories(f: &mut Frame, area: Rect, app: &mut App) {
    let rows: Vec<Row> = app
        .categories
        .iter()
        .map(|c| {
            let amount_style = if c.total_cents < 0 {
                Style::default().fg(Color::Red)
            } else {
                Style::default().fg(Color::Green)
            };
            Row::new(vec![
                Cell::from(c.category.clone()),
                Cell::from(format!("{:>5}", c.count)),
                Cell::from(format_amount(c.total_cents)).style(amount_style),
            ])
        })
        .collect();

    let widths = [
        Constraint::Min(20),
        Constraint::Length(8),
        Constraint::Length(14),
    ];
    let header = Row::new(vec!["Category", "Count", "Net"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ")
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" Categories ({}) ", app.categories.len())),
        );

    f.render_stateful_widget(table, area, &mut app.cat_state);
}

fn render(f: &mut Frame, app: &mut App) {
    // Three vertical bands: tab bar (3 high), main content (rest), status (1 high).
    let chunks = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .split(f.area());

    let selected_tab = match app.screen {
        Screen::Transactions => 0,
        Screen::Categories => 1,
    };
    let tabs = Tabs::new(vec!["Transactions", "Categories"])
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Finance Tracker "),
        )
        .select(selected_tab)
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    f.render_widget(tabs, chunks[0]);

    match app.screen {
        Screen::Transactions => render_transactions(f, chunks[1], app),
        Screen::Categories => render_categories(f, chunks[1], app),
    }

    let hints = "j/k or ↑↓ scroll · PgUp/PgDn page · g/G top/bottom · Tab switch view · q quit";
    let status = Paragraph::new(Line::from(hints)).style(Style::default().fg(Color::DarkGray));
    f.render_widget(status, chunks[2]);
}

// --- Event loop -----------------------------------------------------------

fn run(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| render(f, app))?;

        // Poll for input with a short timeout so we'd be free to redraw on
        // a tick if we ever needed to (e.g. animated spinner). At our scale,
        // 250ms is plenty responsive — humans don't notice <100ms latency.
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }
        if app.should_quit {
            return Ok(());
        }
    }
}

// --- Terminal lifecycle ---------------------------------------------------

fn main() -> Result<()> {
    let db_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "finance.db".to_string());
    let conn = db::open(&db_path)?;
    let mut app = App::new(&conn)?;

    // Enter the "alternate screen" so the TUI doesn't clobber the user's
    // scrollback. Raw mode disables line buffering so we see keys as they're
    // pressed instead of one-line-at-a-time.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    // Run, but always restore the terminal even if `run` errors.
    let result = run(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}
