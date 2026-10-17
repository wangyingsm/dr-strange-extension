//! The container-image convention: the one thread from something running
//! back to the commit that built it.
//!
//! An ops plugin reads a declaration — a Compose service, a Kubernetes pod
//! template — and finds an image reference in it. That string is the only
//! thing both a deployment manifest and a build pipeline name, so it is where
//! the ops graph joins the history graph (`arch/10-ops.md` §5 in the main
//! repository).
//!
//! This lives in the SDK rather than in a plugin because **two plugins must
//! agree exactly**. The same image parsed by the Compose reader and the
//! Kubernetes reader is one node under one key; if they disagreed about what
//! counts as a revision, that node would carry whichever property was written
//! last. Nothing here mints a node: the join is a property and a query, never
//! an edge, because a cross-plane edge is rejected by the storage layer.

/// What a reference says about itself, with nothing inferred beyond it.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ImageRef<'a> {
    /// The reference exactly as written — the node's key.
    pub reference: &'a str,
    /// The registry host, when one is written. `docker.io` is *not* filled in
    /// for a bare name: what the author wrote is the fact.
    pub registry: Option<&'a str>,
    /// Everything between the registry and the tag or digest.
    pub repository: &'a str,
    /// The tag, when the reference carries one.
    pub tag: Option<&'a str>,
    /// The digest, which pins the bytes and says nothing about provenance.
    pub digest: Option<&'a str>,
    /// A git revision the tag appears to name. The join carrier.
    pub revision: Option<String>,
}

/// Split a reference into its parts.
///
/// The one ambiguity is the colon: in `registry:5000/app` it is a port, in
/// `app:v1` it is a tag. It is a tag only in the final path segment, which is
/// how every registry client reads it.
pub fn parse_image(reference: &str) -> ImageRef<'_> {
    let (before_digest, digest) = match reference.split_once('@') {
        Some((head, d)) => (head, Some(d)),
        None => (reference, None),
    };

    let (head, last) = match before_digest.rsplit_once('/') {
        Some((head, last)) => (Some(head), last),
        None => (None, before_digest),
    };
    let (name_tail, tag) = match last.split_once(':') {
        Some((name, t)) => (name, Some(t)),
        None => (last, None),
    };

    // A first segment is a registry when it looks like a host: it carries a
    // dot or a port, or it is exactly `localhost`. Otherwise it is part of
    // the repository path, as in `library/postgres`.
    let registry = head.and_then(|h| {
        let first = h.split('/').next().unwrap_or(h);
        let host_like = first.contains('.') || first.contains(':') || first == "localhost";
        host_like.then_some(first)
    });
    let repository = match (head, registry) {
        (Some(h), Some(r)) => {
            let rest = h.strip_prefix(r).unwrap_or(h).trim_start_matches('/');
            if rest.is_empty() {
                name_tail
            } else {
                // The repository spans the path; the caller wants the whole
                // of it, so point at the original slice.
                let start = reference.find(rest).unwrap_or(0);
                let end = start + rest.len() + 1 + name_tail.len();
                reference.get(start..end).unwrap_or(name_tail)
            }
        }
        (Some(h), None) => {
            let start = reference.find(h).unwrap_or(0);
            let end = start + h.len() + 1 + name_tail.len();
            reference.get(start..end).unwrap_or(name_tail)
        }
        (None, _) => name_tail,
    };

    ImageRef {
        reference,
        registry,
        repository,
        tag,
        digest,
        revision: tag.and_then(revision_in),
    }
}

/// The git revision a tag names, when it names one.
///
/// A pipeline writes the revision into the tag — `sha-9f2c1b7`, `git-abc1234`,
/// or the bare hex — and that is the whole of the join. Seven characters is
/// the shortest abbreviation git itself prints; below that a hex match would
/// be coincidence.
///
/// An all-digit tag is rejected: `17`, `2024` and `8080` are versions and
/// years far more often than revisions, and a wrong join is worse than none.
/// A tag that is exactly a known channel — `latest`, `stable`, `edge`,
/// `main`, `master` — names no revision either, however hex-like.
pub fn revision_in(tag: &str) -> Option<String> {
    const CHANNELS: &[&str] = &["latest", "stable", "edge", "main", "master", "dev", "test"];
    let lower = tag.to_ascii_lowercase();
    if CHANNELS.contains(&lower.as_str()) {
        return None;
    }
    let candidate = ["sha-", "sha256-", "git-", "rev-", "commit-"]
        .iter()
        .find_map(|p| lower.strip_prefix(p))
        .unwrap_or(&lower);

    let hex =
        (7..=40).contains(&candidate.len()) && candidate.chars().all(|c| c.is_ascii_hexdigit());
    let all_digits = candidate.chars().all(|c| c.is_ascii_digit());
    (hex && !all_digits).then(|| candidate.to_string())
}

#[cfg(test)]
mod tests;
