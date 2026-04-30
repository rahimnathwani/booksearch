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
use ratatui_image::{picker::Picker, protocol::StatefulProtocol, StatefulImage};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::{self, Stdout},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};
use walkdir::WalkDir;

const DB_FILE: &str = ".booksearch.db";
const COVER_DIR: &str = ".booksearch_covers";
const BATCH_SIZE: usize = 15;
const PARSE_TIMEOUT: Duration = Duration::from_secs(60);
const CHECKPOINT_EVERY_BATCHES: usize = 10;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct BookEntry {
    path: String,
    size: u64,
    mtime: u64,
    title: String,
    author: String,
    metadata: BTreeMap<String, String>,
    cover: Option<String>,
    /// Lowercased "filename(_→space) title author"; precomputed for fast filter matching.
    /// Not persisted — rebuilt on load and on every Book update.
    #[serde(skip)]
    search_text: String,
}

fn build_search_text(b: &BookEntry) -> String {
    let fname = Path::new(&b.path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
        .replace('_', " ");
    format!("{} {} {}", fname, b.title, b.author).to_lowercase()
}

fn parse_terms(filter: &str) -> Vec<String> {
    filter
        .trim()
        .to_lowercase()
        .split_whitespace()
        .map(String::from)
        .collect()
}

/// Sequentially read the file to warm the OS page cache before parsing.
/// On spinning disks this converts the parser's later random seeks (zip central
/// directory at EOF, then back to individual entries) into RAM hits.
fn prewarm_file(path: &Path) {
    use std::io::Read;
    if let Ok(mut f) = fs::File::open(path) {
        let mut buf = [0u8; 64 * 1024];
        loop {
            match f.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }
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

fn cover_filename(rel: &str) -> String {
    let mut h = DefaultHasher::new();
    rel.hash(&mut h);
    format!("{:016x}.jpg", h.finish())
}

fn extract_metadata_rbook(epub_path: &Path, cover_dir: &Path, rel: &str) -> Option<BookEntry> {
    use rbook::Epub;

    let epub = Epub::options()
        .skip_toc(true)
        .skip_spine(true)
        .open(epub_path)
        .ok()?;
    let meta = epub.metadata();

    let title = meta.title().map(|t| t.value().to_string()).unwrap_or_default();
    let authors: Vec<String> = meta.creators().map(|c| c.value().to_string()).collect();
    let author = authors.join(" & ");

    let mut map = BTreeMap::new();
    if !title.is_empty() {
        map.insert("Title".into(), title.clone());
    }
    if !author.is_empty() {
        map.insert("Author(s)".into(), author.clone());
    }
    let publishers: Vec<String> = meta.publishers().map(|p| p.value().to_string()).collect();
    if !publishers.is_empty() {
        map.insert("Publisher".into(), publishers.join(", "));
    }
    if let Some(lang) = meta.language() {
        map.insert("Languages".into(), lang.value().to_string());
    }
    if let Some(d) = meta.description() {
        map.insert("Comments".into(), d.value().to_string());
    }
    if let Some(id) = meta.identifier() {
        map.insert("Identifiers".into(), id.value().to_string());
    }
    let tags: Vec<String> = meta.tags().map(|t| t.value().to_string()).collect();
    if !tags.is_empty() {
        map.insert("Tags".into(), tags.join(", "));
    }

    let cover_path = cover_dir.join(cover_filename(rel));
    let _ = fs::remove_file(&cover_path);
    let cover = epub
        .manifest()
        .cover_image()
        .and_then(|c| c.read_bytes().ok())
        .filter(|b| !b.is_empty())
        .and_then(|bytes| {
            fs::write(&cover_path, &bytes).ok()?;
            Some(cover_path.to_string_lossy().to_string())
        });

    Some(BookEntry {
        path: String::new(),
        size: 0,
        mtime: 0,
        title,
        author,
        metadata: map,
        cover,
        search_text: String::new(),
    })
}

fn run_command_with_timeout(
    mut cmd: Command,
    timeout: Duration,
) -> Option<std::process::Output> {
    use std::io::Read;
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    let mut child = cmd.spawn().ok()?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = Vec::new();
                let mut err = Vec::new();
                if let Some(mut s) = child.stdout.take() {
                    let _ = s.read_to_end(&mut out);
                }
                if let Some(mut s) = child.stderr.take() {
                    let _ = s.read_to_end(&mut err);
                }
                return Some(std::process::Output {
                    status,
                    stdout: out,
                    stderr: err,
                });
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                thread::sleep(Duration::from_millis(100));
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

fn extract_metadata_external(epub: &Path, cover_dir: &Path, rel: &str) -> BookEntry {
    let mut entry = BookEntry::default();
    let cover_path = cover_dir.join(cover_filename(rel));
    let _ = fs::remove_file(&cover_path);
    let cover_arg = format!("--get-cover={}", cover_path.display());
    let mut cmd = Command::new("ebook-meta");
    cmd.arg(epub).arg(&cover_arg);
    let output = run_command_with_timeout(cmd, PARSE_TIMEOUT);
    if let Some(out) = output {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).to_string();
            let (title, author, map) = parse_ebook_meta(&s);
            entry.title = title;
            entry.author = author;
            entry.metadata = map;
        }
    } else {
        entry
            .metadata
            .insert("_indexer_status".into(), "ebook-meta timed out".into());
    }
    if cover_path.exists() && fs::metadata(&cover_path).map(|m| m.len() > 0).unwrap_or(false) {
        entry.cover = Some(cover_path.to_string_lossy().to_string());
    }
    entry
}

fn extract_metadata_rbook_with_timeout(
    epub_path: &Path,
    cover_dir: &Path,
    rel: &str,
) -> Result<Option<BookEntry>, ()> {
    let p = epub_path.to_path_buf();
    let cd = cover_dir.to_path_buf();
    let r = rel.to_string();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let result = extract_metadata_rbook(&p, &cd, &r);
        let _ = tx.send(result);
    });
    match rx.recv_timeout(PARSE_TIMEOUT) {
        Ok(v) => Ok(v),
        Err(RecvTimeoutError::Timeout) => Err(()),
        Err(RecvTimeoutError::Disconnected) => Ok(None),
    }
}

fn extract_metadata(file: &Path, cover_dir: &Path, rel: &str) -> BookEntry {
    let is_epub = file
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.eq_ignore_ascii_case("epub"))
        .unwrap_or(false);
    if is_epub {
        match extract_metadata_rbook_with_timeout(file, cover_dir, rel) {
            Ok(Some(e)) => return e,
            Ok(None) => {} // rbook failed cleanly, fall back
            Err(()) => {
                // rbook hung; thread is abandoned. Mark and skip ebook-meta to avoid double hang.
                let mut entry = BookEntry::default();
                entry
                    .metadata
                    .insert("_indexer_status".into(), "rbook timed out".into());
                return entry;
            }
        }
    }
    extract_metadata_external(file, cover_dir, rel)
}

fn open_db(root: &Path) -> Result<Connection> {
    let conn = Connection::open(root.join(DB_FILE))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    conn.pragma_update(None, "cache_size", -65536i64)?; // 64 MB
    conn.pragma_update(None, "mmap_size", 268435456i64)?; // 256 MB
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS books (
            path TEXT PRIMARY KEY,
            size INTEGER NOT NULL,
            mtime INTEGER NOT NULL,
            title TEXT NOT NULL DEFAULT '',
            author TEXT NOT NULL DEFAULT '',
            cover TEXT,
            metadata TEXT NOT NULL DEFAULT '{}'
         );
         CREATE VIRTUAL TABLE IF NOT EXISTS books_fts USING fts5(
            path UNINDEXED,
            title,
            author,
            filename,
            tokenize = 'unicode61 remove_diacritics 1'
         );",
    )?;
    Ok(conn)
}

fn load_all_books(conn: &Connection) -> Result<Vec<BookEntry>> {
    let mut stmt = conn.prepare(
        "SELECT path,size,mtime,title,author,cover,metadata FROM books \
         ORDER BY title COLLATE NOCASE, author COLLATE NOCASE, path COLLATE NOCASE",
    )?;
    let rows = stmt.query_map([], |r| {
        let metadata: String = r.get(6)?;
        let map: BTreeMap<String, String> = serde_json::from_str(&metadata).unwrap_or_default();
        Ok(BookEntry {
            path: r.get(0)?,
            size: r.get::<_, i64>(1)? as u64,
            mtime: r.get::<_, i64>(2)? as u64,
            title: r.get(3)?,
            author: r.get(4)?,
            cover: r.get(5)?,
            metadata: map,
            search_text: String::new(),
        })
    })?;
    Ok(rows
        .filter_map(Result::ok)
        .map(|mut b| {
            b.search_text = build_search_text(&b);
            b
        })
        .collect())
}

fn upsert_book(conn: &Connection, e: &BookEntry) -> Result<()> {
    let metadata_json = serde_json::to_string(&e.metadata)?;
    conn.execute(
        "INSERT INTO books(path,size,mtime,title,author,cover,metadata) VALUES(?1,?2,?3,?4,?5,?6,?7)
         ON CONFLICT(path) DO UPDATE SET size=excluded.size,mtime=excluded.mtime,
            title=excluded.title,author=excluded.author,cover=excluded.cover,metadata=excluded.metadata",
        params![
            e.path,
            e.size as i64,
            e.mtime as i64,
            e.title,
            e.author,
            e.cover,
            metadata_json
        ],
    )?;
    let filename = Path::new(&e.path)
        .file_name()
        .map(|s| s.to_string_lossy().replace('_', " "))
        .unwrap_or_default();
    conn.execute("DELETE FROM books_fts WHERE path = ?1", params![e.path])?;
    conn.execute(
        "INSERT INTO books_fts(path,title,author,filename) VALUES(?1,?2,?3,?4)",
        params![e.path, e.title, e.author, filename],
    )?;
    Ok(())
}

fn delete_book(conn: &Connection, path: &str) -> Result<()> {
    conn.execute("DELETE FROM books WHERE path = ?1", params![path])?;
    conn.execute("DELETE FROM books_fts WHERE path = ?1", params![path])?;
    Ok(())
}

#[derive(Debug)]
enum IndexUpdate {
    Total(usize),
    Book(BookEntry),
    Removed(String),
    Progress { done: usize, total: usize },
    Done,
}

fn spawn_indexer(root: PathBuf, tx: Sender<IndexUpdate>) {
    thread::spawn(move || {
        if let Err(e) = run_indexer(&root, &tx) {
            eprintln!("indexer error: {e}");
        }
        let _ = tx.send(IndexUpdate::Done);
    });
}

fn run_indexer(root: &Path, tx: &Sender<IndexUpdate>) -> Result<()> {
    let cover_dir = root.join(COVER_DIR);
    fs::create_dir_all(&cover_dir).ok();
    let conn = open_db(root)?;

    let existing: HashMap<String, (i64, i64, Option<String>)> = {
        let mut stmt = conn.prepare("SELECT path,size,mtime,cover FROM books")?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        rows.filter_map(Result::ok)
            .map(|(p, s, m, c)| (p, (s, m, c)))
            .collect()
    };

    let mut found: Vec<(String, u64, u64, PathBuf)> = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        let ext_ok = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| {
                matches!(
                    s.to_ascii_lowercase().as_str(),
                    "epub" | "azw3" | "azw" | "mobi" | "pdf" | "fb2" | "lit" | "lrf" | "kfx"
                )
            })
            .unwrap_or(false);
        if !ext_ok {
            continue;
        }
        let md = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mt = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let rel = p
            .strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .to_string();
        found.push((rel, md.len(), mt, p.to_path_buf()));
    }
    let found_keys: HashSet<String> = found.iter().map(|(r, _, _, _)| r.clone()).collect();

    for (path, (_, _, cover)) in &existing {
        if !found_keys.contains(path) {
            if let Some(c) = cover {
                let _ = fs::remove_file(c);
            }
            delete_book(&conn, path)?;
            let _ = tx.send(IndexUpdate::Removed(path.clone()));
        }
    }

    let mut to_index: Vec<(String, u64, u64, PathBuf)> = found
        .into_iter()
        .filter(|(rel, size, mt, _)| match existing.get(rel) {
            Some((s, m, c)) => {
                *s as u64 != *size
                    || *m as u64 != *mt
                    || c.as_ref().map(|p| !Path::new(p).exists()).unwrap_or(false)
            }
            None => true,
        })
        .collect();

    // Index epubs first — rbook is in-process and fast; other formats shell out to ebook-meta.
    to_index.sort_by_key(|(rel, _, _, _)| {
        let is_epub = Path::new(rel)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.eq_ignore_ascii_case("epub"))
            .unwrap_or(false);
        if is_epub {
            0
        } else {
            1
        }
    });

    let total = to_index.len();
    let _ = tx.send(IndexUpdate::Total(total));
    if total == 0 {
        return Ok(());
    }

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let mut in_batch = 0usize;
    let mut batches_since_checkpoint = 0usize;
    for (i, (rel, size, mt, path)) in to_index.into_iter().enumerate() {
        prewarm_file(&path);
        let mut e = extract_metadata(&path, &cover_dir, &rel);
        e.path = rel.clone();
        e.size = size;
        e.mtime = mt;
        upsert_book(&conn, &e)?;
        let _ = tx.send(IndexUpdate::Book(e));
        let _ = tx.send(IndexUpdate::Progress {
            done: i + 1,
            total,
        });
        in_batch += 1;
        if in_batch >= BATCH_SIZE {
            conn.execute_batch("COMMIT")?;
            batches_since_checkpoint += 1;
            if batches_since_checkpoint >= CHECKPOINT_EVERY_BATCHES {
                let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
                batches_since_checkpoint = 0;
            }
            conn.execute_batch("BEGIN IMMEDIATE")?;
            in_batch = 0;
        }
    }
    conn.execute_batch("COMMIT")?;
    let _ = conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
    Ok(())
}

struct App {
    filter: String,
    list_state: ListState,
    books: HashMap<String, BookEntry>,
    all_paths: Vec<String>,
    /// Stack of (filter_string, paths_matching_that_filter). Bottom layer is always
    /// ("", all_paths). Each typed char pushes a layer that refines the one below;
    /// backspace pops. Top of stack is the current filtered view.
    filter_stack: Vec<(String, Vec<String>)>,
    rx: Receiver<IndexUpdate>,
    progress: Option<(usize, usize)>,
    indexing_done: bool,
    picker: Option<Picker>,
    cover_cache: HashMap<String, Option<StatefulProtocol>>,
}

impl App {
    fn new(initial: Vec<BookEntry>, rx: Receiver<IndexUpdate>, picker: Option<Picker>) -> Self {
        let mut books = HashMap::new();
        let mut all_paths = Vec::with_capacity(initial.len());
        for b in initial {
            all_paths.push(b.path.clone());
            books.insert(b.path.clone(), b);
        }
        let mut a = App {
            filter: String::new(),
            list_state: ListState::default(),
            books,
            all_paths,
            filter_stack: Vec::new(),
            rx,
            progress: None,
            indexing_done: false,
            picker,
            cover_cache: HashMap::new(),
        };
        a.sort_paths();
        a.rebuild_filter_stack();
        a
    }

    fn filtered(&self) -> &Vec<String> {
        &self.filter_stack.last().expect("base layer always present").1
    }

    fn sort_paths(&mut self) {
        let books = &self.books;
        self.all_paths.sort_by(|a, b| {
            let ba = books.get(a);
            let bb = books.get(b);
            let ka = ba.map(|x| (x.title.to_lowercase(), x.author.to_lowercase(), a.clone()));
            let kb = bb.map(|x| (x.title.to_lowercase(), x.author.to_lowercase(), b.clone()));
            ka.cmp(&kb)
        });
    }

    fn refine_paths(&self, base: &[String], terms: &[String]) -> Vec<String> {
        if terms.is_empty() {
            return base.to_vec();
        }
        base.iter()
            .filter(|p| {
                self.books
                    .get(*p)
                    .map(|b| terms.iter().all(|t| b.search_text.contains(t)))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// Discard the cached stack and rebuild it from `all_paths` + the current filter.
    /// Called when book data changes (so prior layers are stale) or when a non-incremental
    /// filter transition is requested.
    fn rebuild_filter_stack(&mut self) {
        self.filter_stack.clear();
        self.filter_stack
            .push((String::new(), self.all_paths.clone()));
        if !self.filter.is_empty() {
            let terms = parse_terms(&self.filter);
            let base = self.filter_stack[0].1.clone();
            let filtered = self.refine_paths(&base, &terms);
            self.filter_stack.push((self.filter.clone(), filtered));
        }
        self.clamp_selection();
    }

    /// Apply a new filter string. Tries to use the stack incrementally:
    /// - if `new` extends the current top, refine the top's results (cheap: scans only the
    ///   already-filtered subset)
    /// - if `new` is a prefix of the current top, pop layers until matched (free)
    /// - otherwise, full rebuild
    fn set_filter(&mut self, new_filter: String) {
        let cur = self
            .filter_stack
            .last()
            .map(|(s, _)| s.clone())
            .unwrap_or_default();
        if new_filter == cur {
            self.filter = new_filter;
            return;
        }
        if new_filter.starts_with(&cur) {
            let terms = parse_terms(&new_filter);
            let base = self.filter_stack.last().unwrap().1.clone();
            let filtered = self.refine_paths(&base, &terms);
            self.filter_stack.push((new_filter.clone(), filtered));
            self.filter = new_filter;
            self.clamp_selection();
        } else if cur.starts_with(&new_filter) {
            while self.filter_stack.len() > 1
                && self.filter_stack.last().unwrap().0 != new_filter
            {
                self.filter_stack.pop();
            }
            if self
                .filter_stack
                .last()
                .map(|(s, _)| s.as_str())
                != Some(new_filter.as_str())
            {
                self.filter = new_filter;
                self.rebuild_filter_stack();
            } else {
                self.filter = new_filter;
                self.clamp_selection();
            }
        } else {
            self.filter = new_filter;
            self.rebuild_filter_stack();
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.filtered().len();
        if len == 0 {
            self.list_state.select(None);
        } else {
            let cur = self.list_state.selected().unwrap_or(0);
            self.list_state.select(Some(cur.min(len - 1)));
        }
    }

    fn drain_updates(&mut self) -> bool {
        let mut changed = false;
        let mut got_book_or_removal = false;
        loop {
            match self.rx.try_recv() {
                Ok(IndexUpdate::Total(t)) => {
                    self.progress = Some((0, t));
                    changed = true;
                }
                Ok(IndexUpdate::Progress { done, total }) => {
                    self.progress = Some((done, total));
                    changed = true;
                }
                Ok(IndexUpdate::Book(mut b)) => {
                    b.search_text = build_search_text(&b);
                    let path = b.path.clone();
                    let is_new = !self.books.contains_key(&path);
                    self.books.insert(path.clone(), b);
                    if is_new {
                        self.all_paths.push(path.clone());
                    }
                    self.cover_cache.remove(&path);
                    got_book_or_removal = true;
                    changed = true;
                }
                Ok(IndexUpdate::Removed(path)) => {
                    self.books.remove(&path);
                    self.all_paths.retain(|p| p != &path);
                    self.cover_cache.remove(&path);
                    got_book_or_removal = true;
                    changed = true;
                }
                Ok(IndexUpdate::Done) => {
                    self.indexing_done = true;
                    changed = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.indexing_done = true;
                    break;
                }
            }
        }
        if got_book_or_removal {
            self.sort_paths();
            let prev = self
                .list_state
                .selected()
                .and_then(|i| self.filtered().get(i).cloned());
            self.rebuild_filter_stack();
            if let Some(p) = prev {
                if let Some(i) = self.filtered().iter().position(|x| x == &p) {
                    self.list_state.select(Some(i));
                }
            }
        }
        changed
    }

    fn move_selection(&mut self, delta: i32) {
        if self.filtered().is_empty() {
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0) as i32;
        let max = self.filtered().len() as i32 - 1;
        let new = (cur + delta).clamp(0, max);
        self.list_state.select(Some(new as usize));
    }

    fn selected_path(&self) -> Option<&String> {
        let idx = self.list_state.selected()?;
        self.filtered().get(idx)
    }

    fn selected_book(&self) -> Option<&BookEntry> {
        let p = self.selected_path()?;
        self.books.get(p)
    }

    fn current_cover_protocol(&mut self) -> Option<&mut StatefulProtocol> {
        let path = self.selected_path()?.clone();
        if !self.cover_cache.contains_key(&path) {
            let proto = self.load_cover(&path);
            self.cover_cache.insert(path.clone(), proto);
        }
        self.cover_cache.get_mut(&path).and_then(|o| o.as_mut())
    }

    fn load_cover(&self, path: &str) -> Option<StatefulProtocol> {
        let picker = self.picker.as_ref()?;
        let book = self.books.get(path)?;
        let cover_path = book.cover.as_ref()?;
        let img = image::ImageReader::open(cover_path)
            .ok()?
            .with_guessed_format()
            .ok()?
            .decode()
            .ok()?;
        Some(picker.new_resize_protocol(img))
    }
}

fn run_tui(initial: Vec<BookEntry>, rx: Receiver<IndexUpdate>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let picker = Picker::from_query_stdio().ok();

    // Drain any leftover bytes from the terminal's reply to the graphics-protocol
    // query (e.g. `<ESC>_Gi=31;OK<ESC>\`) so they don't get read as keystrokes.
    let drain_deadline = Instant::now() + Duration::from_millis(50);
    while Instant::now() < drain_deadline {
        match event::poll(Duration::from_millis(10)) {
            Ok(true) => {
                let _ = event::read();
            }
            _ => break,
        }
    }

    let mut app = App::new(initial, rx, picker);
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
        app.drain_updates();
        terminal.draw(|f| draw(f, app))?;
        if event::poll(Duration::from_millis(150))? {
            if let Event::Key(key) = event::read()? {
                if key.kind != event::KeyEventKind::Press && key.kind != event::KeyEventKind::Repeat
                {
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
                    KeyCode::Home => app.list_state.select(if app.filtered().is_empty() {
                        None
                    } else {
                        Some(0)
                    }),
                    KeyCode::End => {
                        if !app.filtered().is_empty() {
                            app.list_state.select(Some(app.filtered().len() - 1));
                        }
                    }
                    KeyCode::Backspace => {
                        let mut new = app.filter.clone();
                        new.pop();
                        app.set_filter(new);
                    }
                    KeyCode::Char(c) => {
                        let mut new = app.filter.clone();
                        new.push(c);
                        app.set_filter(new);
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
    let match_count = app.filtered().len();
    let title = match (app.progress, app.indexing_done) {
        (_, true) => format!(
            "Filter ({} matches, {} books)",
            match_count,
            app.books.len()
        ),
        (Some((d, t)), false) if t > 0 => {
            format!("Filter ({} matches, indexing {}/{})", match_count, d, t)
        }
        _ => format!("Filter ({} matches, scanning...)", match_count),
    };
    let filter = Paragraph::new(filter_text)
        .block(Block::default().borders(Borders::ALL).title(title));
    f.render_widget(filter, chunks[0]);

    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(chunks[1]);

    let items: Vec<ListItem> = app
        .filtered()
        .iter()
        .filter_map(|p| app.books.get(p))
        .map(|b| {
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

    let right = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(45), Constraint::Min(1)])
        .split(body[1]);

    let cover_block = Block::default().borders(Borders::ALL).title("Cover");
    let cover_inner = cover_block.inner(right[0]);
    f.render_widget(cover_block, right[0]);
    if let Some(proto) = app.current_cover_protocol() {
        let img = StatefulImage::<StatefulProtocol>::default();
        f.render_stateful_widget(img, cover_inner, proto);
    } else {
        let msg = if app.picker.is_none() {
            "(terminal does not support images)"
        } else {
            "(no cover)"
        };
        f.render_widget(Paragraph::new(msg), cover_inner);
    }

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
    f.render_widget(details, right[1]);

    let help = Paragraph::new(
        "Type to filter · Backspace · ↑↓ navigate · PgUp/PgDn · Home/End · Esc quit",
    );
    f.render_widget(help, chunks[2]);
}

fn main() -> Result<()> {
    let cwd = std::env::current_dir().context("getting cwd")?;
    let conn = open_db(&cwd)?;
    let initial = load_all_books(&conn)?;
    drop(conn);

    let (tx, rx) = mpsc::channel();
    spawn_indexer(cwd.clone(), tx);

    run_tui(initial, rx)?;
    Ok(())
}
