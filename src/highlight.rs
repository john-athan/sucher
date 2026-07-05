// Lightweight, dependency-free syntax highlighting.
//
// A pure classifier: given a file extension it picks a [`Syntax`], and given
// text it splits each line into [`Token`]s tagged with a [`TokenKind`]. It has
// no colours, no IO, and no dependencies — the UI layer maps [`TokenKind`] to
// colours (see `crate::theme::token_color`). Dependency flows theme -> highlight,
// never the reverse.

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

/// A minimal syntax description: enough to colour strings, comments, numbers, keywords.
#[derive(Clone, Copy)]
pub struct Syntax {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    strings: &'static [char],
    keywords: &'static [&'static str],
}

/// Plain text: basic string detection only, no keywords/comments.
pub const PLAIN: Syntax = Syntax {
    line_comments: &[],
    block_comment: None,
    strings: &[],
    keywords: &[],
};

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

/// Pick a [`Syntax`] for a file extension (the extension point for code types).
pub fn syntax_for(ext: &str) -> Option<Syntax> {
    let s = |line_comments, block_comment, strings, keywords| Syntax {
        line_comments,
        block_comment,
        strings,
        keywords,
    };
    let dq: &[char] = &['"'];
    let dq_sq_bt: &[char] = &['"', '\'', '`'];
    Some(match ext {
        "rs" => s(&["//"], Some(("/*", "*/")), &['"'], RUST_KEYWORDS),
        "c" | "h" | "cpp" | "hpp" | "cc" | "java" | "cs" => {
            s(&["//"], Some(("/*", "*/")), &['"', '\''], C_KEYWORDS)
        }
        "js" | "ts" | "tsx" | "jsx" => s(&["//"], Some(("/*", "*/")), dq_sq_bt, JS_KEYWORDS),
        "go" => s(&["//"], Some(("/*", "*/")), dq_sq_bt, GO_KEYWORDS),
        "py" => s(&["#"], None, &['"', '\''], PY_KEYWORDS),
        "rb" => s(&["#"], Some(("=begin", "=end")), &['"', '\''], PY_KEYWORDS),
        "sh" | "bash" | "zsh" | "toml" | "ini" | "conf" | "yaml" | "yml" => {
            s(&["#"], None, &['"', '\''], &[])
        }
        "json" => s(&[], None, dq, &[]),
        "css" => s(&[], Some(("/*", "*/")), dq_sq_bt, &[]),
        "html" | "xml" | "svg" | "md" => s(&[], Some(("<!--", "-->")), dq, &[]),
        "lua" => s(&["--"], Some(("--[[", "]]")), &['"', '\''], &[]),
        _ => return None,
    })
}

/// Is `ext` a text/code type we can preview as highlighted text?
pub fn is_text_ext(ext: &str) -> bool {
    syntax_for(ext).is_some()
        || matches!(
            ext,
            "txt" | "log" | "csv" | "text" | "env" | "gitignore" | "lock" | "cfg" | "properties"
        )
}

/// Highlight `text` into one token row per line. Conservative: colours strings,
/// comments (line + block, with block-comment state carried across lines),
/// numbers, and keywords; everything else is `Plain`. Returns exactly one row per
/// `str::lines()` line, so callers can index rows by line number. Uncapped — bound
/// the work by capping the *input* at the call site (previews read a head; the text
/// viewer caps bytes/lines), which keeps this core correct for multi-line block
/// comments rather than truncating them mid-state.
pub fn highlight(text: &str, syntax: Syntax) -> Vec<Vec<Token>> {
    let mut out = Vec::new();
    let mut in_block = false; // block-comment state carried across lines
    for line in text.lines() {
        out.push(highlight_line(line, syntax, &mut in_block));
    }
    out
}

fn highlight_line(line: &str, syn: Syntax, in_block: &mut bool) -> Vec<Token> {
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
        // Inside a block comment: consume until the closing delimiter.
        if *in_block {
            if let Some((_, close)) = syn.block_comment {
                if let Some(end) = find_at(&chars, i, close) {
                    let upto = end + close.chars().count();
                    push(
                        &mut tokens,
                        chars[i..upto].iter().collect(),
                        TokenKind::Comment,
                    );
                    i = upto;
                    *in_block = false;
                    continue;
                }
            }
            push(&mut tokens, chars[i..].iter().collect(), TokenKind::Comment);
            break;
        }

        let c = chars[i];

        // Block-comment open.
        if let Some((open, close)) = syn.block_comment {
            if starts_with_at(&chars, i, open) {
                if let Some(end) = find_at(&chars, i + open.chars().count(), close) {
                    let upto = end + close.chars().count();
                    push(
                        &mut tokens,
                        chars[i..upto].iter().collect(),
                        TokenKind::Comment,
                    );
                    i = upto;
                    continue;
                }
                push(&mut tokens, chars[i..].iter().collect(), TokenKind::Comment);
                *in_block = true;
                break;
            }
        }

        // Line comment → rest of line.
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
            let kind = if syn.keywords.contains(&word.as_str()) {
                TokenKind::Keyword
            } else {
                TokenKind::Plain
            };
            push(&mut tokens, word, kind);
            i = j;
            continue;
        }

        // Anything else → plain single char.
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
        let mut blk = false;
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
}
