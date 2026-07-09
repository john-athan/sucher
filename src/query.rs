//! Smart-query parsing for the directory filter.
//!
//! A raw query mixes free text with structured predicates:
//!   `report kind:pdf size:>1mb modified:<7d ext:rs`
//! Free-text tokens become the fuzzy `terms` (a subsequence match on the name);
//! recognized `key:value` tokens become predicates applied to entry metadata. An
//! entry matches when every structured predicate passes AND the free-text terms
//! subsequence-match its name.

use crate::format::Format;
use std::time::{Duration, SystemTime};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    fn test<T: PartialOrd>(self, a: T, b: T) -> bool {
        match self {
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
        }
    }
}

/// Map a `kind:`/`type:` alias to the concrete formats it selects. One alias may
/// cover several formats (e.g. `doc` → both Word variants). An unrecognized alias
/// yields an empty vec, signalling the caller to fall the token back to free text.
fn parse_kind(s: &str) -> Vec<Format> {
    match s {
        "folder" | "dir" | "directory" => vec![Format::Directory],
        "image" | "img" | "photo" => vec![Format::Image],
        "video" | "movie" | "vid" => vec![Format::Video],
        "audio" | "music" | "sound" => vec![Format::Audio],
        "pdf" => vec![Format::Pdf],
        "sheet" | "spreadsheet" | "csv" | "excel" => vec![Format::Sheet],
        "code" | "source" | "text" | "txt" => vec![Format::Text],
        "markdown" | "md" => vec![Format::Markdown],
        "doc" | "document" | "word" => vec![Format::Docx, Format::Doc],
        "docx" => vec![Format::Docx],
        "epub" | "ebook" | "book" => vec![Format::Epub],
        "ipynb" | "notebook" => vec![Format::Ipynb],
        "archive" | "zip" => vec![Format::Archive],
        "binary" | "bin" => vec![Format::Binary],
        _ => Vec::new(),
    }
}

/// A parsed query: free-text terms plus structured predicates.
#[derive(Clone)]
pub struct Query {
    terms: String,
    kinds: Vec<Format>,
    exts: Vec<String>,
    size: Option<(Cmp, u64)>,
    /// Age comparison: e.g. `<7d` means "younger than 7 days".
    age: Option<(Cmp, Duration)>,
    /// A `content:` term: a literal substring to look for *inside* files. Unlike
    /// every other predicate this needs the file's bytes, so it is **not** tested
    /// by [`Query::matches`] (which stays pure, metadata-only — ADR 0007 D2). Only
    /// recursive search reads it, via [`Query::content`], and scans the file after
    /// the cheap metadata predicates already passed. Inert for the local filter.
    content: Option<String>,
}

/// Parse a raw filter string. A `key:value` token with a recognized key becomes a
/// predicate; anything else (plain words, unknown keys such as a URL with a colon,
/// or a recognized key with an unparseable value) joins the free-text `terms`.
pub fn parse(raw: &str) -> Query {
    let mut terms: Vec<&str> = Vec::new();
    let mut kinds = Vec::new();
    let mut exts = Vec::new();
    let mut size = None;
    let mut age = None;
    let mut content = None;

    for token in raw.split_whitespace() {
        let Some((key, value)) = token.split_once(':') else {
            terms.push(token);
            continue;
        };
        if value.is_empty() {
            terms.push(token);
            continue;
        }
        match key.to_ascii_lowercase().as_str() {
            "kind" | "type" => {
                let mapped = parse_kind(&value.to_ascii_lowercase());
                if mapped.is_empty() {
                    terms.push(token); // unknown kind → treat as text
                } else {
                    kinds.extend(mapped);
                }
            }
            "ext" => exts.push(value.trim_start_matches('.').to_ascii_lowercase()),
            "size" => match parse_cmp(value, parse_size) {
                Some(p) => size = Some(p),
                None => terms.push(token),
            },
            "modified" | "date" | "age" => match parse_cmp(value, parse_duration) {
                Some(p) => age = Some(p),
                None => terms.push(token),
            },
            // `content:`/`contains:` — a literal substring to grep for inside
            // files (ADR 0007 D2/D4). The last one wins if repeated. The value is
            // taken verbatim (case folding is the searcher's smart-case job, not
            // the parser's), so `content:Foo` and `content:foo` differ here and
            // the searcher decides case sensitivity.
            "content" | "contains" | "grep" => content = Some(value.to_string()),
            _ => terms.push(token), // unknown key → treat as text (e.g. URLs)
        }
    }

    Query {
        // `terms` is stored canonically lower-cased so `matches` needn't re-fold it
        // on every entry (it was the hottest per-entry allocation in filter mode).
        terms: terms.join(" ").to_lowercase(),
        kinds,
        exts,
        size,
        age,
        content,
    }
}

impl Query {
    /// True when at least one structured predicate was given (no fuzzy text).
    pub fn has_predicates(&self) -> bool {
        !self.kinds.is_empty()
            || !self.exts.is_empty()
            || self.size.is_some()
            || self.age.is_some()
            || self.content.is_some()
    }

    /// The `content:` substring to grep for inside files, if any (ADR 0007 D2).
    /// The recursive-search walker reads this *after* [`Query::matches`] has
    /// passed on cheap metadata, and only then opens the file to scan its bytes.
    pub fn content(&self) -> Option<&str> {
        self.content.as_deref()
    }

    /// True when nothing at all was asked for — no free-text terms, no metadata
    /// predicates, no `content:`. Recursive search uses this to avoid walking the
    /// entire tree for a blank query: an empty query matches everything, which as
    /// a *search* means "you haven't asked yet," not "list the whole disk."
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty() && !self.has_predicates()
    }

    /// Does the entry pass every structured predicate AND the free-text terms?
    pub fn matches(
        &self,
        name: &str,
        format: Format,
        size: u64,
        modified: Option<SystemTime>,
    ) -> bool {
        if !self.kinds.is_empty() && !self.kinds.contains(&format) {
            return false;
        }
        if !self.exts.is_empty() {
            let ext = name
                .rsplit_once('.')
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            if !self.exts.contains(&ext) {
                return false;
            }
        }
        if let Some((cmp, value)) = self.size {
            if !cmp.test(size, value) {
                return false;
            }
        }
        if let Some((cmp, dur)) = self.age {
            let item_age = modified.and_then(|t| t.elapsed().ok());
            match item_age {
                Some(a) if cmp.test(a, dur) => {}
                _ => return false,
            }
        }
        // `self.terms` is already lower-cased (see `parse`); only the entry name
        // still needs folding here.
        self.terms.is_empty() || subsequence(&self.terms, &name.to_lowercase())
    }
}

/// Parse a comparison value like `>100mb` / `<=7d` into `(Cmp, T)` using `unit`.
fn parse_cmp<T>(value: &str, unit: impl Fn(&str) -> Option<T>) -> Option<(Cmp, T)> {
    let (cmp, rest) = if let Some(r) = value.strip_prefix(">=") {
        (Cmp::Ge, r)
    } else if let Some(r) = value.strip_prefix("<=") {
        (Cmp::Le, r)
    } else if let Some(r) = value.strip_prefix('>') {
        (Cmp::Gt, r)
    } else if let Some(r) = value.strip_prefix('<') {
        (Cmp::Lt, r)
    } else {
        // bare value (e.g. `size:1mb`) → treat as "at least".
        (Cmp::Ge, value)
    };
    unit(rest).map(|v| (cmp, v))
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.trim().parse().ok()?;
    let mult = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" => 1024.0,
        "m" | "mb" => 1024.0 * 1024.0,
        "g" | "gb" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" => 1024.0_f64.powi(4),
        _ => return None,
    };
    Some((num * mult) as u64)
}

fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let num: f64 = num.trim().parse().ok()?;
    let secs = match unit.trim() {
        "s" | "sec" => 1.0,
        "m" | "min" => 60.0,
        "h" | "hr" | "hour" => 3600.0,
        "d" | "day" => 86_400.0,
        "w" | "week" => 604_800.0,
        "mo" | "month" => 2_592_000.0,
        "y" | "year" => 31_536_000.0,
        _ => return None,
    };
    Some(Duration::from_secs_f64(num * secs))
}

/// Is `needle` a subsequence of `haystack`? (cheap fuzzy match)
pub fn subsequence(needle: &str, haystack: &str) -> bool {
    let mut h = haystack.chars();
    needle.chars().all(|nc| h.any(|hc| hc == nc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_and_predicates() {
        let q = parse("report kind:pdf size:>1mb modified:<7d");
        assert_eq!(q.terms, "report");
        assert!(q.has_predicates());
        assert!(matches!(q.size, Some((Cmp::Gt, _))));
    }

    #[test]
    fn mixed_case_query_is_lowercased_at_parse() {
        // `terms` is folded once in `parse`, so a mixed-case query still matches.
        let q = parse("RePo");
        assert_eq!(q.terms, "repo"); // canonically lower-cased
        assert!(q.matches("myrepo123", Format::Text, 0, None));
    }

    #[test]
    fn size_and_kind_filtering() {
        let q = parse("kind:image size:>100kb");
        // a 200kb image passes
        assert!(q.matches("a.png", Format::Image, 200 * 1024, None));
        // a 10kb image fails the size predicate
        assert!(!q.matches("a.png", Format::Image, 10 * 1024, None));
        // a 200kb text file fails the kind predicate
        assert!(!q.matches("a.txt", Format::Text, 200 * 1024, None));
    }

    #[test]
    fn kind_pdf_matches_pdf_not_text() {
        let q = parse("kind:pdf");
        assert!(q.matches("report.pdf", Format::Pdf, 0, None));
        assert!(!q.matches("notes.txt", Format::Text, 0, None));
    }

    #[test]
    fn kind_doc_matches_docx_and_doc() {
        let q = parse("kind:doc");
        assert!(q.matches("a.docx", Format::Docx, 0, None));
        assert!(q.matches("a.doc", Format::Doc, 0, None));
        assert!(!q.matches("a.pdf", Format::Pdf, 0, None));
    }

    #[test]
    fn ext_filtering() {
        let q = parse("ext:rs");
        assert!(q.matches("main.rs", Format::Text, 0, None));
        assert!(!q.matches("main.py", Format::Text, 0, None));
    }

    #[test]
    fn modified_younger_than() {
        let q = parse("modified:<7d");
        let recent = SystemTime::now() - Duration::from_secs(86_400); // 1 day old
        let old = SystemTime::now() - Duration::from_secs(30 * 86_400); // 30 days old
        assert!(q.matches("a.txt", Format::Text, 0, Some(recent)));
        assert!(!q.matches("a.txt", Format::Text, 0, Some(old)));
        // A missing timestamp fails an age predicate.
        assert!(!q.matches("a.txt", Format::Text, 0, None));
    }

    #[test]
    fn terms_and_predicate_together() {
        let q = parse("report kind:pdf");
        // name subsequence-matches "report" AND is a Pdf.
        assert!(q.matches("annual-report.pdf", Format::Pdf, 0, None));
        // right kind, but the name doesn't contain "report".
        assert!(!q.matches("budget.pdf", Format::Pdf, 0, None));
        // matches the terms, but wrong kind.
        assert!(!q.matches("report.txt", Format::Text, 0, None));
    }

    #[test]
    fn bare_word_is_just_text() {
        let q = parse("kind"); // no colon
        assert_eq!(q.terms, "kind");
        assert!(!q.has_predicates());
    }

    #[test]
    fn unknown_kind_becomes_free_text() {
        let q = parse("kind:whatever");
        assert_eq!(q.terms, "kind:whatever");
        assert!(!q.has_predicates());
    }

    #[test]
    fn unparseable_size_falls_back_to_text() {
        let q = parse("size:huge");
        assert_eq!(q.terms, "size:huge");
        assert!(!q.has_predicates());
    }

    #[test]
    fn unknown_key_is_free_text() {
        // A URL with a colon is not a recognized key → free text.
        let q = parse("http://example.com");
        assert_eq!(q.terms, "http://example.com");
        assert!(!q.has_predicates());
    }

    #[test]
    fn content_predicate_is_parsed_and_exposed() {
        let q = parse("content:TODO");
        assert_eq!(q.content(), Some("TODO"));
        assert!(q.has_predicates());
        // content: is not a free-text term (it doesn't fuzzy-match the name).
        assert_eq!(q.terms, "");
        // `contains:` and `grep:` are aliases.
        assert_eq!(parse("contains:foo").content(), Some("foo"));
        assert_eq!(parse("grep:bar").content(), Some("bar"));
    }

    #[test]
    fn content_value_is_case_preserving() {
        // The parser preserves case; smart-case is the searcher's job (ADR 0007 D4).
        assert_eq!(parse("content:Foo").content(), Some("Foo"));
        assert_eq!(parse("content:foo").content(), Some("foo"));
    }

    #[test]
    fn content_is_metadata_only_in_matches() {
        // `matches` stays pure metadata (ADR 0007 D2): a content-only query does
        // not reject any entry on name/kind/size/age — the walker greps the file.
        let q = parse("content:needle");
        assert!(q.matches("anything.rs", Format::Text, 0, None));
        assert!(q.matches("other.pdf", Format::Pdf, 999, None));
    }

    #[test]
    fn content_combines_with_metadata_predicates() {
        let q = parse("content:fn kind:code ext:rs");
        assert_eq!(q.content(), Some("fn"));
        // metadata predicates still gate `matches`; content is separate.
        assert!(q.matches("main.rs", Format::Text, 0, None));
        assert!(!q.matches("main.py", Format::Text, 0, None)); // wrong ext
        assert!(!q.matches("a.pdf", Format::Pdf, 0, None)); // wrong kind
    }

    #[test]
    fn empty_query_is_empty_but_any_predicate_is_not() {
        assert!(parse("").is_empty());
        assert!(parse("   ").is_empty());
        assert!(!parse("report").is_empty()); // free text
        assert!(!parse("kind:pdf").is_empty()); // metadata predicate
        assert!(!parse("content:x").is_empty()); // content predicate
    }

    #[test]
    fn empty_content_value_falls_back_to_text() {
        // `content:` with no value is not a predicate (the shared empty-value
        // guard in `parse`), so it degrades to a free-text token.
        let q = parse("content:");
        assert_eq!(q.content(), None);
        assert_eq!(q.terms, "content:");
    }
}
