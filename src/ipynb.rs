// Jupyter notebook (.ipynb) -> markdown. An .ipynb is a JSON document: a `cells`
// array of markdown / code cells, each carrying `source` and (for code) a list of
// execution `outputs`. It reduces cleanly to markdown — markdown cells pass
// through verbatim, code cells become fenced blocks in the notebook's language,
// text outputs are shown, and image outputs are sent to the viewer's image
// gallery — so the existing markdown TUI renders the notebook with no new UI
// (mirrors docx/pptx/epub: `to_markdown` + `media`).
//
// The transformation is factored into PURE functions (`notebook_to_markdown`,
// `cell_source`, `strip_ansi`, `b64_decode`, …) unit-tested against inline JSON
// and known vectors; `to_markdown` and `media` are the thin file-IO wrappers.

use crate::util::{read_to_string_capped, MAX_DECODE_BYTES};
use serde_json::Value;
use std::fs::File;
use std::path::PathBuf;

/// Embedded raster images from a notebook's code-cell outputs (`image/png` /
/// `image/jpeg`), base64-decoded to temp files for the viewer's image gallery, in
/// document order. Empty when the notebook has none.
pub fn media(path: &str) -> Vec<PathBuf> {
    let Ok(file) = File::open(path) else {
        return Vec::new();
    };
    // Byte-cap the read (ADR 0009): a hostile notebook could otherwise inflate a
    // huge base64 blob through `read_to_string`.
    let Ok(text) = read_to_string_capped(file, MAX_DECODE_BYTES) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let images = collect_images(&v);
    if images.is_empty() {
        return Vec::new();
    }
    let dir = std::env::temp_dir().join(format!("sucher-media-{}", std::process::id()));
    if std::fs::create_dir_all(&dir).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, (ext, b64)) in images.iter().enumerate() {
        // Skip anything that isn't valid base64 or that decodes beyond the cap —
        // a bounded, best-effort extraction (ADR 0009).
        let Some(bytes) = b64_decode(b64) else {
            continue;
        };
        if bytes.is_empty() || bytes.len() > MAX_DECODE_BYTES {
            continue;
        }
        let dest = dir.join(format!("ipynb-{i}.{ext}"));
        if std::fs::write(&dest, &bytes).is_ok() {
            out.push(dest);
        }
    }
    out
}

pub fn to_markdown(path: &str) -> Result<String, String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    // Byte-cap the decompressed source (ADR 0009): the reader `take`s the cap, so
    // a giant notebook is rejected rather than OOM-ing on parse.
    let text = read_to_string_capped(file, MAX_DECODE_BYTES)?;
    let v: Value =
        serde_json::from_str(&text).map_err(|e| format!("not a notebook (invalid JSON): {e}"))?;
    if v.get("cells").and_then(|c| c.as_array()).is_none() {
        return Err("not a notebook (no cells array)".to_string());
    }
    let md = notebook_to_markdown(&v);
    if md.trim().is_empty() {
        return Err("notebook has no readable content".to_string());
    }
    Ok(md)
}

/// The notebook's code language for fenced blocks: `metadata.language_info.name`,
/// then `metadata.kernelspec.language`, defaulting to `python`. PURE.
fn code_language(v: &Value) -> String {
    let meta = v.get("metadata");
    meta.and_then(|m| m.get("language_info"))
        .and_then(|l| l.get("name"))
        .and_then(|n| n.as_str())
        .or_else(|| {
            meta.and_then(|m| m.get("kernelspec"))
                .and_then(|k| k.get("language"))
                .and_then(|l| l.as_str())
        })
        .unwrap_or("python")
        .to_string()
}

/// Reduce a parsed notebook to markdown. PURE — unit-tested against inline JSON.
/// Cells are emitted in order separated by a blank line; the *total* output is
/// bounded (ADR 0009): a notebook with thousands of cells — each under the cap —
/// could still blow up unbounded, so we stop appending past the cap and mark it.
fn notebook_to_markdown(v: &Value) -> String {
    let lang = code_language(v);
    let mut out = String::new();
    let Some(cells) = v.get("cells").and_then(|c| c.as_array()) else {
        return out;
    };
    let mut truncated = false;
    for cell in cells {
        let block = cell_markdown(cell, &lang);
        if block.trim().is_empty() {
            continue;
        }
        out.push_str(block.trim_end());
        out.push_str("\n\n");
        if out.len() > MAX_DECODE_BYTES {
            truncated = true;
            break;
        }
    }
    if truncated {
        out.push_str("… (truncated)\n");
    }
    out
}

/// One cell reduced to markdown. A `markdown` cell passes through verbatim; a
/// `code` cell becomes a fenced block in `lang` followed by its rendered outputs;
/// any other cell type (e.g. `raw`) yields nothing. PURE.
fn cell_markdown(cell: &Value, lang: &str) -> String {
    let cell_type = cell.get("cell_type").and_then(|t| t.as_str()).unwrap_or("");
    let source = cell_source(cell);
    match cell_type {
        "markdown" => source,
        "code" => {
            let outputs = cell.get("outputs").and_then(|o| o.as_array());
            let has_outputs = outputs.is_some_and(|o| !o.is_empty());
            if source.trim().is_empty() && !has_outputs {
                return String::new();
            }
            let mut s = format!("```{lang}\n{}\n```\n", source.trim_end_matches('\n'));
            if let Some(outputs) = outputs {
                for output in outputs {
                    let rendered = output_markdown(output);
                    if !rendered.is_empty() {
                        s.push('\n');
                        s.push_str(&rendered);
                    }
                }
            }
            s
        }
        _ => String::new(),
    }
}

/// A code cell's `outputs` entry reduced to markdown. PURE. `stream` output and
/// `execute_result`/`display_data` `text/plain` become fenced text blocks; a
/// `text/plain`-less image result becomes an `_[image output]_` marker (the pixels
/// go to [`media`]); an `error` becomes its ANSI-stripped traceback (or
/// `ename: evalue`). Unknown output types yield nothing.
fn output_markdown(output: &Value) -> String {
    match output.get("output_type").and_then(|t| t.as_str()) {
        Some("stream") => output_text_block(&value_text(output.get("text"))),
        Some("execute_result") | Some("display_data") => {
            let mut s = String::new();
            if let Some(data) = output.get("data") {
                if let Some(text) = data.get("text/plain") {
                    s.push_str(&output_text_block(&value_text(Some(text))));
                }
                if data.get("image/png").is_some() || data.get("image/jpeg").is_some() {
                    if !s.is_empty() {
                        s.push('\n');
                    }
                    s.push_str("_[image output]_\n");
                }
            }
            s
        }
        Some("error") => {
            let text = match output.get("traceback").and_then(|t| t.as_array()) {
                Some(lines) => {
                    let joined = lines
                        .iter()
                        .filter_map(|l| l.as_str())
                        .collect::<Vec<_>>()
                        .join("\n");
                    strip_ansi(&joined)
                }
                None => {
                    let ename = output.get("ename").and_then(|e| e.as_str()).unwrap_or("");
                    let evalue = output.get("evalue").and_then(|e| e.as_str()).unwrap_or("");
                    format!("{ename}: {evalue}")
                }
            };
            output_text_block(&text)
        }
        _ => String::new(),
    }
}

/// Wrap execution output in a fenced `text` block so its content is shown plainly
/// and never re-interpreted as markdown. Empty (after trimming) → nothing.
fn output_text_block(text: &str) -> String {
    let trimmed = text.trim_end_matches('\n');
    if trimmed.trim().is_empty() {
        return String::new();
    }
    format!("```text\n{trimmed}\n```\n")
}

/// A cell's `source` joined to a string. PURE.
fn cell_source(cell: &Value) -> String {
    value_text(cell.get("source"))
}

/// nbformat allows a `source`/`text`/image field to be either a single JSON
/// string or an array of line strings (each already carrying its own newline), so
/// the array form is concatenated — not joined with `\n`. Absent / other → empty.
fn value_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(lines)) => lines.iter().filter_map(|l| l.as_str()).collect(),
        _ => String::new(),
    }
}

/// Every code-cell image output as `(extension, base64)` in document order:
/// `image/png` → `png`, `image/jpeg` → `jpg`. PURE — the base64 is decoded and
/// written by [`media`].
fn collect_images(v: &Value) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    let Some(cells) = v.get("cells").and_then(|c| c.as_array()) else {
        return out;
    };
    for cell in cells {
        let Some(outputs) = cell.get("outputs").and_then(|o| o.as_array()) else {
            continue;
        };
        for output in outputs {
            let Some(data) = output.get("data") else {
                continue;
            };
            for (key, ext) in [("image/png", "png"), ("image/jpeg", "jpg")] {
                if let Some(img) = data.get(key) {
                    let b64 = value_text(Some(img));
                    if !b64.trim().is_empty() {
                        out.push((ext, b64));
                    }
                }
            }
        }
    }
    out
}

/// Strip ANSI CSI escape sequences (`\x1b[…<final>`, final byte `0x40..=0x7e`)
/// from a string. Notebook error tracebacks are ANSI-coloured; the markdown
/// renderer would otherwise show the raw escapes as mojibake. PURE — unit-tested.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // A CSI sequence is `ESC [ … final`; consume up to and incl. the final
            // byte. A lone ESC (or other escape) just drops the ESC itself.
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c2) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Decode standard-alphabet base64 (`A-Za-z0-9+/`, `=` padding), IGNORING
/// whitespace/newlines since notebook base64 image data is line-wrapped. Returns
/// `None` on any non-alphabet, non-whitespace, non-padding byte. PURE — unit-tested.
fn b64_decode(s: &str) -> Option<Vec<u8>> {
    fn sextet(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a' + 26) as u32),
            b'0'..=b'9' => Some((c - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for &b in s.as_bytes() {
        match b {
            b'=' => break, // padding is always trailing — stop.
            _ if b.is_ascii_whitespace() => continue,
            _ => {
                buf = (buf << 6) | sextet(b)?;
                bits += 6;
                if bits >= 8 {
                    bits -= 8;
                    out.push((buf >> bits) as u8);
                }
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn b64_decodes_known_vectors() {
        assert_eq!(b64_decode("TWFu").unwrap(), b"Man");
        assert_eq!(b64_decode("").unwrap(), b"");
        // Padding cases (RFC 4648): one and two `=`.
        assert_eq!(b64_decode("TWE=").unwrap(), b"Ma");
        assert_eq!(b64_decode("TQ==").unwrap(), b"M");
    }

    #[test]
    fn b64_ignores_whitespace() {
        // Notebook base64 is line-wrapped; newlines/spaces must not corrupt it.
        assert_eq!(b64_decode("TW\nFu").unwrap(), b"Man");
        assert_eq!(b64_decode("  TWFu  ").unwrap(), b"Man");
    }

    #[test]
    fn b64_rejects_invalid_char() {
        assert_eq!(b64_decode("****"), None);
        assert_eq!(b64_decode("TW.Fu"), None);
    }

    #[test]
    fn strip_ansi_removes_csi_sequences() {
        // A coloured traceback fragment: two SGR sequences bracketing the text.
        assert_eq!(strip_ansi("\x1b[0;31mError\x1b[0m"), "Error");
        // No escapes → unchanged.
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn markdown_cell_passes_through() {
        let nb = json!({"cells": [
            {"cell_type": "markdown", "source": ["# Title\n", "\n", "Some **text**.\n"]}
        ]});
        let md = notebook_to_markdown(&nb);
        assert!(md.contains("# Title"), "heading: {md}");
        assert!(md.contains("Some **text**."), "body: {md}");
        // No code fence for a pure markdown cell.
        assert!(!md.contains("```"), "unexpected fence: {md}");
    }

    #[test]
    fn code_cell_becomes_fenced_block_in_language() {
        let nb = json!({
            "metadata": {"language_info": {"name": "rust"}},
            "cells": [{"cell_type": "code", "source": "let x = 1;", "outputs": []}]
        });
        let md = notebook_to_markdown(&nb);
        assert!(md.contains("```rust\nlet x = 1;\n```"), "fence: {md}");
    }

    #[test]
    fn language_defaults_to_python() {
        let nb = json!({"cells": [{"cell_type": "code", "source": "print(1)", "outputs": []}]});
        assert!(notebook_to_markdown(&nb).contains("```python\n"), "default lang");
    }

    #[test]
    fn stream_output_is_included() {
        let nb = json!({"cells": [{
            "cell_type": "code",
            "source": "print('hi')",
            "outputs": [{"output_type": "stream", "name": "stdout", "text": ["hi\n"]}]
        }]});
        let md = notebook_to_markdown(&nb);
        assert!(md.contains("```text\nhi\n```"), "stream output: {md}");
    }

    #[test]
    fn execute_result_text_plain_is_included() {
        let nb = json!({"cells": [{
            "cell_type": "code",
            "source": "1 + 1",
            "outputs": [{
                "output_type": "execute_result",
                "data": {"text/plain": "2"}
            }]
        }]});
        assert!(notebook_to_markdown(&nb).contains("```text\n2\n```"), "result text");
    }

    #[test]
    fn image_output_yields_inline_marker_and_is_collected() {
        // "TWFu" is valid base64 ("Man"); enough to exercise both the marker in
        // the markdown and extraction by `collect_images`.
        let nb = json!({"cells": [{
            "cell_type": "code",
            "source": "plot()",
            "outputs": [{
                "output_type": "display_data",
                "data": {"image/png": "TWFu"}
            }]
        }]});
        let md = notebook_to_markdown(&nb);
        assert!(md.contains("_[image output]_"), "marker: {md}");
        let images = collect_images(&nb);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].0, "png");
        assert_eq!(b64_decode(&images[0].1).unwrap(), b"Man");
    }

    #[test]
    fn error_output_is_ansi_stripped() {
        let nb = json!({"cells": [{
            "cell_type": "code",
            "source": "boom()",
            "outputs": [{
                "output_type": "error",
                "ename": "ValueError",
                "evalue": "bad",
                "traceback": ["\x1b[0;31mValueError\x1b[0m: bad"]
            }]
        }]});
        let md = notebook_to_markdown(&nb);
        assert!(md.contains("ValueError: bad"), "traceback: {md}");
        assert!(!md.contains('\x1b'), "escapes leaked: {md}");
    }

    #[test]
    fn source_as_string_and_array_both_join() {
        let as_string = json!({"cell_type": "markdown", "source": "one line"});
        let as_array = json!({"cell_type": "markdown", "source": ["line 1\n", "line 2\n"]});
        assert_eq!(cell_source(&as_string), "one line");
        assert_eq!(cell_source(&as_array), "line 1\nline 2\n");
    }
}
