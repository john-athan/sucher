// Lightweight, dependency-free syntax highlighting.
//
// A pure classifier: given a file's [`lang_key`] it picks a [`Syntax`], and given
// text it splits each line into [`Token`]s tagged with a [`TokenKind`]. It has
// no colours, no IO, and no dependencies: the UI layer maps [`TokenKind`] to
// colours (see `crate::theme::token_color`). Dependency flows theme -> highlight,
// never the reverse.

use std::path::Path;

/// A classified token in a highlighted text preview. The UI maps these to colours.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TokenKind {
    Plain,
    Keyword,
    Str,
    Comment,
    Number,
}

/// A run of text sharing one [`TokenKind`]. Named `Token` to avoid confusion
/// with `ratatui::text::Span`, into which the UI converts these.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Token {
    pub text: String,
    pub kind: TokenKind,
}

/// A multi-line span: an opening delimiter, a closing delimiter, and the
/// [`TokenKind`] the whole span (including both delimiters) is coloured as. A
/// block comment (`/* */`) and a multi-line string (triple-quoted, a template
/// literal) are the same mechanism with a different `kind`: a docstring is a
/// language fact, not a special case bolted onto comment handling.
#[derive(Clone, Copy)]
pub struct Block {
    open: &'static str,
    close: &'static str,
    kind: TokenKind,
}

/// A minimal syntax description: enough to colour strings, comments, numbers, keywords.
#[derive(Clone, Copy)]
pub struct Syntax {
    line_comments: &'static [&'static str],
    /// Multi-line spans (block comments, triple-quoted strings, template
    /// literals), tried in order before line comments / single-char strings so
    /// e.g. a triple quote wins over a lone quote.
    blocks: &'static [Block],
    strings: &'static [char],
    keywords: &'static [&'static str],
    /// When true, keyword matching is case-insensitive (Dockerfile: `from` and
    /// `FROM` are both the instruction).
    keywords_ci: bool,
}

/// Plain text: basic string detection only, no keywords/comments.
pub const PLAIN: Syntax = Syntax {
    line_comments: &[],
    blocks: &[],
    strings: &[],
    keywords: &[],
    keywords_ci: false,
};

const NO_BLOCKS: &[Block] = &[];
const C_BLOCK: &[Block] = &[Block {
    open: "/*",
    close: "*/",
    kind: TokenKind::Comment,
}];
const HTML_BLOCK: &[Block] = &[Block {
    open: "<!--",
    close: "-->",
    kind: TokenKind::Comment,
}];
const LUA_BLOCK: &[Block] = &[Block {
    open: "--[[",
    close: "]]",
    kind: TokenKind::Comment,
}];
const RUBY_BLOCK: &[Block] = &[Block {
    open: "=begin",
    close: "=end",
    kind: TokenKind::Comment,
}];
// Python docstrings: both quote styles span lines and are string content, not
// code. The reported bug (`is` reads as a keyword inside a triple-quoted
// string) is exactly this span being tokenised as ordinary code today.
const PY_BLOCKS: &[Block] = &[
    Block {
        open: "\"\"\"",
        close: "\"\"\"",
        kind: TokenKind::Str,
    },
    Block {
        open: "'''",
        close: "'''",
        kind: TokenKind::Str,
    },
];
// JS/TS template literals legitimately span lines, so the backtick moves from
// the single-char string list to a block (see `syntax_for`).
const JS_TS_BLOCKS: &[Block] = &[
    Block {
        open: "/*",
        close: "*/",
        kind: TokenKind::Comment,
    },
    Block {
        open: "`",
        close: "`",
        kind: TokenKind::Str,
    },
];

const C_KEYWORDS: &[&str] = &[
    "if",
    "else",
    "for",
    "while",
    "return",
    "break",
    "continue",
    "struct",
    "enum",
    "class",
    "public",
    "private",
    "protected",
    "static",
    "const",
    "void",
    "int",
    "char",
    "float",
    "double",
    "bool",
    "true",
    "false",
    "null",
    "new",
    "delete",
    "switch",
    "case",
    "default",
];
const RUST_KEYWORDS: &[&str] = &[
    "fn", "let", "mut", "pub", "struct", "enum", "impl", "trait", "for", "while", "loop", "if",
    "else", "match", "return", "use", "mod", "self", "Self", "crate", "super", "as", "ref", "move",
    "async", "await", "dyn", "where", "type", "const", "static", "true", "false", "Some", "None",
    "Ok", "Err",
];
const PY_KEYWORDS: &[&str] = &[
    "def", "class", "return", "if", "elif", "else", "for", "while", "import", "from", "as", "with",
    "try", "except", "finally", "raise", "lambda", "yield", "True", "False", "None", "and", "or",
    "not", "in", "is", "pass", "break", "continue", "global", "self",
];
const JS_KEYWORDS: &[&str] = &[
    "function",
    "const",
    "let",
    "var",
    "return",
    "if",
    "else",
    "for",
    "while",
    "class",
    "extends",
    "import",
    "export",
    "from",
    "default",
    "new",
    "this",
    "async",
    "await",
    "try",
    "catch",
    "finally",
    "throw",
    "true",
    "false",
    "null",
    "undefined",
    "typeof",
    "switch",
    "case",
];
const GO_KEYWORDS: &[&str] = &[
    "func",
    "package",
    "import",
    "var",
    "const",
    "type",
    "struct",
    "interface",
    "map",
    "chan",
    "go",
    "defer",
    "return",
    "if",
    "else",
    "for",
    "range",
    "switch",
    "case",
    "default",
    "select",
    "nil",
    "true",
    "false",
];
// Docker instructions are case-insensitive in Docker itself (`from alpine` is
// valid); the table is upper case and `Syntax::keywords_ci` handles the rest.
const DOCKER_KEYWORDS: &[&str] = &[
    "FROM",
    "RUN",
    "CMD",
    "LABEL",
    "MAINTAINER",
    "EXPOSE",
    "ENV",
    "ADD",
    "COPY",
    "ENTRYPOINT",
    "VOLUME",
    "USER",
    "WORKDIR",
    "ARG",
    "ONBUILD",
    "STOPSIGNAL",
    "HEALTHCHECK",
    "SHELL",
    "AS",
];

/// The key a file's language and format are looked up by: its lowercased
/// extension, or, for the files whose NAME is their type, a canonical
/// pseudo-extension. PURE, no IO.
///
/// A handful of real-world file families are typed by NAME, not extension
/// (`Dockerfile`, `Makefile`, `.gitignore`, `.env`): keying everything off
/// `Path::extension()` silently drops them (a `.gitignore` has no extension at
/// all). This is the one place that turns a file name into the key every other
/// lookup (`format::classify`, `syntax_for`, `icons::nerd_glyph`/`nerd_color`)
/// shares, so a new named family is one match arm here rather than N call sites.
pub fn lang_key(file_name: &str) -> String {
    let lower = file_name.to_lowercase();
    if lower == "dockerfile" || lower.starts_with("dockerfile.") || lower.ends_with(".dockerfile") {
        return "dockerfile".to_string();
    }
    if lower == "makefile" || lower == "gnumakefile" {
        return "make".to_string();
    }
    if matches!(
        lower.as_str(),
        ".gitignore" | ".dockerignore" | ".npmignore" | ".eslintignore" | ".prettierignore"
    ) {
        return "gitignore".to_string();
    }
    if lower == ".env" || lower.starts_with(".env.") {
        return "env".to_string();
    }
    let ext = Path::new(&lower)
        .extension()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();
    if ext == "mk" {
        return "make".to_string();
    }
    ext
}

/// Pick a [`Syntax`] for a [`lang_key`] (the extension point for code types).
pub fn syntax_for(key: &str) -> Option<Syntax> {
    let s = |line_comments, blocks, strings, keywords| Syntax {
        line_comments,
        blocks,
        strings,
        keywords,
        keywords_ci: false,
    };
    let dq: &[char] = &['"'];
    let dq_sq: &[char] = &['"', '\''];
    let dq_sq_bt: &[char] = &['"', '\'', '`'];
    Some(match key {
        "rs" => s(&["//"], C_BLOCK, &['"'], RUST_KEYWORDS),
        "c" | "h" | "cpp" | "hpp" | "cc" | "java" | "cs" => s(&["//"], C_BLOCK, dq_sq, C_KEYWORDS),
        // Template literals span lines (JS_TS_BLOCKS carries a backtick block);
        // the backtick is dropped from the single-char string list below.
        "js" | "ts" | "tsx" | "jsx" => s(&["//"], JS_TS_BLOCKS, dq_sq, JS_KEYWORDS),
        "go" => s(&["//"], C_BLOCK, dq_sq_bt, GO_KEYWORDS),
        "py" => s(&["#"], PY_BLOCKS, dq_sq, PY_KEYWORDS),
        "rb" => s(&["#"], RUBY_BLOCK, dq_sq, PY_KEYWORDS),
        "sh" | "bash" | "zsh" | "toml" | "ini" | "conf" | "yaml" | "yml" => {
            s(&["#"], NO_BLOCKS, dq_sq, &[])
        }
        "json" => s(&[], NO_BLOCKS, dq, &[]),
        "css" => s(&[], C_BLOCK, dq_sq_bt, &[]),
        "html" | "xml" | "svg" | "md" => s(&[], HTML_BLOCK, dq, &[]),
        "lua" => s(&["--"], LUA_BLOCK, dq_sq, &[]),
        // `.gitignore`/`.dockerignore`/etc: shell-glob lines, `#` comments only.
        "gitignore" => s(&["#"], NO_BLOCKS, &[], &[]),
        // `.env`: `#` comments, quoted values.
        "env" => s(&["#"], NO_BLOCKS, dq_sq, &[]),
        // Makefiles: `#` comments colour is the whole value here, quoting isn't
        // C-like, so no string delimiters and no keyword table to invent.
        "make" => s(&["#"], NO_BLOCKS, &[], &[]),
        "dockerfile" => Syntax {
            line_comments: &["#"],
            blocks: NO_BLOCKS,
            strings: dq_sq,
            keywords: DOCKER_KEYWORDS,
            keywords_ci: true,
        },
        _ => return None,
    })
}

/// Is `key` a text/code type we can preview as highlighted text?
pub fn is_text_ext(key: &str) -> bool {
    syntax_for(key).is_some()
        || matches!(
            key,
            "txt" | "log" | "csv" | "text" | "lock" | "cfg" | "properties"
        )
}

/// Highlight `text` into one token row per line. Conservative: colours strings,
/// comments, multi-line spans (block comments, docstrings, template literals,
/// with their state carried across lines), numbers, and keywords; everything
/// else is `Plain`. Returns exactly one row per `str::lines()` line, so callers
/// can index rows by line number. Uncapped: bound the work by capping the
/// *input* at the call site (previews read a head; the text viewer caps
/// bytes/lines), which keeps this core correct for multi-line spans rather than
/// truncating them mid-state.
pub fn highlight(text: &str, syntax: Syntax) -> Vec<Vec<Token>> {
    let mut out = Vec::new();
    let mut in_block: Option<usize> = None; // index into syntax.blocks of the open span
    for line in text.lines() {
        out.push(highlight_line(line, syntax, &mut in_block));
    }
    out
}

fn highlight_line(line: &str, syn: Syntax, in_block: &mut Option<usize>) -> Vec<Token> {
    let chars: Vec<char> = line.chars().collect();
    let mut tokens: Vec<Token> = Vec::new();
    let mut i = 0;

    // Push text with a kind, merging into the previous token when the kind matches.
    let push = |tokens: &mut Vec<Token>, text: String, kind: TokenKind| {
        if text.is_empty() {
            return;
        }
        match tokens.last_mut() {
            Some(last) if last.kind == kind => last.text.push_str(&text),
            _ => tokens.push(Token { text, kind }),
        }
    };

    while i < chars.len() {
        // Inside an already-open span: consume until ITS closing delimiter.
        if let Some(bi) = *in_block {
            let block = syn.blocks[bi];
            if let Some(end) = find_at(&chars, i, block.close) {
                let upto = end + block.close.chars().count();
                push(&mut tokens, chars[i..upto].iter().collect(), block.kind);
                i = upto;
                *in_block = None;
                continue;
            }
            push(&mut tokens, chars[i..].iter().collect(), block.kind);
            break;
        }

        let c = chars[i];

        // Span open, tried in table order. This is what makes a triple quote win
        // over a lone quote: the check runs before the single-char string branch.
        if let Some((bi, block)) = syn
            .blocks
            .iter()
            .enumerate()
            .find(|(_, b)| starts_with_at(&chars, i, b.open))
        {
            // Start searching for the close AFTER the opener so a delimiter whose
            // open and close are identical (triple quotes, a backtick) never
            // matches itself as its own closer.
            if let Some(end) = find_at(&chars, i + block.open.chars().count(), block.close) {
                let upto = end + block.close.chars().count();
                push(&mut tokens, chars[i..upto].iter().collect(), block.kind);
                i = upto;
                continue;
            }
            push(&mut tokens, chars[i..].iter().collect(), block.kind);
            *in_block = Some(bi);
            break;
        }

        // Line comment, rest of line.
        if syn
            .line_comments
            .iter()
            .any(|lc| starts_with_at(&chars, i, lc))
        {
            push(&mut tokens, chars[i..].iter().collect(), TokenKind::Comment);
            break;
        }

        // String literal.
        if syn.strings.contains(&c) {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let j = j.min(chars.len());
            push(&mut tokens, chars[i..j].iter().collect(), TokenKind::Str);
            i = j;
            continue;
        }

        // Number.
        if c.is_ascii_digit() {
            let mut j = i;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '.') {
                j += 1;
            }
            push(&mut tokens, chars[i..j].iter().collect(), TokenKind::Number);
            i = j;
            continue;
        }

        // Identifier / keyword.
        if c.is_alphabetic() || c == '_' {
            let mut j = i;
            while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let is_keyword = if syn.keywords_ci {
                syn.keywords.iter().any(|k| k.eq_ignore_ascii_case(&word))
            } else {
                syn.keywords.contains(&word.as_str())
            };
            let kind = if is_keyword {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            };
            push(&mut tokens, word, kind);
            i = j;
            continue;
        }

        // Anything else: plain single char.
        push(&mut tokens, c.to_string(), TokenKind::Plain);
        i += 1;
    }
    tokens
}

/// Does the sub-slice starting at `i` begin with `pat`?
fn starts_with_at(chars: &[char], i: usize, pat: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    i + p.len() <= chars.len() && chars[i..i + p.len()] == p[..]
}

/// Index of the first occurrence of `pat` in `chars` at or after `from`.
fn find_at(chars: &[char], from: usize, pat: &str) -> Option<usize> {
    let p: Vec<char> = pat.chars().collect();
    if p.is_empty() || from > chars.len() {
        return None;
    }
    (from..=chars.len().saturating_sub(p.len())).find(|&k| chars[k..k + p.len()] == p[..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(tokens: &[Token]) -> Vec<(TokenKind, &str)> {
        tokens.iter().map(|t| (t.kind, t.text.as_str())).collect()
    }

    #[test]
    fn highlights_keyword_string_number_comment() {
        let syn = syntax_for("rs").unwrap();
        let mut blk = None;
        let line = highlight_line(r#"let x = 42; // note "s""#, syn, &mut blk);
        let k = kinds(&line);
        assert!(k.contains(&(TokenKind::Keyword, "let")));
        assert!(k.contains(&(TokenKind::Number, "42")));
        assert!(k.iter().any(|(kind, _)| *kind == TokenKind::Comment));
    }

    #[test]
    fn block_comment_carries_across_lines() {
        let syn = syntax_for("rs").unwrap();
        let lines = highlight("/* start\nstill comment */ let y", syn);
        assert_eq!(lines[0][0].kind, TokenKind::Comment);
        // After the close on line 2, `let` is a keyword again.
        let l2 = &lines[1];
        assert!(l2
            .iter()
            .any(|t| t.kind == TokenKind::Keyword && t.text == "let"));
    }

    #[test]
    fn plain_text_has_no_keywords() {
        let lines = highlight("just some words 12", PLAIN);
        assert!(lines[0].iter().all(|t| t.kind != TokenKind::Keyword));
        assert!(lines[0].iter().any(|t| t.kind == TokenKind::Number));
    }

    #[test]
    fn python_docstring_is_entirely_str_not_leaking_keywords() {
        // The exact reported bug: `is` inside a triple-quoted docstring must not
        // be coloured as the Python keyword `is`.
        let syn = syntax_for("py").unwrap();
        let src = "def f():\n    \"\"\" nothing here is deployed \"\"\"\n    return 1";
        let lines = highlight(src, syn);
        assert!(
            lines[1].iter().all(|t| t.kind != TokenKind::Keyword),
            "docstring line leaked a keyword: {:?}",
            kinds(&lines[1])
        );
        // Everything but the leading indentation (Plain, before the `"""` opens)
        // is part of the string.
        assert!(lines[1]
            .iter()
            .filter(|t| !t.text.trim().is_empty())
            .all(|t| t.kind == TokenKind::Str));
    }

    #[test]
    fn triple_quoted_string_spans_multiple_lines_then_code_resumes() {
        let syn = syntax_for("py").unwrap();
        let src = "x = \"\"\"\nstill in the string, if is not code\nclosed\"\"\" \nreturn 1";
        let lines = highlight(src, syn);
        // Line 0: everything after the opening triple quote is part of the string.
        assert!(lines[0].iter().any(|t| t.kind == TokenKind::Str));
        // Line 1 is entirely inside the string.
        assert!(lines[1].iter().all(|t| t.kind == TokenKind::Str));
        // Line 2 closes the string; line 3 is ordinary code again.
        assert!(lines[3]
            .iter()
            .any(|t| t.kind == TokenKind::Keyword && t.text == "return"));
    }

    #[test]
    fn triple_single_quote_behaves_like_triple_double_quote() {
        let syn = syntax_for("py").unwrap();
        let src = "def f():\n    ''' nothing here is deployed '''\n    return 1";
        let lines = highlight(src, syn);
        assert!(lines[1].iter().all(|t| t.kind != TokenKind::Keyword));
        assert!(lines[1]
            .iter()
            .filter(|t| !t.text.trim().is_empty())
            .all(|t| t.kind == TokenKind::Str));
    }

    #[test]
    fn js_template_literal_spans_lines() {
        let syn = syntax_for("js").unwrap();
        let src = "const x = `hello\nstill a template, const is not a keyword here\nend`;";
        let lines = highlight(src, syn);
        assert!(lines[1].iter().all(|t| t.kind == TokenKind::Str));
        // After the closing backtick on line 2, ordinary tokenising resumes.
        assert!(lines[2].iter().any(|t| t.kind == TokenKind::Str));
    }

    #[test]
    fn lang_key_table() {
        assert_eq!(lang_key("Dockerfile"), "dockerfile");
        assert_eq!(lang_key("dockerfile"), "dockerfile");
        assert_eq!(lang_key("Dockerfile.dev"), "dockerfile");
        assert_eq!(lang_key("web.dockerfile"), "dockerfile");
        assert_eq!(lang_key("Makefile"), "make");
        assert_eq!(lang_key(".gitignore"), "gitignore");
        assert_eq!(lang_key(".env"), "env");
        assert_eq!(lang_key(".env.local"), "env");
        assert_eq!(lang_key("main.rs"), "rs");
        assert_eq!(lang_key("README.md"), "md");
        assert_eq!(lang_key("README"), "");
    }

    #[test]
    fn dockerfile_keywords_are_case_insensitive() {
        let syn = syntax_for("dockerfile").unwrap();
        let mut blk = None;
        let line = highlight_line("FROM alpine:3.20 AS build # comment", syn, &mut blk);
        let k = kinds(&line);
        assert!(k.contains(&(TokenKind::Keyword, "FROM")));
        assert!(k.contains(&(TokenKind::Keyword, "AS")));
        assert!(k
            .iter()
            .any(|(kind, text)| *kind == TokenKind::Comment && text.starts_with('#')));

        let mut blk2 = None;
        let lower = highlight_line("from alpine", syn, &mut blk2);
        assert!(kinds(&lower).contains(&(TokenKind::Keyword, "from")));
    }
}
