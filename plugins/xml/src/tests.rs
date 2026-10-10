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

// ---- the MyBatis mapper dialect -------------------------------------------

/// A mapper's statements, as a real one is written: a reused `<sql>` fragment,
/// dynamic branches, bind parameters, and CDATA for the `<` a comparison needs.
const ORDER_MAPPER: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE mapper PUBLIC "-//mybatis.org//DTD Mapper 3.0//EN" "http://mybatis.org/dtd/mybatis-3-mapper.dtd">
<mapper namespace="com.acme.OrderMapper">
  <sql id="cols">id, user_id, total</sql>

  <select id="selectById" parameterType="long" resultType="com.acme.Order">
    SELECT <include refid="cols"/>
    FROM orders
    WHERE id = #{id}
    <if test="onlyPaid">
      AND paid = 1
    </if>
  </select>

  <update id="markPaid">
    UPDATE orders SET paid = 1 WHERE id = #{id}
  </update>

  <select id="olderThan" resultType="com.acme.Order">
    <![CDATA[ SELECT id FROM orders WHERE created < #{cutoff} ]]>
  </select>
</mapper>
"#;

fn prop(o: &Output, key: &str, name: &str) -> String {
    let n = o.nodes.iter().find(|n| n.key == key).expect("node");
    let props: dr_strange_ext::Value = n.properties.parse().expect("properties are JSON");
    let v = props.get(name).expect("property");
    v.get("$value")
        .unwrap_or(v)
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| v.get("$value").unwrap_or(v).to_string())
}

fn has_edge(o: &Output, src: &str, ty: &str, dst: &str) -> bool {
    o.edges
        .iter()
        .any(|e| e.src == src && e.type_ == ty && e.dst == dst)
}

/// The namespace and the id name the Java method, so the statement points at
/// the key the java plugin minted for it.
#[test]
fn a_mapper_names_its_interface_and_statements() {
    let out = run("resources/mapper/OrderMapper.xml", ORDER_MAPPER);
    assert_eq!(
        prop(&out, "resources/mapper/OrderMapper.xml", "namespace"),
        "com.acme.OrderMapper"
    );
    let stmt = "resources/mapper/OrderMapper.xml#selectById";
    assert_eq!(prop(&out, stmt, "kind"), "select");
    assert_eq!(prop(&out, stmt, "result_type"), "com.acme.Order");
    assert_eq!(prop(&out, stmt, "parameter_type"), "long");
    assert!(has_edge(
        &out,
        "resources/mapper/OrderMapper.xml",
        "CONTAINS",
        stmt
    ));
    assert!(has_edge(
        &out,
        stmt,
        "IMPLEMENTS",
        "com.acme.OrderMapper.selectById"
    ));
}

/// The method node is a stand-in, not a declaration: the java plugin owns that
/// key, and `External` is what makes the two agree instead of collide.
#[test]
fn the_method_stand_in_is_external() {
    let out = run("resources/mapper/OrderMapper.xml", ORDER_MAPPER);
    let m = out
        .nodes
        .iter()
        .find(|n| n.key == "com.acme.OrderMapper.selectById")
        .expect("the stand-in");
    assert_eq!(m.label, "Method");
    assert!(m.extra_labels.iter().any(|l| l == "External"));
}

/// An `<include>` splices the fragment it names and a dynamic branch
/// contributes its text — while every marker stays exactly as written. What a
/// `#{…}` turns into is the driver's business, not this reader's.
#[test]
fn a_statement_is_assembled_from_its_parts() {
    let out = run("resources/mapper/OrderMapper.xml", ORDER_MAPPER);
    assert_eq!(
        prop(&out, "resources/mapper/OrderMapper.xml#selectById", "sql"),
        "SELECT id, user_id, total FROM orders WHERE id = #{id} AND paid = 1"
    );
}

/// CDATA is where a statement puts the `<` that XML would otherwise eat.
#[test]
fn cdata_carries_the_sql_it_holds() {
    let out = run("resources/mapper/OrderMapper.xml", ORDER_MAPPER);
    assert_eq!(
        prop(&out, "resources/mapper/OrderMapper.xml#olderThan", "sql"),
        "SELECT id FROM orders WHERE created < #{cutoff}"
    );
}

/// A `refid` naming another file's fragment is counted, not guessed at — and
/// `${…}` is left exactly as written, because what it pastes may be the table.
#[test]
fn what_cannot_be_resolved_is_counted() {
    let out = run(
        "m.xml",
        r#"<mapper namespace="a.B">
  <select id="s">SELECT * FROM ${table} <include refid="other.ns.cols"/></select>
</mapper>"#,
    );
    assert_eq!(prop(&out, "m.xml#s", "sql"), "SELECT * FROM ${table}");
    assert!(
        out.report
            .notes
            .iter()
            .any(|n| n.contains("cross-file include")),
        "{:?}",
        out.report.notes
    );
    assert!(
        out.report.notes.iter().any(|n| n.contains("interpolation")),
        "{:?}",
        out.report.notes
    );
}

/// A statement knows where it is written, which the Java method's node cannot
/// say — its file is the interface.
#[test]
fn a_statement_knows_its_place_in_the_file() {
    let out = run("resources/mapper/OrderMapper.xml", ORDER_MAPPER);
    let n = out
        .nodes
        .iter()
        .find(|n| n.key == "resources/mapper/OrderMapper.xml#markPaid")
        .expect("node");
    let props: dr_strange_ext::Value = n.properties.parse().unwrap();
    assert_eq!(props.get("line").and_then(|v| v.as_u64()), Some(15));
    assert_eq!(props.get("end_line").and_then(|v| v.as_u64()), Some(17));
    assert_eq!(
        props.get("file").and_then(|v| v.as_str()),
        Some("resources/mapper/OrderMapper.xml")
    );
}

/// The reporter's question — *which columns does this query use* — answered on
/// the mapper above: one table, so every column in the statement binds to it.
#[test]
fn a_statement_reaches_the_table_and_columns_it_touches() {
    let out = run("resources/mapper/OrderMapper.xml", ORDER_MAPPER);
    let stmt = "resources/mapper/OrderMapper.xml#selectById";
    assert!(has_edge(&out, stmt, "READS", "orders"));
    for column in ["id", "user_id", "total", "paid"] {
        let key = format!("orders.{column}");
        assert!(
            has_edge(&out, stmt, "USES_COLUMN", &key),
            "{stmt} uses {key}"
        );
        assert!(
            has_edge(&out, "orders", "HAS_COLUMN", &key),
            "orders has {key}"
        );
    }
}

/// A table and a column name something outside the tree, so both are
/// `External` — which is what lets a future reader of the schema itself agree
/// on these keys rather than collide with them.
#[test]
fn tables_and_columns_are_external() {
    let out = run("resources/mapper/OrderMapper.xml", ORDER_MAPPER);
    for key in ["orders", "orders.id"] {
        let n = out.nodes.iter().find(|n| n.key == key).expect(key);
        assert!(n.extra_labels.iter().any(|l| l == "External"), "{key}");
    }
    assert_eq!(
        out.nodes.iter().find(|n| n.key == "orders").unwrap().label,
        "Table"
    );
    assert_eq!(
        out.nodes
            .iter()
            .find(|n| n.key == "orders.id")
            .unwrap()
            .label,
        "Column"
    );
}

/// An update writes where a select reads: the direction impact analysis wants,
/// told by the keyword that introduced the table.
#[test]
fn an_update_writes_its_table() {
    let out = run("resources/mapper/OrderMapper.xml", ORDER_MAPPER);
    let stmt = "resources/mapper/OrderMapper.xml#markPaid";
    assert!(has_edge(&out, stmt, "WRITES", "orders"));
    assert!(!has_edge(&out, stmt, "READS", "orders"));
}

/// `SELECT *` names no columns. The result map is where they are written down,
/// and with one table they bind to it.
#[test]
fn a_result_map_names_the_columns_a_star_select_hides() {
    let out = run(
        "m.xml",
        r#"<mapper namespace="a.OrderMapper">
  <resultMap id="orderMap" type="a.Order">
    <id column="id" property="id"/>
    <result column="user_name" property="userName"/>
  </resultMap>
  <select id="all" resultMap="orderMap">SELECT * FROM orders</select>
</mapper>"#,
    );
    assert!(has_edge(&out, "m.xml#all", "USES_COLUMN", "orders.id"));
    assert!(has_edge(
        &out,
        "m.xml#all",
        "USES_COLUMN",
        "orders.user_name"
    ));
}

/// Two tables and a bare column: it is named on the statement, and no edge
/// puts it under the wrong table.
#[test]
fn an_unbound_column_rides_the_statement() {
    let out = run(
        "m.xml",
        r#"<mapper namespace="a.M">
  <select id="j">SELECT u.id, created FROM users u JOIN orders o ON o.user_id = u.id</select>
</mapper>"#,
    );
    assert_eq!(prop(&out, "m.xml#j", "unbound_columns"), "created");
    assert!(!out.edges.iter().any(|e| e.dst.ends_with(".created")));
    assert!(has_edge(&out, "m.xml#j", "USES_COLUMN", "users.id"));
}

// ---- the MyBatis surface, across its versions -----------------------------

/// `<set>`, `<where>`, `<trim>` and `<foreach>` carry SQL in their attributes
/// rather than their text. Without it an update's assignments read as an alias
/// of the table and the columns are lost.
#[test]
fn dynamic_tags_contribute_the_sql_their_attributes_hold() {
    let out = run(
        "m.xml",
        r#"<mapper namespace="a.M">
  <update id="patch">
    UPDATE orders
    <set>
      <if test="paid != null">paid = #{paid},</if>
      <if test="total != null">total = #{total},</if>
    </set>
    <where>id = #{id}</where>
  </update>
  <insert id="add">
    INSERT INTO orders
    <trim prefix="(" suffix=")" suffixOverrides=",">
      <if test="id != null">id,</if>
      <if test="total != null">total,</if>
    </trim>
    VALUES
    <foreach item="r" collection="rows" open="(" separator="," close=")">#{r.id}</foreach>
  </insert>
</mapper>"#,
    );
    assert_eq!(
        prop(&out, "m.xml#patch", "sql"),
        "UPDATE orders SET paid = #{paid}, total = #{total}, WHERE id = #{id}"
    );
    assert!(has_edge(&out, "m.xml#patch", "WRITES", "orders"));
    for c in ["orders.paid", "orders.total", "orders.id"] {
        assert!(has_edge(&out, "m.xml#patch", "USES_COLUMN", c), "{c}");
    }
    assert_eq!(
        prop(&out, "m.xml#add", "sql"),
        "INSERT INTO orders ( id, total, ) VALUES ( #{r.id} )"
    );
    assert!(has_edge(&out, "m.xml#add", "WRITES", "orders"));
}

/// The same `id` may be written once per vendor, and the container picks by
/// the configured `databaseId`. Both are statements, and neither displaces
/// the other.
#[test]
fn a_statement_per_vendor_is_a_statement_each() {
    let out = run(
        "m.xml",
        r#"<mapper namespace="a.M">
  <select id="now" databaseId="mysql" resultType="long">SELECT UNIX_TIMESTAMP() FROM dual</select>
  <select id="now" databaseId="oracle" resultType="long">SELECT SYSDATE FROM dual</select>
</mapper>"#,
    );
    assert_eq!(prop(&out, "m.xml#now@mysql", "database_id"), "mysql");
    assert_eq!(prop(&out, "m.xml#now@oracle", "database_id"), "oracle");
    // Both implement the one method, which is what the container chooses from.
    assert!(has_edge(&out, "m.xml#now@mysql", "IMPLEMENTS", "a.M.now"));
    assert!(has_edge(&out, "m.xml#now@oracle", "IMPLEMENTS", "a.M.now"));
}

/// A statement may name several result maps, and a map may extend another —
/// both of which name columns a `SELECT *` does not.
#[test]
fn result_maps_compose() {
    let out = run(
        "m.xml",
        r#"<mapper namespace="a.M">
  <resultMap id="base" type="a.Order">
    <id column="id" property="id"/>
  </resultMap>
  <resultMap id="full" type="a.Order" extends="base">
    <result column="total" property="total"/>
    <association column="user_id" property="user" select="a.U.byId"/>
    <discriminator column="kind" javaType="string"/>
  </resultMap>
  <select id="all" resultMap="full">SELECT * FROM orders</select>
</mapper>"#,
    );
    for c in ["orders.id", "orders.total", "orders.user_id", "orders.kind"] {
        assert!(has_edge(&out, "m.xml#all", "USES_COLUMN", c), "{c}");
    }
}
