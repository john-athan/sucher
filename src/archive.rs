// Archive listing viewer — a read-only table of contents.
//
// Sucher does not extract; it shows what an archive *holds* (path, size) so you
// can see inside without unpacking. Backends by extension:
//   * zip                    -> the `zip` crate
//   * tar                    -> the `tar` crate
//   * tar.gz / tgz / gz-tar  -> `tar` over a gzip decoder
//   * plain .gz (one file)   -> a single synthesised entry
// Formats we have no decoder for (7z/rar/xz/bz2/zst) report honestly rather than
// pretending. The viewer presents the flat entry list as a navigable tree —
// Enter descends into a folder, Backspace goes up, a breadcrumb shows the path —
// with sub-folders derived from path prefixes so archives that omit explicit
// directory entries still browse correctly. `dump` (piped output) stays flat.

use crate::theme;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::{DefaultTerminal, Frame};
use std::fs;
use std::io::{self, Read};
use std::path::Path;

/// One archive member: display path, uncompressed size, directory flag.
pub struct Entry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
}

/// One visible row within the currently-open folder of an archive.
struct Node {
    /// Last path segment (folders keep a trailing `/`).
    label: String,
    is_dir: bool,
    size: u64,
}

pub struct App {
    title: String,
    entries: Vec<Entry>,
    cwd: String, // path prefix of the open folder ("" = root, else ends with '/')
    view: Vec<Node>,
    sel: usize,
    offset: usize,
    viewport_h: u16,
}

pub fn run(title: String, path: String) -> io::Result<()> {
    let entries = match entries(&path) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("sucher: {path}: {e}");
            return Ok(());
        }
    };
    let mut app = App {
        title,
        entries,
        cwd: String::new(),
        view: Vec::new(),
        sel: 0,
        offset: 0,
        viewport_h: 0,
    };
    app.rebuild();
    let mut term = ratatui::init();
    let res = app.main_loop(&mut term);
    ratatui::restore();
    res
}

/// The immediate children of `cwd` within a flat entry list: sub-folders (derived
/// from path prefixes, so archives that omit explicit directory entries still get
/// them) followed by files directly in the folder. Both groups sorted by name.
fn children(entries: &[Entry], cwd: &str) -> Vec<Node> {
    use std::collections::BTreeSet;
    let mut dirs: BTreeSet<String> = BTreeSet::new();
    let mut files: Vec<Node> = Vec::new();
    for e in entries {
        let Some(rest) = e.name.strip_prefix(cwd) else {
            continue;
        };
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            continue;
        }
        match rest.find('/') {
            // A path segment before a `/` is a sub-folder of `cwd`.
            Some(i) => {
                dirs.insert(rest[..i].to_string());
            }
            None if e.is_dir => {
                dirs.insert(rest.to_string());
            }
            None => files.push(Node {
                label: rest.to_string(),
                is_dir: false,
                size: e.size,
            }),
        }
    }
    files.sort_by(|a, b| a.label.to_lowercase().cmp(&b.label.to_lowercase()));
    let mut out: Vec<Node> = dirs
        .into_iter()
        .map(|d| Node {
            label: format!("{d}/"),
            is_dir: true,
            size: 0,
        })
        .collect();
    out.extend(files);
    out
}

/// One-shot listing to stdout for piped/non-TTY output: `size\tpath` per line.
pub fn dump(path: &str) -> String {
    match entries(path) {
        Ok(entries) => {
            let mut out = String::new();
            for e in &entries {
                let size = if e.is_dir {
                    "-".to_string()
                } else {
                    crate::util::human_size(e.size)
                };
                out.push_str(&format!("{size}\t{}\n", e.name));
            }
            out.push_str(&format!("({} entries)\n", entries.len()));
            out
        }
        Err(e) => format!("sucher: {path}: {e}\n"),
    }
}

/// Read an archive's table of contents. PURE-ish: opens the file read-only and
/// never extracts. Errors carry a human message (unsupported type, corrupt, IO).
pub fn entries(path: &str) -> Result<Vec<Entry>, String> {
    let lower = path.to_lowercase();
    let mut list = if lower.ends_with(".zip") {
        zip_entries(path)?
    } else if lower.ends_with(".tar") {
        // A plain .tar is bounded by file size, but listing streams the whole
        // file to read names — unbounded over an S3/GCS mount. Cap the read too
        // (ADR 0009); a huge tar then lists partially (with a marker) rather than
        // reading forever.
        let f = fs::File::open(path).map_err(|e| e.to_string())?;
        tar_entries(f.take(crate::util::MAX_ARCHIVE_INFLATE as u64))?
    } else if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        let f = fs::File::open(path).map_err(|e| e.to_string())?;
        tar_entries(gz_capped(f))?
    } else if lower.ends_with(".gz") {
        // A bare .gz wraps a single file; it may or may not be a tar inside.
        let f = fs::File::open(path).map_err(|e| e.to_string())?;
        match tar_entries(gz_capped(f)) {
            Ok(list) if !list.is_empty() => list,
            _ => vec![gz_single_entry(path)],
        }
    } else {
        return Err("no archive lister for this type (extract with a shell tool)".to_string());
    };
    // Directories after their files is noisy; sort dirs first, then by path.
    list.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(list)
}

fn zip_entries(path: &str) -> Result<Vec<Entry>, String> {
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| e.to_string())?;
    let mut out = Vec::with_capacity(zip.len());
    for i in 0..zip.len() {
        let f = zip.by_index(i).map_err(|e| e.to_string())?;
        out.push(Entry {
            name: f.name().to_string(),
            size: f.size(),
            is_dir: f.is_dir(),
        });
    }
    Ok(out)
}

fn tar_entries<R: Read>(reader: io::Take<R>) -> Result<Vec<Entry>, String> {
    let mut ar = tar::Archive::new(reader);
    let mut out = Vec::new();
    // A failing `entries()` means we could not start reading a tar at all (not a
    // tar / corrupt) — propagate it so the bare-.gz path can fall back to a
    // single synthesised entry. A *mid-stream* per-entry error means the input
    // ended early, which is exactly what our inflation/read cap (ADR 0009) does
    // to a huge or bomb archive: stop and return what we listed, degrading to a
    // partial listing rather than discarding it or hanging.
    for entry in ar.entries().map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { break };
        let name = entry
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "?".to_string());
        let is_dir = entry.header().entry_type().is_dir();
        let size = entry.header().size().unwrap_or(0);
        out.push(Entry { name, size, is_dir });
    }
    // If the inflation cap was fully consumed we can't be sure we saw every
    // header: the listing MIGHT be missing later entries, or it might be complete
    // with only the final member's data overrunning the cap. We cannot tell the
    // two apart from a `Take`, so state the uncertainty honestly rather than
    // asserting truncation or silently dropping the possibility (ADR 0009).
    // `into_inner` is reachable now the borrowing `entries()` iterator is dropped.
    if ar.into_inner().limit() == 0 && !out.is_empty() {
        out.push(Entry {
            name: format!(
                "… archive exceeds {}; listing may be incomplete",
                crate::util::human_size(crate::util::MAX_ARCHIVE_INFLATE as u64)
            ),
            size: 0,
            is_dir: false,
        });
    }
    Ok(out)
}

/// A gzip decoder whose *inflated* output is capped at
/// [`crate::util::MAX_DECODE_BYTES`] (ADR 0009). Listing a `.tar.gz` inflates the
/// entire stream just to read member names, so a gzip bomb would otherwise
/// inflate unbounded here; `take` stops the inflate at the cap and the tar reader
/// then hits EOF, yielding a partial listing (see [`tar_entries`]) rather than
/// OOM-ing. A bare `.gz` that is not a tar simply falls back to a single entry.
fn gz_capped(f: fs::File) -> io::Take<flate2::read::GzDecoder<fs::File>> {
    flate2::read::GzDecoder::new(f).take(crate::util::MAX_ARCHIVE_INFLATE as u64)
}

/// The single logical member of a bare `.gz`: `foo.txt.gz` → `foo.txt`. Size is
/// unknown without inflating, so it is reported as 0.
fn gz_single_entry(path: &str) -> Entry {
    let name = Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "(contents)".to_string());
    Entry {
        name,
        size: 0,
        is_dir: false,
    }
}

impl App {
    fn main_loop(&mut self, term: &mut DefaultTerminal) -> io::Result<()> {
        let mut dirty = true;
        loop {
            if dirty {
                term.draw(|f| self.render(f))?;
                dirty = false;
            }
            if event::poll(std::time::Duration::from_millis(1000))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        dirty = true;
                        if self.handle_key(key.code) {
                            return Ok(());
                        }
                    }
                    Event::Resize(..) => dirty = true,
                    _ => {}
                }
            }
        }
    }

    /// Recompute the visible rows for the current folder and clamp the cursor.
    fn rebuild(&mut self) {
        self.view = children(&self.entries, &self.cwd);
        self.sel = self.sel.min(self.view.len().saturating_sub(1));
        self.offset = 0;
    }

    /// Descend into the selected folder (Enter/l/→ on a directory row).
    fn enter(&mut self) {
        if let Some(node) = self.view.get(self.sel) {
            if node.is_dir {
                self.cwd = format!("{}{}", self.cwd, node.label); // label ends in '/'
                self.rebuild();
            }
        }
    }

    /// Go up one folder (Backspace/h/←). No-op at the archive root.
    fn up(&mut self) {
        if self.cwd.is_empty() {
            return;
        }
        let trimmed = self.cwd.trim_end_matches('/');
        self.cwd = match trimmed.rfind('/') {
            Some(i) => trimmed[..=i].to_string(),
            None => String::new(),
        };
        self.rebuild();
    }

    /// Returns true to quit.
    fn handle_key(&mut self, code: KeyCode) -> bool {
        let max = self.view.len().saturating_sub(1);
        let half = (self.viewport_h / 2).max(1) as usize;
        match code {
            KeyCode::Char('q') | KeyCode::Esc => return true,
            KeyCode::Char('j') | KeyCode::Down => self.sel = (self.sel + 1).min(max),
            KeyCode::Char('k') | KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Char('d') | KeyCode::PageDown => self.sel = (self.sel + half).min(max),
            KeyCode::Char('u') | KeyCode::PageUp => self.sel = self.sel.saturating_sub(half),
            KeyCode::Char('g') | KeyCode::Home => self.sel = 0,
            KeyCode::Char('G') | KeyCode::End => self.sel = max,
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => self.enter(),
            KeyCode::Backspace | KeyCode::Char('h') | KeyCode::Left => self.up(),
            _ => {}
        }
        false
    }

    fn render(&mut self, f: &mut Frame) {
        let area = f.area();
        let body = Layout::default()
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(area);

        // Title shows the archive name and the open path (breadcrumb).
        let crumb = if self.cwd.is_empty() {
            format!(" {} ", self.title)
        } else {
            format!(" {}  ›  {} ", self.title, self.cwd.trim_end_matches('/'))
        };
        let block = Block::default().borders(Borders::ALL).title(crumb);
        let inner = block.inner(body[0]);
        self.viewport_h = inner.height;

        // Keep the selection in view.
        if self.sel < self.offset {
            self.offset = self.sel;
        } else if self.sel >= self.offset + inner.height.max(1) as usize {
            self.offset = self.sel + 1 - inner.height.max(1) as usize;
        }

        let end = (self.offset + inner.height as usize).min(self.view.len());
        let rows: Vec<Line> = (self.offset..end)
            .map(|i| {
                let n = &self.view[i];
                let size = if n.is_dir {
                    "     dir".to_string()
                } else {
                    format!("{:>8}", crate::util::human_size(n.size))
                };
                let color = if n.is_dir {
                    theme::palette().dir
                } else {
                    theme::palette().other
                };
                let mut style = Style::default().fg(color);
                if i == self.sel {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::from(vec![
                    Span::styled(
                        format!(" {size}  "),
                        Style::default().fg(theme::palette().dim),
                    ),
                    Span::styled(n.label.clone(), style),
                ])
            })
            .collect();
        f.render_widget(Paragraph::new(Text::from(rows)).block(block), body[0]);

        let status = format!(
            " {} items   [j/k] move  [Enter/l] open folder  [h/Bksp] up  [g/G] top/end  [q] quit",
            self.view.len(),
        );
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(theme::palette().dim)),
            body[1],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{children, Entry};

    fn e(name: &str, is_dir: bool) -> Entry {
        Entry {
            name: name.to_string(),
            size: 10,
            is_dir,
        }
    }

    #[test]
    fn children_derives_folders_without_explicit_dir_entries() {
        // No explicit "src/" entry — it must still appear as a folder at root.
        let entries = vec![
            e("README.md", false),
            e("src/main.rs", false),
            e("src/lib.rs", false),
            e("src/util/mod.rs", false),
        ];
        let root: Vec<_> = children(&entries, "")
            .into_iter()
            .map(|n| (n.label, n.is_dir))
            .collect();
        assert_eq!(
            root,
            vec![("src/".into(), true), ("README.md".into(), false)]
        );

        // Inside src/: one nested folder (util/) and two files.
        let inside: Vec<_> = children(&entries, "src/")
            .into_iter()
            .map(|n| (n.label, n.is_dir))
            .collect();
        assert_eq!(
            inside,
            vec![
                ("util/".into(), true),
                ("lib.rs".into(), false),
                ("main.rs".into(), false),
            ]
        );
    }
}
