//! Which SQL a migration tree actually applies, and in what order.
//!
//! A migration directory is not a set of independent files — it is a
//! *sequence*. `0001_create_users.sql` adds a column that `0014_trim.sql`
//! removes, and a parser that unions every file reports a schema that never
//! existed. So the order has to be right, and the half that runs backwards has
//! to be left out.

/// Whether this file's statements run forwards.
///
/// Four conventions put the reverse migration in a file of its own, and a
/// parser that reads them undoes the tree it just read:
/// `*.down.sql` (golang-migrate, dbmate), `down.sql` (Diesel), and Flyway's
/// `U1__*.sql` undo scripts.
pub fn is_up(path: &str) -> bool {
    let base = basename(path).to_ascii_lowercase();
    if base == "down.sql" || base.ends_with(".down.sql") {
        return false;
    }
    // Flyway names an undo script `U<version>__<description>.sql`; `V` and `R`
    // run forwards. The version and the `__` are both required, so a file
    // named `users.sql` is not mistaken for one.
    let Some(rest) = base.strip_prefix('u') else {
        return true;
    };
    let version: String = rest
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    !(version.starts_with(|c: char| c.is_ascii_digit()) && rest[version.len()..].starts_with("__"))
}

/// The part of a file that runs forwards.
///
/// goose and sql-migrate keep both directions in one file, split by a comment
/// directive. Everything from the `Up` marker to the `Down` marker is the
/// forward migration; a file with neither is forward in its entirety.
pub fn up_section(text: &str) -> &str {
    let up = marker(text, "up");
    let body = match up {
        Some(at) => &text[at..],
        None => text,
    };
    match marker(body, "down") {
        Some(at) => &body[..at],
        None => body,
    }
}

/// Byte offset just past a `-- +goose Up` / `-- +migrate Down` directive.
///
/// Matched on the line rather than by a regex so a `-- +goose StatementBegin`
/// in between is simply not a match, and so the directive is recognised
/// whatever spacing it was written with.
fn marker(text: &str, want: &str) -> Option<usize> {
    let mut at = 0usize;
    for line in text.lines() {
        let end = at + line.len();
        at = end + 1;
        let Some(comment) = line.trim_start().strip_prefix("--") else {
            continue;
        };
        let mut words = comment.split_whitespace();
        if let Some(tool) = words.next()
            && (tool == "+goose" || tool == "+migrate")
            && words.next().is_some_and(|d| d.eq_ignore_ascii_case(want))
        {
            return Some(end);
        }
    }
    None
}

/// `a/b/c.sql` → `c.sql`.
pub fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Order two migration paths the way the tool that wrote them would run them.
///
/// Digit runs compare as numbers, so `9_x.sql` precedes `10_x.sql` — plain
/// lexicographic order reverses those two, and a tree numbered without
/// zero-padding would replay its migrations in the wrong order while looking
/// perfectly sorted.
pub fn migration_order(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        match (x.first(), y.first()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) => {
                if p.is_ascii_digit() && q.is_ascii_digit() {
                    let (nx, rx) = number(x);
                    let (ny, ry) = number(y);
                    match nx.cmp(&ny) {
                        Ordering::Equal => {
                            x = rx;
                            y = ry;
                        }
                        other => return other,
                    }
                } else {
                    match p.cmp(q) {
                        Ordering::Equal => {
                            x = &x[1..];
                            y = &y[1..];
                        }
                        other => return other,
                    }
                }
            }
        }
    }
}

/// The leading digit run as a number, and what follows it.
///
/// Saturating, because a timestamp-named migration can be longer than `u128`
/// and two that differ only past that digit are in the same second anyway.
fn number(s: &[u8]) -> (u128, &[u8]) {
    let end = s
        .iter()
        .position(|c| !c.is_ascii_digit())
        .unwrap_or(s.len());
    let mut n = 0u128;
    for c in &s[..end] {
        n = n.saturating_mul(10).saturating_add(u128::from(*c - b'0'));
    }
    (n, &s[end..])
}
