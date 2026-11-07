//! String literals in source files — where a query written in code lives.
//!
//! Not a parser for any of these languages. The only question here is which
//! byte ranges are string literals, and that is a lexical one. Comments are
//! skipped rather than merely ignored: a `//` holding an apostrophe would
//! otherwise open a literal that runs to the next quote and swallows the rest
//! of the file. Everything past that — which call the literal is an argument
//! to, which function encloses it — is the language plugin's to know, and
//! this one does not ask.

/// One string literal, and the lines it spans.
#[derive(Debug, PartialEq)]
pub struct Literal {
    pub line: u32,
    pub end_line: u32,
    pub text: String,
}

/// How one language writes a string literal.
#[derive(Clone, Copy)]
struct Form {
    open: &'static str,
    close: &'static str,
    /// A `\` starts an escape inside, so `\"` does not end the literal.
    escape: bool,
    /// A doubled closer stands for one of itself — C#'s `@"say ""hi"""`.
    double_close: bool,
}

/// A plain double-quoted string with backslash escapes: every language here
/// has one.
const DQ: Form = Form {
    open: "\"",
    close: "\"",
    escape: true,
    double_close: false,
};

/// A single-quoted string. Only Python and JavaScript have one — elsewhere
/// `'` is a character literal or, in Rust, a lifetime.
const SQ: Form = Form {
    open: "'",
    close: "'",
    escape: true,
    double_close: false,
};

/// A triple-quoted block: Python's, Java's text block, Kotlin's raw string,
/// C#'s raw literal. None of them escape, and all of them span lines.
const TRIPLE: Form = Form {
    open: "\"\"\"",
    close: "\"\"\"",
    escape: false,
    double_close: false,
};

/// A backquoted string: Go's raw literal and JavaScript's template.
const BACKTICK: Form = Form {
    open: "`",
    close: "`",
    escape: false,
    double_close: false,
};

/// How one language writes strings and comments.
pub struct Syntax {
    /// Longest opener first: `"""` has to be tried before `"`.
    forms: &'static [Form],
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    /// Rust's block comments nest; C's and Java's do not.
    nested_block: bool,
    /// `r"…"` and `r#"…"#`, whose closer the opener decides.
    rust_raw: bool,
    /// Two literals with only whitespace between them are one string — C's
    /// rule, and Python's. In Go, Java and C# that is a syntax error.
    glue_adjacent: bool,
    /// A `+` between two literals joins them: how Java, C# and Go write one
    /// statement over several lines.
    glue_plus: bool,
}

/// The C family's comments, which every language here but Python shares.
const SLASH: &[&str] = &["//"];
const STARS: Option<(&str, &str)> = Some(("/*", "*/"));

impl Syntax {
    /// C-family comments, no raw strings, no adjacency.
    const fn c_like(forms: &'static [Form], glue_plus: bool) -> Self {
        Self {
            forms,
            line_comments: SLASH,
            block_comment: STARS,
            nested_block: false,
            rust_raw: false,
            glue_adjacent: false,
            glue_plus,
        }
    }
}

/// The syntax for a file's extension, or `None` for one this plugin does not
/// read. Deliberately the extensions the official language plugins claim: a
/// file no parser here knows holds no function for a query to belong to.
pub fn syntax_of(path: &str) -> Option<Syntax> {
    let ext = path.rsplit_once('.').map(|(_, e)| e)?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "rs" => Syntax {
            forms: &[DQ],
            line_comments: SLASH,
            block_comment: STARS,
            nested_block: true,
            rust_raw: true,
            glue_adjacent: false,
            glue_plus: false,
        },
        "go" => Syntax::c_like(&[BACKTICK, DQ], true),
        "py" | "pyi" | "pyw" => Syntax {
            forms: &[
                TRIPLE,
                Form {
                    open: "'''",
                    close: "'''",
                    escape: false,
                    double_close: false,
                },
                DQ,
                SQ,
            ],
            line_comments: &["#"],
            block_comment: None,
            nested_block: false,
            rust_raw: false,
            glue_adjacent: true,
            glue_plus: true,
        },
        "ts" | "tsx" | "mts" | "cts" | "js" | "jsx" | "mjs" | "cjs" => {
            Syntax::c_like(&[BACKTICK, DQ, SQ], true)
        }
        "java" | "kt" | "kts" => Syntax::c_like(&[TRIPLE, DQ], true),
        "cs" => Syntax::c_like(
            &[
                TRIPLE,
                Form {
                    open: "@\"",
                    close: "\"",
                    escape: false,
                    double_close: true,
                },
                DQ,
            ],
            true,
        ),
        "c" | "h" | "cc" | "cpp" | "hpp" | "cxx" | "hh" => Syntax {
            glue_adjacent: true,
            ..Syntax::c_like(&[DQ], false)
        },
        _ => return None,
    })
}

/// Every string literal in `text`, in the order written, adjacent ones joined
/// the way the language joins them.
pub fn literals(text: &str, syn: &Syntax) -> Vec<Literal> {
    let mut out = Vec::new();
    let mut at = 0usize;
    let mut line = 1u32;
    while at < text.len() {
        if text.as_bytes()[at] == b'\n' {
            line += 1;
            at += 1;
            continue;
        }
        if let Some(end) = comment_end(text, at, syn) {
            line += newlines(&text[at..end]);
            at = end;
            continue;
        }
        let Some((end, mut body)) = string_at(text, at, syn) else {
            at += step(text, at);
            continue;
        };
        let started = line;
        line += newlines(&text[at..end]);
        at = end;
        // Whatever the language glues on, as long as it keeps gluing.
        while let Some((end, more)) = glued(text, at, syn) {
            line += newlines(&text[at..end]);
            at = end;
            body.push_str(&more);
        }
        out.push(Literal {
            line: started,
            end_line: line,
            text: body,
        });
    }
    out
}

/// Where the comment starting at `at` ends, if one does.
fn comment_end(text: &str, at: usize, syn: &Syntax) -> Option<usize> {
    let rest = &text[at..];
    if syn.line_comments.iter().any(|c| rest.starts_with(c)) {
        return Some(rest.find('\n').map_or(text.len(), |n| at + n));
    }
    let (open, close) = syn.block_comment?;
    if !rest.starts_with(open) {
        return None;
    }
    let mut depth = 1usize;
    let mut cur = at + open.len();
    while cur < text.len() {
        if text[cur..].starts_with(close) {
            depth -= 1;
            cur += close.len();
            if depth == 0 {
                return Some(cur);
            }
            continue;
        }
        if syn.nested_block && text[cur..].starts_with(open) {
            depth += 1;
            cur += open.len();
            continue;
        }
        cur += step(text, cur);
    }
    // Unterminated: the rest of the file is comment, which is what the
    // compiler would say about it too.
    Some(text.len())
}

/// The literal starting at `at`, as `(end, body)`.
fn string_at(text: &str, at: usize, syn: &Syntax) -> Option<(usize, String)> {
    if syn.rust_raw
        && let Some(found) = rust_raw(text, at)
    {
        return Some(found);
    }
    let rest = &text[at..];
    let form = syn.forms.iter().find(|f| rest.starts_with(f.open))?;
    let mut cur = at + form.open.len();
    let mut body = String::new();
    while cur < text.len() {
        if form.escape && text.as_bytes()[cur] == b'\\' {
            cur = escape(text, cur, &mut body);
            continue;
        }
        if text[cur..].starts_with(form.close) {
            let after = cur + form.close.len();
            if form.double_close && text[after..].starts_with(form.close) {
                body.push_str(form.close);
                cur = after + form.close.len();
                continue;
            }
            return Some((after, body));
        }
        let next = cur + step(text, cur);
        body.push_str(&text[cur..next]);
        cur = next;
    }
    None
}

/// One escape sequence onto `body`, returning where it ended.
///
/// A quote must come through as a quote — `"SELECT \"id\" FROM t"` names a
/// column, and turning its quotes into anything else loses it. A `\n` must
/// come through as a newline rather than a space: a statement written
/// `"-- the hot path\nSELECT …"` is a comment and then a query, and flattened
/// onto one line it is a comment and nothing else. Anything a lexer cannot
/// name keeps the character and drops the backslash, which is as close as it
/// can get without the language's whole table.
fn escape(text: &str, at: usize, body: &mut String) -> usize {
    let Some(ch) = text[at + 1..].chars().next() else {
        return text.len();
    };
    match ch {
        'n' => body.push('\n'),
        'r' => body.push('\r'),
        't' => body.push('\t'),
        // A backslash before the end of the line continues the literal and
        // stands for nothing.
        '\n' => {}
        other => body.push(other),
    }
    at + 1 + ch.len_utf8()
}

/// `r"…"`, `r#"…"#`, `r##"…"##`: the hash count in the opener names the
/// closer, so a raw string can hold any quote at all.
fn rust_raw(text: &str, at: usize) -> Option<(usize, String)> {
    let rest = text[at..].strip_prefix('r')?;
    let hashes = rest.len() - rest.trim_start_matches('#').len();
    let rest = rest[hashes..].strip_prefix('"')?;
    let close = format!("\"{}", "#".repeat(hashes));
    let body = rest.find(&close)?;
    // `r`, the hashes, and the opening quote.
    let start = at + hashes + 2;
    Some((start + body + close.len(), rest[..body].to_string()))
}

/// The next literal, when only glue separates it from the last one.
fn glued(text: &str, at: usize, syn: &Syntax) -> Option<(usize, String)> {
    let mut cur = at;
    let mut plus = false;
    while cur < text.len() {
        let b = text.as_bytes()[cur];
        if b.is_ascii_whitespace() || (syn.glue_plus && b == b'+' && !plus) {
            plus |= b == b'+';
            cur += 1;
            continue;
        }
        break;
    }
    if !plus && !syn.glue_adjacent {
        return None;
    }
    string_at(text, cur, syn)
}

/// Bytes of the character at `at` — never zero, so no caller can spin.
fn step(text: &str, at: usize) -> usize {
    text[at..].chars().next().map_or(1, char::len_utf8)
}

fn newlines(s: &str) -> u32 {
    s.bytes().filter(|b| *b == b'\n').count() as u32
}
