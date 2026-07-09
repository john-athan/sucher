// Streaming .xlsx reader for large workbooks.
//
// calamine materializes a whole sheet before returning — fatal for multi-
// hundred-MB files. Here we stream <row> elements with quick-xml on a worker
// thread, appending to a shared buffer the UI reads live, and stop at a row
// cap. Only the prefix of each (huge) sheet XML is decompressed, so opening is
// fast regardless of total file size.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::fs::File;
use std::io::{BufReader, Read};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

// Safety bound on rows held in memory for one sheet. High enough to fully load
// the large real-world sheets we target (≈800k rows × 17 cols ≈ 600MB) so the
// whole sheet is scrollable; guards only against pathological files. Only the
// current sheet is held — switching sheets frees the previous one.
pub const ROW_CAP: usize = 2_000_000;

#[derive(Default)]
pub struct SheetData {
    pub rows: Vec<Vec<String>>,
    pub ncols: usize,
    pub done: bool,
    pub capped: bool,
}

pub struct StreamBook {
    file: String,
    sheets: Vec<(String, String)>, // (name, zip entry path)
    sst: Arc<Vec<String>>,
    cur: usize,
    data: Arc<Mutex<SheetData>>,
    stop: Arc<AtomicBool>,
}

impl StreamBook {
    pub fn open(file: &str) -> Result<Self, String> {
        let f = File::open(file).map_err(|e| e.to_string())?;
        let mut zip = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
        let sheets = read_workbook_sheets(&mut zip)?;
        let sst = Arc::new(read_shared_strings(&mut zip));
        let mut book = StreamBook {
            file: file.to_string(),
            sheets,
            sst,
            cur: 0,
            data: Arc::new(Mutex::new(SheetData::default())),
            stop: Arc::new(AtomicBool::new(false)),
        };
        book.spawn();
        Ok(book)
    }

    fn spawn(&mut self) {
        self.stop.store(true, Ordering::Relaxed); // signal any previous worker
        self.stop = Arc::new(AtomicBool::new(false));
        self.data = Arc::new(Mutex::new(SheetData::default()));

        let file = self.file.clone();
        let path = self.sheets[self.cur].1.clone();
        let sst = self.sst.clone();
        let data = self.data.clone();
        let stop = self.stop.clone();
        thread::spawn(move || stream_sheet(&file, &path, &sst, &data, &stop));
    }

    pub fn names(&self) -> Vec<String> {
        self.sheets.iter().map(|(n, _)| n.clone()).collect()
    }

    pub fn selected(&self) -> usize {
        self.cur
    }

    pub fn select(&mut self, idx: usize) {
        if idx < self.sheets.len() && idx != self.cur {
            self.cur = idx;
            self.spawn();
        }
    }

    /// (rows_loaded, ncols, done, capped)
    pub fn dims(&self) -> (usize, usize, bool, bool) {
        let d = self.data.lock().unwrap();
        (d.rows.len(), d.ncols, d.done, d.capped)
    }

    /// Find all cells (loaded so far) containing `query`, case-insensitive.
    /// Scans under one lock; covers the whole sheet once loading is done.
    pub fn find(&self, query: &str) -> Vec<(usize, usize)> {
        let needle = query.to_ascii_lowercase();
        let d = self.data.lock().unwrap();
        let mut hits = Vec::new();
        for (r, row) in d.rows.iter().enumerate() {
            for (c, cell) in row.iter().enumerate() {
                if contains_ci(cell, &needle) {
                    hits.push((r, c));
                }
            }
        }
        hits
    }

    /// Snapshot a rectangular window of cells (locks once).
    pub fn window(&self, r0: usize, r1: usize, c0: usize, c1: usize) -> Vec<Vec<String>> {
        let d = self.data.lock().unwrap();
        let mut out = Vec::new();
        for r in r0..r1.min(d.rows.len()) {
            let row = &d.rows[r];
            let mut line = Vec::with_capacity(c1.saturating_sub(c0));
            for c in c0..c1 {
                line.push(row.get(c).cloned().unwrap_or_default());
            }
            out.push(line);
        }
        out
    }
}

impl Drop for StreamBook {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Case-insensitive substring test (ASCII fold), allocation-free — important
/// when scanning tens of millions of cells. `needle` must be pre-lowercased.
pub fn contains_ci(hay: &str, needle: &str) -> bool {
    let (h, n) = (hay.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    for i in 0..=h.len() - n.len() {
        if h[i..i + n.len()]
            .iter()
            .zip(n)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
            return true;
        }
    }
    false
}

fn col_index(r: &[u8]) -> usize {
    let mut n = 0usize;
    for &b in r {
        if b.is_ascii_alphabetic() {
            n = n * 26 + (b.to_ascii_uppercase() - b'A' + 1) as usize;
        } else {
            break;
        }
    }
    n.saturating_sub(1)
}

fn read_workbook_sheets(zip: &mut zip::ZipArchive<File>) -> Result<Vec<(String, String)>, String> {
    // workbook.xml: sheet name + r:id (document order)
    let wb = read_entry(zip, "xl/workbook.xml").ok_or("missing xl/workbook.xml (not an xlsx?)")?;
    let mut order: Vec<(String, String)> = Vec::new(); // (name, rId)
    let mut rd = Reader::from_str(&wb);
    let mut buf = Vec::new();
    loop {
        match rd.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"sheet" => {
                let mut name = String::new();
                let mut rid = String::new();
                for a in e.attributes().flatten() {
                    match a.key.as_ref() {
                        b"name" => name = String::from_utf8_lossy(&a.value).into_owned(),
                        b"r:id" => rid = String::from_utf8_lossy(&a.value).into_owned(),
                        _ => {}
                    }
                }
                order.push((name, rid));
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    // rels: rId -> target path
    let rels = read_entry(zip, "xl/_rels/workbook.xml.rels").unwrap_or_default();
    let mut rid_to_path = std::collections::HashMap::new();
    let mut rd = Reader::from_str(&rels);
    let mut buf = Vec::new();
    loop {
        match rd.read_event_into(&mut buf) {
            Ok(Event::Empty(e)) | Ok(Event::Start(e)) if e.name().as_ref() == b"Relationship" => {
                let mut id = String::new();
                let mut target = String::new();
                for a in e.attributes().flatten() {
                    match a.key.as_ref() {
                        b"Id" => id = String::from_utf8_lossy(&a.value).into_owned(),
                        b"Target" => target = String::from_utf8_lossy(&a.value).into_owned(),
                        _ => {}
                    }
                }
                if !id.is_empty() {
                    rid_to_path.insert(id, target);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }

    let sheets: Vec<(String, String)> = order
        .into_iter()
        .filter_map(|(name, rid)| {
            rid_to_path.get(&rid).map(|t| {
                let t = t.trim_start_matches('/');
                let path = if t.starts_with("xl/") {
                    t.to_string()
                } else {
                    format!("xl/{t}")
                };
                (name, path)
            })
        })
        .collect();
    if sheets.is_empty() {
        return Err("no worksheets found".into());
    }
    Ok(sheets)
}

fn read_shared_strings(zip: &mut zip::ZipArchive<File>) -> Vec<String> {
    let Some(xml) = read_entry(zip, "xl/sharedStrings.xml") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut rd = Reader::from_str(&xml);
    let mut buf = Vec::new();
    let mut cur = String::new();
    let mut in_si = false;
    let mut in_t = false;
    loop {
        match rd.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"si" => {
                    cur.clear();
                    in_si = true;
                }
                b"t" => in_t = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_t && in_si => {
                cur.push_str(&crate::util::xml_text(&t));
            }
            Ok(Event::GeneralRef(r)) if in_t && in_si => {
                cur.push_str(&crate::util::xml_ref(&r));
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"t" => in_t = false,
                b"si" => {
                    out.push(std::mem::take(&mut cur));
                    in_si = false;
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

fn read_entry(zip: &mut zip::ZipArchive<File>, name: &str) -> Option<String> {
    let mut f = zip.by_name(name).ok()?;
    let mut s = String::new();
    f.read_to_string(&mut s).ok()?;
    Some(s)
}

/// Bounded, synchronous first-rows reader for the directory preview pane.
///
/// Parses ONLY the first worksheet's XML on the calling thread and stops after
/// `max_rows` rows — no background thread, no polling. The decompressed sheet
/// XML is read through [`Read::take`] at [`crate::util::MAX_DECODE_BYTES`] so a
/// zip bomb cannot inflate unbounded even if it presents few `<row>` elements.
/// Each row is truncated to `max_cols`. Shares the row/cell decoding with the
/// streaming loader via [`parse_sheet_xml`], so the two never drift (ADR 0009).
pub fn preview_rows(file: &str, max_rows: usize, max_cols: usize) -> Result<Vec<Vec<String>>, String> {
    let f = File::open(file).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(f).map_err(|e| e.to_string())?;
    let sheets = read_workbook_sheets(&mut zip)?;
    let sst = read_shared_strings(&mut zip);
    // A crafted workbook may declare no usable sheets; never index blindly.
    let sheet_path = sheets.first().ok_or("workbook has no sheets")?.1.clone();
    let entry = zip.by_name(&sheet_path).map_err(|e| e.to_string())?;
    // Cap the DECOMPRESSED sheet-XML bytes: a bomb inflates at most this much
    // before quick-xml simply hits EOF on the truncated stream.
    let bounded = entry.take(crate::util::MAX_DECODE_BYTES as u64);

    let mut rows: Vec<Vec<String>> = Vec::new();
    parse_sheet_xml(bounded, &sst, || false, |mut row| {
        row.truncate(max_cols);
        rows.push(row);
        rows.len() >= max_rows
    });
    Ok(rows)
}

fn stream_sheet(
    file: &str,
    sheet_path: &str,
    sst: &[String],
    data: &Arc<Mutex<SheetData>>,
    stop: &Arc<AtomicBool>,
) {
    let Ok(f) = File::open(file) else { return };
    let Ok(mut zip) = zip::ZipArchive::new(f) else {
        return;
    };
    let Ok(entry) = zip.by_name(sheet_path) else {
        return;
    };

    parse_sheet_xml(
        entry,
        sst,
        || stop.load(Ordering::Relaxed),
        |row| {
            let mut d = data.lock().unwrap();
            d.ncols = d.ncols.max(row.len());
            d.rows.push(row);
            if d.rows.len() >= ROW_CAP {
                d.capped = true;
                d.done = true;
                true
            } else {
                false
            }
        },
    );

    if let Ok(mut d) = data.lock() {
        d.done = true;
    }
}

/// Shared worksheet-XML row parser used by both the streaming loader and the
/// bounded preview. Reads `<row>`/`<c>` elements from `reader`, resolving shared
/// strings against `sst`, and hands each completed row to `on_row`. Stops when
/// `on_row` returns `true` (the consumer is full) or `should_abort` returns
/// `true` (checked per event, so the streaming worker can cancel promptly when
/// the user switches sheets), or at EOF. Keeping the cell/row decoding here
/// means the interactive grid and the preview can never disagree (ADR 0009).
fn parse_sheet_xml<R, A, F>(reader: R, sst: &[String], should_abort: A, mut on_row: F)
where
    R: Read,
    A: Fn() -> bool,
    F: FnMut(Vec<String>) -> bool,
{
    let mut rd = Reader::from_reader(BufReader::with_capacity(1 << 20, reader));
    let mut buf = Vec::new();

    let mut row: Vec<String> = Vec::new();
    let mut col = 0usize;
    let mut ctype: u8 = b'n'; // 's' shared, 'i' inline, else literal
    let mut val = String::new();
    let mut in_v = false;
    let mut in_t = false;

    loop {
        if should_abort() {
            return;
        }
        match rd.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => match e.name().as_ref() {
                b"row" => {
                    row = Vec::new();
                    col = 0;
                }
                b"c" => {
                    ctype = b'n';
                    let mut cref: Option<Vec<u8>> = None;
                    for a in e.attributes().flatten() {
                        match a.key.as_ref() {
                            b"r" => cref = Some(a.value.into_owned()),
                            b"t" => {
                                ctype = match a.value.as_ref() {
                                    b"s" => b's',
                                    b"inlineStr" => b'i',
                                    b"str" => b'l',
                                    b"b" => b'b',
                                    _ => b'n',
                                }
                            }
                            _ => {}
                        }
                    }
                    if let Some(r) = cref {
                        col = col_index(&r);
                    }
                    while row.len() < col {
                        row.push(String::new());
                    }
                    val.clear();
                }
                b"v" => {
                    in_v = true;
                    val.clear();
                }
                b"t" => in_t = true,
                _ => {}
            },
            Ok(Event::Text(t)) => {
                if in_v || in_t {
                    val.push_str(&crate::util::xml_text(&t));
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if in_v || in_t {
                    val.push_str(&crate::util::xml_ref(&r));
                }
            }
            Ok(Event::End(e)) => match e.name().as_ref() {
                b"v" => in_v = false,
                b"t" => in_t = false,
                b"c" => {
                    let resolved = match ctype {
                        b's' => val
                            .trim()
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| sst.get(i).cloned())
                            .unwrap_or_default(),
                        b'b' => {
                            if val.trim() == "1" {
                                "TRUE".into()
                            } else {
                                "FALSE".into()
                            }
                        }
                        _ => val.clone(),
                    };
                    // col was set (or defaults to next); place and advance
                    if row.len() <= col {
                        row.resize(col + 1, String::new());
                    }
                    row[col] = resolved;
                    col += 1;
                }
                b"row" => {
                    if on_row(std::mem::take(&mut row)) {
                        return;
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    // Benchmark against a large local workbook. Point SUCHER_BIG at a file:
    //   SUCHER_BIG=/path/big.xlsx cargo test --release big_xlsx -- --ignored --nocapture
    #[test]
    #[ignore]
    fn big_xlsx() {
        let Ok(path) = std::env::var("SUCHER_BIG") else {
            eprintln!("set SUCHER_BIG=/path/to/file.xlsx to run this benchmark");
            return;
        };
        if !std::path::Path::new(&path).exists() {
            eprintln!("SUCHER_BIG file not found: {path}");
            return;
        }
        let t = Instant::now();
        let book = StreamBook::open(&path).expect("open");
        println!("open() (sst+workbook parse): {:?}", t.elapsed());
        println!("sheets: {:?}", book.names());

        let t = Instant::now();
        loop {
            let (rows, _, done, _) = book.dims();
            if rows >= 1000 || done {
                println!("time to first {rows} rows: {:?}", t.elapsed());
                break;
            }
        }
        let first = book.window(0, 3, 0, 5);
        println!("first rows sample: {first:?}");

        let t = Instant::now();
        loop {
            let (rows, ncols, done, capped) = book.dims();
            if done {
                println!(
                    "done: {rows} rows x {ncols} cols, capped={capped}, in {:?}",
                    t.elapsed()
                );
                break;
            }
        }

        // search across the fully-loaded sheet
        let t = Instant::now();
        let hits = book.find("REACH");
        println!("find(\"REACH\"): {} hits in {:?}", hits.len(), t.elapsed());
        let t = Instant::now();
        let hits = book.find("zzznotfoundzzz");
        println!("find(miss): {} hits in {:?}", hits.len(), t.elapsed());
    }

    #[test]
    fn ci() {
        assert!(contains_ci("Hello World", "world"));
        assert!(contains_ci("ABC", "abc"));
        assert!(!contains_ci("abc", "xyz"));
        assert!(contains_ci("anything", ""));
    }

    // Collect every row the shared parser yields from an inline sheet-XML blob.
    fn parse_all(xml: &str, sst: &[String]) -> Vec<Vec<String>> {
        let mut rows = Vec::new();
        parse_sheet_xml(xml.as_bytes(), sst, || false, |row| {
            rows.push(row);
            false
        });
        rows
    }

    #[test]
    fn parses_shared_inline_and_typed_cells() {
        // A shared-string ref (t="s"), an inline string (t="inlineStr"), a
        // number, and a boolean — the four cell shapes the resolver handles.
        let sst = vec!["Alpha".to_string(), "Beta".to_string()];
        let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\" t=\"s\"><v>1</v></c>\
            <c r=\"B1\" t=\"inlineStr\"><is><t>Inline</t></is></c></row>\
            <row r=\"2\"><c r=\"A2\" t=\"n\"><v>42</v></c>\
            <c r=\"B2\" t=\"b\"><v>1</v></c></row>\
            </sheetData></worksheet>";
        let rows = parse_all(xml, &sst);
        assert_eq!(
            rows,
            vec![
                vec!["Beta".to_string(), "Inline".to_string()],
                vec!["42".to_string(), "TRUE".to_string()],
            ]
        );
    }

    #[test]
    fn parser_stops_when_on_row_signals_full() {
        // Returning `true` from on_row halts parsing immediately — the mechanism
        // both the ROW_CAP and the preview cap rely on.
        let xml = "<worksheet><sheetData>\
            <row r=\"1\"><c r=\"A1\" t=\"n\"><v>1</v></c></row>\
            <row r=\"2\"><c r=\"A2\" t=\"n\"><v>2</v></c></row>\
            <row r=\"3\"><c r=\"A3\" t=\"n\"><v>3</v></c></row>\
            </sheetData></worksheet>";
        let mut rows = Vec::new();
        parse_sheet_xml(xml.as_bytes(), &[], || false, |row| {
            rows.push(row);
            rows.len() >= 2 // stop after two rows
        });
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1], vec!["2".to_string()]);
    }

    #[test]
    fn preview_rows_reads_and_caps() {
        // samples/sample.xlsx sheet1 is A1:D5 (5 rows x 4 cols, inline strings).
        let g = preview_rows("samples/sample.xlsx", 2, 3).expect("first worksheet rows");
        assert_eq!(g.len(), 2, "row cap");
        assert!(g.iter().all(|r| r.len() <= 3), "col cap");
        assert_eq!(g[0], vec!["Item", "Qty", "Unit"]);
        assert_eq!(g[1], vec!["Coffee", "3", "4.5"]);
    }
}
