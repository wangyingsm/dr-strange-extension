//! Commit and tag objects, as text.
//!
//! Both are headers, a blank line, and a message. The headers a repository can
//! carry are open-ended — `gpgsig` wraps over continuation lines, `mergetag`
//! embeds a whole other object — so parsing reads the ones it knows and steps
//! over the rest rather than refusing a commit for carrying something new.

/// Who did something, and when.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sig {
    pub name: String,
    pub email: String,
    /// Unix seconds.
    pub ts: i64,
    /// Offset from UTC in minutes, as the author's own clock had it. Kept
    /// because it is the only trace of where a commit was made, and because
    /// dropping it would make the ISO rendering a lie.
    pub tz: i32,
}

impl Sig {
    /// `Ada Lovelace <ada@example.com> 1787728626 +0800`
    pub fn parse(line: &str) -> Sig {
        let mut sig = Sig::default();
        let (who, when) = match (line.find('<'), line.find('>')) {
            (Some(open), Some(close)) if open < close => {
                sig.name = line[..open].trim().to_string();
                sig.email = line[open + 1..close].to_string();
                (&line[..0], line[close + 1..].trim())
            }
            // No angle brackets is malformed, but the timestamp may still be
            // there; keeping what can be read beats discarding the commit.
            _ => (line, line),
        };
        let _ = who;
        let mut fields = when.split_whitespace();
        sig.ts = fields
            .next()
            .and_then(|t| t.parse().ok())
            .unwrap_or_default();
        sig.tz = fields.next().map(parse_tz).unwrap_or_default();
        sig
    }

    /// The signature's own moment, in its own zone.
    pub fn iso8601(&self) -> String {
        iso8601(self.ts, self.tz)
    }
}

/// `+0800` → 480 minutes.
fn parse_tz(tz: &str) -> i32 {
    let sign = if tz.starts_with('-') { -1 } else { 1 };
    let digits: String = tz.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() < 4 {
        return 0;
    }
    let hours: i32 = digits[..2].parse().unwrap_or(0);
    let minutes: i32 = digits[2..4].parse().unwrap_or(0);
    sign * (hours * 60 + minutes)
}

/// Unix seconds and a zone offset, as `2026-08-26T15:17:06+08:00`.
///
/// Written out rather than pulled from a date crate: the plugin runs in a
/// sandbox with a frozen clock, so the only date arithmetic it ever needs is
/// this one conversion, and a dependency for it would be larger than it.
pub fn iso8601(ts: i64, tz_minutes: i32) -> String {
    let local = ts + tz_minutes as i64 * 60;
    let days = local.div_euclid(86_400);
    let secs = local.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let (sign, off) = if tz_minutes < 0 {
        ('-', -tz_minutes)
    } else {
        ('+', tz_minutes)
    };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}{sign}{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60,
        off / 60,
        off % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the epoch → a calendar date,
/// with no table and no leap-year special cases.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Commit {
    pub oid: String,
    /// The tree this commit records — the root of what the files looked like.
    pub tree: String,
    pub parents: Vec<String>,
    pub author: Sig,
    pub committer: Sig,
    /// The message's first line.
    pub summary: String,
    /// Everything below it, trimmed.
    pub body: String,
}

/// Split an object into its headers and its message.
///
/// A header may continue onto following lines, each starting with a space —
/// which is how a PGP signature fits into a line-oriented format. Continuations
/// are stepped over: nothing here reads a signature, and treating one's inner
/// lines as headers would invent `-----BEGIN` as a field name.
fn split_headers(text: &str) -> (Vec<(&str, &str)>, &str) {
    let mut headers = Vec::new();
    let mut rest = text;
    loop {
        let (line, tail) = match rest.split_once('\n') {
            Some(split) => split,
            None => (rest, ""),
        };
        if line.is_empty() {
            return (headers, tail);
        }
        if !line.starts_with(' ')
            && let Some((key, value)) = line.split_once(' ')
        {
            headers.push((key, value));
        }
        if tail.is_empty() {
            return (headers, "");
        }
        rest = tail;
    }
}

pub fn parse_commit(oid: &str, data: &[u8]) -> Commit {
    let text = String::from_utf8_lossy(data);
    let (headers, message) = split_headers(&text);
    let mut commit = Commit {
        oid: oid.to_string(),
        ..Default::default()
    };
    for (key, value) in headers {
        match key {
            "tree" => commit.tree = value.trim().to_string(),
            "parent" => commit.parents.push(value.trim().to_string()),
            "author" => commit.author = Sig::parse(value),
            "committer" => commit.committer = Sig::parse(value),
            _ => {}
        }
    }
    let message = message.trim_end();
    let (summary, body) = match message.split_once('\n') {
        Some((first, rest)) => (first, rest.trim()),
        None => (message, ""),
    };
    commit.summary = summary.trim().to_string();
    commit.body = body.to_string();
    commit
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tag {
    /// What the tag points at.
    pub object: String,
    /// The kind of thing that is — `commit` for an ordinary release tag.
    pub kind: String,
    pub name: String,
    pub tagger: Sig,
    pub message: String,
}

pub fn parse_tag(data: &[u8]) -> Tag {
    let text = String::from_utf8_lossy(data);
    let (headers, message) = split_headers(&text);
    let mut tag = Tag::default();
    for (key, value) in headers {
        match key {
            "object" => tag.object = value.trim().to_string(),
            "type" => tag.kind = value.trim().to_string(),
            "tag" => tag.name = value.trim().to_string(),
            "tagger" => tag.tagger = Sig::parse(value),
            _ => {}
        }
    }
    tag.message = message.trim().to_string();
    tag
}
