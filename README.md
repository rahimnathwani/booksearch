# booksearch

A fast terminal UI for searching and sharing the ebooks you already have on disk.

`booksearch` walks the current directory, builds a local index of every ebook
it finds, and gives you instant typeahead search over titles, authors, and
filenames. Pick a book and hit Enter to send it to someone via
[magic-wormhole](https://github.com/magic-wormhole/magic-wormhole) without
uploading anywhere or signing up for anything.

## Why this exists

Calibre and Calibre Web are great if you want a managed library: a single
canonical "library folder" that Calibre owns, with files renamed and
reorganised into its own structure. That model is wrong if you already have
a folder tree of ebooks you've curated yourself, or if your collection lives
on a NAS that other tools also touch.

`booksearch` is built around a different premise:

- **Your files stay exactly where they are.** Nothing is moved, copied, or
  renamed.
- **Your files are never modified.** Metadata is read, not written back.
- **The index is a sidecar.** A single SQLite file (`.booksearch.db`) and a
  cover cache (`.booksearch_covers/`) live in the directory you ran it in.
  Delete those two and the tool leaves no trace.
- **Search is local and instant.** No server, no account, no network unless
  you explicitly choose to share a file.

### Compared to Calibre / Calibre Web

| | booksearch | Calibre / Calibre Web |
|---|---|---|
| Touches your files | Never | Renames, reorganises, can rewrite metadata |
| Library layout | Your existing folder tree | Calibre's own structure |
| Search UI | TUI typeahead | Desktop GUI / web UI |
| Remote access | No | Yes (Calibre Web) |
| Edit metadata | No | Yes |
| Convert formats | No | Yes |
| Send a single book to a friend | One keystroke (wormhole) | Email plugin / download link |
| Dependencies | One binary (+ optional `ebook-meta` for non-epub) | Full Calibre install |
| Where the index lives | `.booksearch.db` next to your books | Calibre's library database |

If you want a complete library manager, use Calibre. If you want fast search
over an existing folder tree without anything being modified, use this.

## Prerequisites

- **Rust toolchain** (1.92 or newer) to build from source.
- **A terminal.** Cover images render best in
  [kitty](https://sw.kovidgoyal.net/kitty/),
  [WezTerm](https://wezfurlong.org/wezterm/), iTerm2, or any terminal that
  supports the kitty graphics protocol, iTerm2 inline images, or sixel.
  Other terminals fall back to half-block rendering or skip covers.
- **Calibre's `ebook-meta` CLI** is required *only* if you want to index
  non-EPUB formats (`.azw3`, `.azw`, `.mobi`, `.pdf`, `.fb2`, `.lit`,
  `.lrf`, `.kfx`). EPUB files are parsed in-process with
  [`rbook`](https://crates.io/crates/rbook) and need no external tools.
  - macOS: `brew install --cask calibre`
  - Debian/Ubuntu: `sudo apt install calibre`
  - Or download from [calibre-ebook.com](https://calibre-ebook.com/download)
  - Make sure `ebook-meta` is on your `PATH`.

The recipient on the other end of a magic-wormhole transfer needs a
wormhole client — install instructions at
<https://github.com/magic-wormhole/magic-wormhole>.

## Install

```sh
git clone https://github.com/rahimnathwani/booksearch
cd booksearch
cargo install --path .
```

This puts a `booksearch` binary in `~/.cargo/bin/`.

## Usage

```sh
cd /path/to/your/ebooks
booksearch
```

The first run scans the directory tree and indexes every supported file in
the background. The TUI starts immediately; you can search while indexing
is still in progress and books appear as soon as they're parsed. Subsequent
runs only re-parse files whose size or mtime has changed.

### Keys

| Key | Action |
|---|---|
| (any character) | Append to the filter; results update instantly |
| Backspace | Delete a character from the filter |
| ↑ / ↓ | Move selection |
| PgUp / PgDn | Move selection by 10 |
| Home / End | Jump to first / last result |
| Enter | Share the selected book via magic-wormhole |
| Esc (in browse) | Quit |
| Esc (in share modal) | Cancel transfer and return to browse |
| Ctrl-C | Quit |

### Search behaviour

Filter terms are split on whitespace and matched as a logical AND.
Each term is a case-insensitive substring match against the file name
(with `_` treated as a space), the book's title, and its author(s).

Typing is incremental: each new character only re-filters the previous
result set, so search stays responsive even with hundreds of thousands
of books.

### Sharing a book

Hit Enter on the selected book. A modal appears with a wormhole code like
`7-crossover-clockwork`. Tell the recipient that code (over any channel —
chat, voice, smoke signals). On their end:

```sh
wormhole receive 7-crossover-clockwork
```

The transfer is end-to-end encrypted. There's no upload step: the bytes
flow directly between the two machines (or via a relay if direct connection
isn't possible). Esc cancels at any time.

## What gets stored

In the directory you run `booksearch` in:

- `.booksearch.db` — SQLite database with the index (FTS5 enabled).
- `.booksearch_covers/` — extracted cover images, one per book, named by
  hash of the relative path.

Both are safe to delete; the next run will rebuild from scratch.

Add this to your `.gitignore` if your ebook directory is under version
control:

```gitignore
.booksearch.db
.booksearch.db-shm
.booksearch.db-wal
.booksearch_covers/
```

## Limitations

- No metadata editing. Use Calibre or a tag editor for that.
- No format conversion.
- No remote access. It's a local terminal app.
- Non-EPUB formats need Calibre's `ebook-meta` on `PATH`. Without it,
  those files are still listed (by filename) but title/author/cover
  won't be populated.
- A single very large or malformed file can take time to parse. The
  indexer enforces a 60 second timeout and records a placeholder so the
  same file isn't retried on every run; bump `PARSE_TIMEOUT` in the
  source if your environment legitimately needs longer.

## Supported formats

EPUB (parsed in-process), plus the following via `ebook-meta`: AZW3, AZW,
MOBI, PDF, FB2, LIT, LRF, KFX.

## License

See repository for license details.
