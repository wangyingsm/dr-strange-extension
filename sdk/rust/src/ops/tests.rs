use super::*;

/// The parts of a reference, including the colon that is a port rather than
/// a tag.
#[test]
fn a_reference_splits_the_way_a_registry_client_reads_it() {
    let r = parse_image("ghcr.io/acme/api:sha-9f2c1b7");
    assert_eq!(r.registry, Some("ghcr.io"));
    assert_eq!(r.repository, "acme/api");
    assert_eq!(r.tag, Some("sha-9f2c1b7"));
    assert_eq!(r.digest, None);
    assert_eq!(r.revision.as_deref(), Some("9f2c1b7"));

    // A bare name has no registry: `docker.io` is not invented.
    let r = parse_image("postgres:17");
    assert_eq!(r.registry, None);
    assert_eq!(r.repository, "postgres");
    assert_eq!(r.tag, Some("17"));

    // `library/postgres` is a path, not a host.
    let r = parse_image("library/postgres:17");
    assert_eq!(r.registry, None);
    assert_eq!(r.repository, "library/postgres");

    // A port in the first segment is a registry, and the colon after it is
    // not a tag.
    let r = parse_image("registry:5000/app");
    assert_eq!(r.registry, Some("registry:5000"));
    assert_eq!(r.repository, "app");
    assert_eq!(r.tag, None);

    let r = parse_image("registry:5000/team/app:v1");
    assert_eq!(r.registry, Some("registry:5000"));
    assert_eq!(r.repository, "team/app");
    assert_eq!(r.tag, Some("v1"));

    // localhost is a host without a dot.
    assert_eq!(parse_image("localhost/app:v1").registry, Some("localhost"));

    // No tag at all.
    let r = parse_image("alpine");
    assert_eq!(r.repository, "alpine");
    assert_eq!(r.tag, None);
    assert_eq!(r.revision, None);
}

/// A digest pins the bytes exactly and says nothing about where they came
/// from, so it is never read as a revision.
#[test]
fn a_digest_is_exact_and_silent() {
    let r = parse_image("ghcr.io/acme/api@sha256:1f2e3d4c5b6a7980");
    assert_eq!(r.digest, Some("sha256:1f2e3d4c5b6a7980"));
    assert_eq!(r.tag, None);
    assert_eq!(r.revision, None);

    // Both, which a careful pipeline writes.
    let r = parse_image("ghcr.io/acme/api:sha-9f2c1b7@sha256:abcdef1234567890");
    assert_eq!(r.tag, Some("sha-9f2c1b7"));
    assert_eq!(r.digest, Some("sha256:abcdef1234567890"));
    assert_eq!(r.revision.as_deref(), Some("9f2c1b7"));
}

/// The join carrier, and every case where there is none. A wrong join is
/// worse than no join, so this table is the one that matters.
#[test]
fn only_a_revision_shaped_tag_carries_the_join() {
    for (tag, want) in [
        // Carries it.
        ("sha-9f2c1b7", Some("9f2c1b7")),
        ("git-abc1234", Some("abc1234")),
        ("rev-9f2c1b7", Some("9f2c1b7")),
        ("commit-9f2c1b7", Some("9f2c1b7")),
        ("sha256-9f2c1b7", Some("9f2c1b7")),
        ("9f2c1b7", Some("9f2c1b7")),
        ("SHA-9F2C1B7", Some("9f2c1b7")),
        (
            "9f2c1b7d8e5a4c3b2f1098765432109876543210",
            Some("9f2c1b7d8e5a4c3b2f1098765432109876543210"),
        ),
        // Does not.
        ("latest", None),
        ("stable", None),
        ("main", None),
        ("master", None),
        ("v2.1.0", None),
        ("2.1.0", None),
        ("17", None),
        ("2024", None),
        ("abc", None),    // too short to be anything but coincidence
        ("abcdef", None), // six characters, still short of git's seven
        ("release-candidate", None),
        ("", None),
        (
            "9f2c1b7d8e5a4c3b2f10987654321098765432101",
            None, // 41 characters: longer than a sha-1
        ),
    ] {
        assert_eq!(revision_in(tag).as_deref(), want, "tag {tag:?}");
    }
}

/// The reason this is in the SDK: two plugins reading the same reference must
/// write the same property, because it is one node under one key.
#[test]
fn the_same_reference_parses_identically_however_it_arrives() {
    let from_compose = parse_image("ghcr.io/acme/api:sha-9f2c1b7");
    let from_k8s = parse_image("ghcr.io/acme/api:sha-9f2c1b7");
    assert_eq!(from_compose, from_k8s);
}
