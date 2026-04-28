use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Stdout},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, UNIX_EPOCH},
};
use walkdir::WalkDir;

const INDEX_FILE: &str = ".booksearch_index.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BookEntry {
    path: String,
    size: u64,
    mtime: u64,
    title: String,
    author: String,
    metadata: BTreeMap<String, String>,
    raw: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    books: BTreeMap<String, BookEntry>,
}

fn mtime_secs(p: &Path) -> u64 {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn parse_ebook_meta(out: &str) -> (String, String, BTreeMap<String, String>) {
    let mut title = String::new();
    let mut author = String::new();
    let mut map = BTreeMap::new();
    for line in out.lines() {
        if let Some(idx) = line.find(':') {
            let (k, v) = line.split_at(idx);
            let key = k.trim().to_string();
            let val = v[1..].trim().to_string();
            if key.is_empty() {
                continue;
            }
            match key.to_ascii_lowercase().as_str() {
                "title" => title = val.clone(),
                "author(s)" | "authors" | "author" => author = val.clone(),
                _ => {}
            }
            map.insert(key, val);
        }
    }
    (title, author, map)
}

fn extract_metadata(path: &Path) -> BookEntry {
    let mut entry = BookEntry::default();
    let output = Command::new("ebook-meta").arg(path).output();
    if let Ok(out) = output {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).to_string();
            let (title, author, map) = parse_ebook_meta(&s);
            entry.title = title;
            entry.author = author;
            entry.metadata = map;
            entry.raw = s;
        }
    }
    entry
}

fn build_or_update_index(root: &Path) -> Result<Index> {
    let index_path = root.join(INDEX_FILE);
    let mut index: Index = if index_path.exists() {
        let data = fs::read_to_string(&index_path)?;
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        Index::default()
    };

    let mut found: Vec<(String, u64, u64, PathBuf)> = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        if p.extension().and_then(|e| e.to_str()).map(|s| s.eq_ignore_ascii_case("epub")) != Some(true) {
            continue;
        }
        let rel = p.strip_prefix(root).unwrap_or(p).to_string_lossy().to_string();
        let md = match fs::metadata(p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        found.push((rel, md.len(), mtime_secs(p), p.to_path_buf()));
    }

    let found_keys: std::collections::HashSet<String> =
        found.iter().map(|(r, _, _, _)| r.clone()).collect();
    index.books.retain(|k, _| found_keys.contains(k));

    let total = found.len();
    let to_index: Vec<(String, u64, u64, PathBuf)> = found
        .iter()
        .filter(|f| match index.books.get(&f.0) {
            Some(b) => b.size != f.1 || b.mtime != f.2,
            None => true,
        })
        .cloned()
        .collect();

    if !to_index.is_empty() {
        eprintln!("Indexing {}/{} epub files...", to_index.len(), total);
        for (i, (rel, size, mt, path)) in to_index.iter().enumerate() {
            eprintln!("  [{}/{}] {}", i + 1, to_index.len(), rel);
            let mut e = extract_metadata(path);
            e.path = rel.clone();
            e.size = *size;
            e.mtime = *mt;
            index.books.insert(rel.clone(), e);
        }
        let data = serde_json::to_string_pretty(&index)?;
        fs::write(&index_path, data)?;
    }

    Ok(index)
}

#[derive(Default)]
struct App {
    filter: String,
    list_state: ListState,
    books: Vec<BookEntry>,
    filtered: Vec<usize>,
}

impl App {
    fn new(books: Vec<BookEntry>) -> Self {
        let mut a = App {
            books,
            ..Default::default()
        };
        a.recompute_filter();
        a
    }

    fn search_haystack(b: &BookEntry) -> String {
        let fname = Path::new(&b.path)
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
            .replace('_', " ");
        format!("{} {} {}", fname, b.title, b.author).to_lowercase()
    }

    fn recompute_filter(&mut self) {
        let q = self.filter.trim().to_lowercase();
        let terms: Vec<&str> = q.split_whitespace().collect();
        self.filtered = self
            .books
            .iter()
            .enumerate()
            .filter(|(_, b)| {
                if terms.is_empty() {
                    return true;
                }
                let h = Self::search_haystack(b);
                terms.iter().all(|t| h.contains(t))
            })
            .map(|(i, _)| i)
            .collect();
        if self.filtered.is_empty() {
            self.list_state.select(None);
        } else {
            self.list_state.select(Some(0));
        }
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered.is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let max = self.filtered.len() as i32 - 1;
        let new = (cur + delta).clamp(0, max);
        self.list_state.select(Some(new as usize));
    }

    fn selected_book(&self) -> Option<&BookEntry> {
        let idx = self.list_state.selected()?;
        let bidx = *self.filtered.get(idx)?;
        self.books.get(bidx)
    }
}

fn run_tui(books: Vec<BookEntry>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(books);
    let res = event_loop(&mut terminal, &mut app);

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    res
}

fn event_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != event::KeyEventKind::Press && key.kind != event::KeyEventKind::Repeat {
                    continue;
                }
                match key.code {
                    KeyCode::Esc => return Ok(()),
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        return Ok(());
                    }
                    KeyCode::Up => app.move_selection(-1),
                    KeyCode::Down => app.move_selection(1),
                    KeyCode::PageUp => app.move_selection(-10),
                    KeyCode::PageDown => app.move_selection(10),
                    KeyCode::Home => app
                        .list_state
                        .select(if app.filtered.is_empty() { None } else { Some(0) }),
                    KeyCode::End => {
                        if !app.filtered.is_empty() {
                            app.list_state.select(Some(app.filtered.len() - 1));
                        }
                    }
                    KeyCode::Backspace => {
                        app.filter.pop();
                        app.recompute_filter();
                    }
                    KeyCode::Char(c) => {
                        app.filter.push(c);
                        app.recompute_filter();
                    }
                    _ => {}
                }
            }
        }
    }
}

fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1), Constraint::Length(1)])
        .split(f.area());

    let filter_text = format!("{}_", app.filter);
    let filter = Paragraph::new(filter_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!("Filter ({} matches)", app.filtered.len())),
    );
    f.render_widget(filter, chunks[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    let items: Vec<ListItem> = app
        .filtered
        .iter()
        .map(|&i| {
            let b = &app.books[i];
            let label = if !b.title.is_empty() {
                let auth = if b.author.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", b.author)
                };
                format!("{}{}", b.title, auth)
            } else {
                Path::new(&b.path)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| b.path.clone())
            };
            ListItem::new(Line::from(label))
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Books"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, body[0], &mut app.list_state);

    let detail_lines: Vec<Line> = match app.selected_book() {
        Some(b) => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Title:  ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(b.title.clone()),
                ]),
                Line::from(vec![
                    Span::styled("Author: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(b.author.clone()),
                ]),
                Line::from(vec![
                    Span::styled("File:   ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(b.path.clone()),
                ]),
                Line::from(""),
            ];
            for (k, v) in &b.metadata {
                let kl = k.to_ascii_lowercase();
                if kl == "title" || kl == "author(s)" || kl == "authors" || kl == "author" {
                    continue;
                }
                lines.push(Line::from(vec![
                    Span::styled(
                        format!("{}: ", k),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(v.clone()),
                ]));
            }
            lines
        }
        None => vec![Line::from("No selection")],
    };
    let details = Paragraph::new(detail_lines)
        .block(Block::default().borders(Borders::ALL).title("Details"))
        .wrap(Wrap { trim: false });
    f.render_widget(details, body[1]);

    let help = Paragraph::new(
        "Type to filter · Backspace · ↑↓ navigate · PgUp/PgDn · Home/End · Esc quit",
    );
    f.render_widget(help, chunks[2]);
}

fn main() -> Result<()> {
    let cwd = std::env::current_dir().context("getting cwd")?;
    let index = build_or_update_index(&cwd)?;
    let books: Vec<BookEntry> = index.books.into_values().collect();
    if books.is_empty() {
        eprintln!("No .epub files found in {}", cwd.display());
        return Ok(());
    }
    run_tui(books)?;
    Ok(())
}
