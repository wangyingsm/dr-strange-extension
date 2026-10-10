//! The plugin's own tests, running natively.

use super::*;

fn run(name: &str, text: &str) -> Output {
    let mut out = output();
    extract(name, text, &mut out);
    out.finish()
}

const MAPPER_DOCTYPE: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE mapper PUBLIC \"-//mybatis.org//DTD Mapper 3.0//EN\" \"http://mybatis.org/dtd/mybatis-3-mapper.dtd\">\n";

/// A dialect is read from the root element the file declares, past the
/// declaration and the DOCTYPE — which is never fetched.
#[test]
fn a_mapper_is_recognised_by_its_root_element() {
    let out = run(
        "resources/mapper/OrderMapper.xml",
        &format!("{MAPPER_DOCTYPE}<mapper namespace=\"com.acme.OrderMapper\">\n</mapper>\n"),
    );
    assert!(
        out.nodes
            .iter()
            .any(|n| n.key == "resources/mapper/OrderMapper.xml" && n.label == "Mapper")
    );
    assert!(out.prose.is_empty(), "a dialect it reads is never prose");
}

/// `<mapper>` without a namespace names no interface, so there is nothing to
/// join it to and it is not this dialect.
#[test]
fn a_mapper_without_a_namespace_is_not_a_mapper() {
    let out = run("odd.xml", "<mapper>\n</mapper>\n");
    assert!(out.nodes.is_empty());
    assert!(!out.prose.is_empty());
}

/// An XML this plugin has no reader for leaves as prose, exactly as an
/// unclaimed file's text does — claiming the extension takes nothing away.
#[test]
fn a_non_mapper_xml_is_prose_not_facts() {
    let text = "<configuration>\n  <appender name=\"STDOUT\"/>\n</configuration>\n";
    let out = run("src/main/resources/logback.xml", text);
    assert!(out.nodes.is_empty());
    assert!(out.edges.is_empty());
    assert_eq!(out.prose, text);
    assert!(out.report.prose_chars > 0);
}

/// A file that opens no element at all is counted and named, never fatal.
#[test]
fn a_malformed_xml_is_counted_not_fatal() {
    let out = run("broken.xml", "not xml at all");
    assert!(out.nodes.is_empty());
    assert_eq!(out.report.skipped, 1);
    assert!(
        out.report.notes.iter().any(|n| n.contains("broken.xml")),
        "{:?}",
        out.report.notes
    );
}

/// Two documents in one call keep their prose apart rather than running
/// together into one paragraph.
#[test]
fn prose_from_two_documents_stays_separated() {
    let mut out = output();
    extract("a.xml", "<a/>", &mut out);
    extract("b.xml", "<b/>", &mut out);
    assert_eq!(out.prose, "<a/>\n\n<b/>");
}
