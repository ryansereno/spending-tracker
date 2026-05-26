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
    widgets::{Block, Borders, Cell, Clear, List, ListItem, Paragraph, Row, Table, TableState, Tabs},
};

use finance_tracker_api::{categorize, db};

// --- App state ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Transactions,
    Categories,
}

// Where the user is "typing." Each variant owns the state needed to render
// and dispatch keystrokes for that mode. Normal-mode keys (j/k/Tab/q) only
// fire when input_mode is Normal — that's how `q` becomes "quit" in one
// context and "the letter q" in another, with no special-casing per key.
enum InputMode {
    Normal,
    Filter {
        // Stored on mode entry so Esc can revert if the user cancels.
        prior: String,
    },
    Category(CategoryModal),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ModalFocus {
    Category,
    Toggle,
    Pattern,
}

struct CategoryModal {
    target_txn_id: i64,
    category_input: String,
    apply_to_all: bool,
    pattern_input: String,
    focus: ModalFocus,
    // Errors from a failed submit live here so the modal stays open and the
    // user can fix the input without losing what they typed.
    error: Option<String>,
}

impl CategoryModal {
    fn new(target_txn_id: i64, default_pattern: String) -> Self {
        Self {
            target_txn_id,
            category_input: String::new(),
            apply_to_all: false,
            pattern_input: default_pattern,
            focus: ModalFocus::Category,
            error: None,
        }
    }

    fn cycle_focus(&mut self, forward: bool) {
        use ModalFocus::*;
        self.focus = match (self.focus, forward, self.apply_to_all) {
            (Category, true, _) => Toggle,
            (Toggle, true, true) => Pattern,
            (Toggle, true, false) => Category,
            (Pattern, true, _) => Category,
            (Category, false, true) => Pattern,
            (Category, false, false) => Toggle,
            (Toggle, false, _) => Category,
            (Pattern, false, _) => Toggle,
        };
    }
}

// In-memory snapshot of the DB. `visible` is an index-into-transactions
// list that respects the current filter; the table state's selected row
// is an index INTO visible, not into transactions directly.
struct App {
    screen: Screen,
    transactions: Vec<db::TransactionRow>,
    visible: Vec<usize>,
    txn_state: TableState,
    categories: Vec<db::CategorySummary>,
    cat_state: TableState,
    category_list: Vec<String>,
    filter: String,
    input_mode: InputMode,
    should_quit: bool,
}

impl App {
    fn new(conn: &rusqlite::Connection) -> Result<Self> {
        let transactions = db::list_transactions(conn)?;
        let categories = db::summary_by_category(conn)?;
        let category_list = db::list_categories(conn)?;

        let mut app = Self {
            screen: Screen::Transactions,
            transactions,
            visible: Vec::new(),
            txn_state: TableState::default(),
            categories,
            cat_state: TableState::default(),
            category_list,
            filter: String::new(),
            input_mode: InputMode::Normal,
            should_quit: false,
        };
        app.recompute_visible();
        if !app.visible.is_empty() {
            app.txn_state.select(Some(0));
        }
        if !app.categories.is_empty() {
            app.cat_state.select(Some(0));
        }
        Ok(app)
    }

    // Rebuild `visible` from `filter`. Called whenever either changes.
    // Also clamps the selection so we don't end up pointing past the end
    // of a now-shorter visible list.
    fn recompute_visible(&mut self) {
        let q = self.filter.to_lowercase();
        self.visible = self
            .transactions
            .iter()
            .enumerate()
            .filter(|(_, t)| q.is_empty() || t.description.to_lowercase().contains(&q))
            .map(|(i, _)| i)
            .collect();

        if self.visible.is_empty() {
            self.txn_state.select(None);
        } else if let Some(sel) = self.txn_state.selected() {
            if sel >= self.visible.len() {
                self.txn_state.select(Some(self.visible.len() - 1));
            }
        } else {
            self.txn_state.select(Some(0));
        }
    }

    // After any mutation (categorize a row, add a rule), re-pull everything
    // from the DB and try to keep the user on the same transaction they were
    // looking at. If that row dropped out of the visible set (e.g. they had
    // an "uncategorized only" mental filter and just categorized it), fall
    // back to the top.
    fn reload(&mut self, conn: &rusqlite::Connection) -> Result<()> {
        let prev_id = self.selected_txn_id();
        self.transactions = db::list_transactions(conn)?;
        self.categories = db::summary_by_category(conn)?;
        self.category_list = db::list_categories(conn)?;
        self.recompute_visible();
        if let Some(id) = prev_id {
            if let Some(pos) = self
                .visible
                .iter()
                .position(|&i| self.transactions[i].id == id)
            {
                self.txn_state.select(Some(pos));
            }
        }
        Ok(())
    }

    fn selected_txn_id(&self) -> Option<i64> {
        let pos = self.txn_state.selected()?;
        let idx = self.visible.get(pos)?;
        Some(self.transactions[*idx].id)
    }

    fn selected_txn(&self) -> Option<&db::TransactionRow> {
        let pos = self.txn_state.selected()?;
        let idx = self.visible.get(pos)?;
        self.transactions.get(*idx)
    }

    // Navigation only applies to whichever table is on screen.
    fn cur_state(&mut self) -> (&mut TableState, usize) {
        match self.screen {
            Screen::Transactions => (&mut self.txn_state, self.visible.len()),
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
}

// --- Pattern derivation ---------------------------------------------------

// Best-guess starting pattern for "Apply to all matching." Filters out the
// obvious noise (TST* prefixes, all-digit tokens, transaction IDs) and
// takes the first few remaining word-ish tokens. Always editable.
fn derive_pattern(description: &str) -> String {
    let cleaned: Vec<&str> = description
        .split_whitespace()
        .filter(|tok| {
            !tok.contains('*')
                && !tok
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '-' || c == '#')
        })
        .take(3)
        .collect();
    let result = cleaned.join(" ").to_lowercase();
    if result.is_empty() {
        description.to_lowercase().chars().take(20).collect()
    } else {
        result
    }
}

// --- Key dispatch ---------------------------------------------------------

// Top-level dispatch routes to a mode-specific handler. Each handler is
// free to assume it owns the input — Normal-mode bindings never fire while
// the user is typing into the filter or modal.
fn handle_key(app: &mut App, conn: &mut rusqlite::Connection, key: KeyCode) -> Result<()> {
    match app.input_mode {
        InputMode::Normal => handle_normal(app, key),
        InputMode::Filter { .. } => handle_filter(app, key),
        InputMode::Category(_) => handle_category(app, conn, key)?,
    }
    Ok(())
}

fn handle_normal(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
        KeyCode::Tab | KeyCode::BackTab => {
            app.screen = match app.screen {
                Screen::Transactions => Screen::Categories,
                Screen::Categories => Screen::Transactions,
            };
        }
        KeyCode::Char('j') | KeyCode::Down => app.move_by(1),
        KeyCode::Char('k') | KeyCode::Up => app.move_by(-1),
        KeyCode::PageDown => app.move_by(20),
        KeyCode::PageUp => app.move_by(-20),
        KeyCode::Char('g') | KeyCode::Home => app.select_first(),
        KeyCode::Char('G') | KeyCode::End => app.select_last(),
        KeyCode::Char('/') if app.screen == Screen::Transactions => {
            app.input_mode = InputMode::Filter {
                prior: app.filter.clone(),
            };
        }
        KeyCode::Char('c') if app.screen == Screen::Transactions => {
            if let Some(txn) = app.selected_txn() {
                let modal = CategoryModal::new(txn.id, derive_pattern(&txn.description));
                app.input_mode = InputMode::Category(modal);
            }
        }
        _ => {}
    }
}

fn handle_filter(app: &mut App, key: KeyCode) {
    match key {
        KeyCode::Esc => {
            // Revert to whatever filter was active when the user pressed /.
            let prior = if let InputMode::Filter { prior } = &app.input_mode {
                prior.clone()
            } else {
                String::new()
            };
            app.filter = prior;
            app.input_mode = InputMode::Normal;
            app.recompute_visible();
        }
        KeyCode::Enter => {
            app.input_mode = InputMode::Normal;
        }
        KeyCode::Backspace => {
            app.filter.pop();
            app.recompute_visible();
        }
        KeyCode::Char(c) => {
            app.filter.push(c);
            app.recompute_visible();
        }
        _ => {}
    }
}

fn handle_category(app: &mut App, conn: &mut rusqlite::Connection, key: KeyCode) -> Result<()> {
    // Sanity check — caller already routed us here, but the borrow checker
    // wants the discriminant proven each time we touch it.
    if !matches!(app.input_mode, InputMode::Category(_)) {
        return Ok(());
    }

    match key {
        KeyCode::Esc => {
            app.input_mode = InputMode::Normal;
            return Ok(());
        }
        KeyCode::Tab => {
            if let InputMode::Category(m) = &mut app.input_mode {
                m.cycle_focus(true);
            }
            return Ok(());
        }
        KeyCode::BackTab => {
            if let InputMode::Category(m) = &mut app.input_mode {
                m.cycle_focus(false);
            }
            return Ok(());
        }
        KeyCode::Char(' ') => {
            // Space on the toggle row flips the checkbox; everywhere else
            // it's a literal space character.
            if let InputMode::Category(m) = &mut app.input_mode {
                if m.focus == ModalFocus::Toggle {
                    m.apply_to_all = !m.apply_to_all;
                    return Ok(());
                }
            }
            // fall through to text-input handling below
        }
        KeyCode::Enter => {
            // Pull everything we need out of the modal up front so we can
            // drop the &mut borrow before calling the library.
            let (target_id, category, pattern_opt) = {
                let InputMode::Category(m) = &app.input_mode else {
                    return Ok(());
                };
                let category = m.category_input.trim().to_string();
                if category.is_empty() {
                    if let InputMode::Category(m) = &mut app.input_mode {
                        m.error = Some("Category cannot be empty".into());
                    }
                    return Ok(());
                }
                let pattern_opt = if m.apply_to_all {
                    let p = m.pattern_input.trim().to_string();
                    if p.is_empty() {
                        if let InputMode::Category(m) = &mut app.input_mode {
                            m.error = Some("Pattern required when 'Apply to all' is on".into());
                        }
                        return Ok(());
                    }
                    Some(p)
                } else {
                    None
                };
                (m.target_txn_id, category, pattern_opt)
            };

            let result = match pattern_opt {
                Some(p) => categorize::add_rule(conn, &p, &category).map(|_| ()),
                None => categorize::categorize_one(conn, target_id, &category).map(|_| ()),
            };

            match result {
                Ok(()) => {
                    app.input_mode = InputMode::Normal;
                    app.reload(conn)?;
                }
                Err(e) => {
                    if let InputMode::Category(m) = &mut app.input_mode {
                        m.error = Some(format!("{e}"));
                    }
                }
            }
            return Ok(());
        }
        _ => {}
    }

    // Text-input dispatch for whichever field has focus.
    if let InputMode::Category(m) = &mut app.input_mode {
        m.error = None;
        let buf = match m.focus {
            ModalFocus::Category => &mut m.category_input,
            ModalFocus::Pattern => &mut m.pattern_input,
            ModalFocus::Toggle => return Ok(()),
        };
        match key {
            KeyCode::Char(c) => buf.push(c),
            KeyCode::Backspace => {
                buf.pop();
            }
            _ => {}
        }
    }

    Ok(())
}

// --- Formatting helpers ---------------------------------------------------

fn format_amount(cents: i64) -> String {
    let abs = cents.unsigned_abs();
    let dollars = abs / 100;
    let frac = abs % 100;
    let sign = if cents < 0 { "-" } else { " " };
    format!("{sign}${dollars}.{frac:02}")
}

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
        .visible
        .iter()
        .map(|&i| {
            let t = &app.transactions[i];
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
        Constraint::Length(10),
        Constraint::Length(12),
        Constraint::Min(20),
        Constraint::Length(11),
        Constraint::Length(22),
    ];
    let header = Row::new(vec!["Date", "Source", "Description", "Amount", "Category"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let title = if app.filter.is_empty() {
        format!(" Transactions ({}) ", app.transactions.len())
    } else {
        format!(
            " Transactions ({} of {}, filter: \"{}\") ",
            app.visible.len(),
            app.transactions.len(),
            app.filter,
        )
    };

    let table = Table::new(rows, widths)
        .header(header)
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ")
        .block(Block::default().borders(Borders::ALL).title(title));

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

// Centered popup rect, sized as a percentage of the parent area.
fn centered_rect(area: Rect, pct_x: u16, pct_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - pct_y) / 2),
        Constraint::Percentage(pct_y),
        Constraint::Percentage((100 - pct_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_x) / 2),
        Constraint::Percentage(pct_x),
        Constraint::Percentage((100 - pct_x) / 2),
    ])
    .split(vertical[1])[1]
}

fn render_category_modal(
    f: &mut Frame,
    area: Rect,
    modal: &CategoryModal,
    category_list: &[String],
) {
    // Clear the area first so we don't paint on top of garbage.
    f.render_widget(Clear, area);
    let block = Block::default().borders(Borders::ALL).title(" Categorize ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let chunks = Layout::vertical([
        Constraint::Length(1), // category input
        Constraint::Length(1), // gap
        Constraint::Length(6), // suggestions
        Constraint::Length(1), // toggle
        Constraint::Length(1), // pattern input (rendered blank when toggle off)
        Constraint::Length(1), // gap
        Constraint::Min(1),    // hint / error
    ])
    .split(inner);

    let cursor = |focused: bool| if focused { "█" } else { "" };
    let focus_style = |focused: bool| {
        if focused {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        }
    };

    // Category input
    let cat_line = format!(
        "Category: {}{}",
        modal.category_input,
        cursor(modal.focus == ModalFocus::Category)
    );
    f.render_widget(
        Paragraph::new(cat_line).style(focus_style(modal.focus == ModalFocus::Category)),
        chunks[0],
    );

    // Suggestions — substring match against category_list (case-insensitive)
    let q = modal.category_input.to_lowercase();
    let suggestions: Vec<ListItem> = category_list
        .iter()
        .filter(|c| q.is_empty() || c.to_lowercase().contains(&q))
        .take(5)
        .map(|c| ListItem::new(c.as_str()))
        .collect();
    let suggestions_widget = List::new(suggestions).block(
        Block::default()
            .borders(Borders::TOP)
            .title("existing categories")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(suggestions_widget, chunks[2]);

    // Toggle
    let mark = if modal.apply_to_all { "x" } else { " " };
    let toggle_line = format!("[{mark}] Apply to all matching (creates rule)");
    f.render_widget(
        Paragraph::new(toggle_line).style(focus_style(modal.focus == ModalFocus::Toggle)),
        chunks[3],
    );

    // Pattern input — render only when toggle is on, otherwise leave blank
    // (still occupying its row so the layout doesn't jump).
    if modal.apply_to_all {
        let pat_line = format!(
            "Pattern:  {}{}",
            modal.pattern_input,
            cursor(modal.focus == ModalFocus::Pattern)
        );
        f.render_widget(
            Paragraph::new(pat_line).style(focus_style(modal.focus == ModalFocus::Pattern)),
            chunks[4],
        );
    }

    // Hint / error
    let (hint, style) = match &modal.error {
        Some(err) => (
            format!("Error: {err}"),
            Style::default().fg(Color::Red),
        ),
        None => (
            "Tab: next field · Space: toggle · Enter: apply · Esc: cancel".to_string(),
            Style::default().fg(Color::DarkGray),
        ),
    };
    f.render_widget(Paragraph::new(hint).style(style), chunks[6]);
}

fn render(f: &mut Frame, app: &mut App) {
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

    // Status bar — content depends on input mode so the user always sees
    // what they can do RIGHT NOW.
    let status_line = match &app.input_mode {
        InputMode::Filter { .. } => Line::from(format!("Filter: {}█  (Enter commits · Esc cancels)", app.filter)),
        _ => Line::from(
            "j/k or ↑↓ scroll · / filter · c categorize · Tab switch view · q quit",
        ),
    };
    let status_style = match app.input_mode {
        InputMode::Filter { .. } => Style::default().fg(Color::Yellow),
        _ => Style::default().fg(Color::DarkGray),
    };
    f.render_widget(Paragraph::new(status_line).style(status_style), chunks[2]);

    // Modal renders LAST so it draws on top of the table beneath it.
    if let InputMode::Category(modal) = &app.input_mode {
        let area = centered_rect(f.area(), 60, 55);
        render_category_modal(f, area, modal, &app.category_list);
    }
}

// --- Event loop -----------------------------------------------------------

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    conn: &mut rusqlite::Connection,
) -> Result<()> {
    loop {
        terminal.draw(|f| render(f, app))?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    handle_key(app, conn, key.code)?;
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
    let mut conn = db::open(&db_path)?;
    let mut app = App::new(&conn)?;

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;

    let result = run(&mut terminal, &mut app, &mut conn);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}
