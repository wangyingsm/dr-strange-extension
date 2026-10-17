use super::*;
use dr_strange_ext::Value;

/// Both phases, as the host runs them.
fn run(path: &str, text: &str) -> Output {
    let mut chunk = Chunk::default();
    read(path, text, &mut chunk);
    resolve(chunk)
}

/// Two chunks, as the host would split two files — a resource in one
/// referencing one in the other is the ordinary case.
fn run_chunks(files: &[(&str, &str)]) -> Output {
    let mut all = Chunk::default();
    for (path, text) in files {
        let mut chunk = Chunk::default();
        read(path, text, &mut chunk);
        let bytes = rmp_serde::to_vec(&chunk).unwrap();
        let back: Chunk = rmp_serde::from_slice(&bytes).unwrap();
        all.nodes.extend(back.nodes);
        all.edges.extend(back.edges);
        all.roots.extend(back.roots);
        all.declared.extend(back.declared);
        all.skipped += back.skipped;
        all.notes.extend(back.notes);
    }
    resolve(all)
}

fn node_at<'a>(o: &'a Output, key: &str) -> &'a dr_strange_ext::Node {
    o.nodes.iter().find(|n| n.key == key).unwrap_or_else(|| {
        panic!(
            "no node {key} in {:?}",
            o.nodes.iter().map(|n| &n.key).collect::<Vec<_>>()
        )
    })
}

fn props(n: &dr_strange_ext::Node) -> Value {
    n.properties.parse().expect("node properties are JSON")
}

fn prop(n: &dr_strange_ext::Node, name: &str) -> String {
    let v = props(n);
    let at = &v[name];
    at.as_str()
        .or_else(|| at["$value"].as_str())
        .unwrap_or_else(|| panic!("no prop {name} on {}: {v:?}", n.key))
        .to_string()
}

fn has_edge(o: &Output, src: &str, ty: &str, dst: &str) -> bool {
    o.edges
        .iter()
        .any(|e| e.src == src && e.type_ == ty && e.dst == dst)
}

const MAIN: &str = r#"
provider "aws" {
  region = "eu-west-1"
}

variable "ami" {
  type    = string
  default = "ami-0abc"
}

variable "db_password" {
  type      = string
  sensitive = true
  default   = "hunter2"
}

data "aws_availability_zones" "all" {}

resource "aws_subnet" "main" {
  availability_zone = data.aws_availability_zones.all.names[0]
}

resource "aws_instance" "web" {
  ami       = var.ami
  subnet_id = aws_subnet.main.id

  network_interface {
    subnet_id = aws_subnet.main.id
  }
}

resource "aws_ecs_task_definition" "api" {
  container_definitions = "[]"

  container {
    image = "ghcr.io/acme/api:sha-9f2c1b7"
  }
}

module "vpc" {
  source  = "terraform-aws-modules/vpc/aws"
  version = "5.0.0"
  cidr    = var.ami
}

output "instance_id" {
  value = aws_instance.web.id
}

output "db_password" {
  value     = var.db_password
  sensitive = true
}
"#;

/// Every block kind, keyed by Terraform's own address syntax.
#[test]
fn blocks_are_keyed_by_their_terraform_address() {
    let o = run("infra/prod/main.tf", MAIN);

    assert_eq!(
        prop(node_at(&o, "aws_instance.web"), "type"),
        "aws_instance"
    );
    assert_eq!(prop(node_at(&o, "aws_instance.web"), "provider"), "aws");
    assert_eq!(node_at(&o, "aws_instance.web").label, "Infra");
    assert_eq!(
        node_at(&o, "data.aws_availability_zones.all").label,
        "DataSource"
    );
    assert_eq!(
        prop(node_at(&o, "module.vpc"), "source"),
        "terraform-aws-modules/vpc/aws"
    );
    assert_eq!(prop(node_at(&o, "module.vpc"), "version"), "5.0.0");
    assert_eq!(prop(node_at(&o, "provider.aws"), "region"), "eu-west-1");
    assert_eq!(prop(node_at(&o, "var.ami"), "default"), "ami-0abc");

    // The root module is the directory, which is what `apply` is run in.
    assert_eq!(prop(node_at(&o, "infra/prod"), "kind"), "terraform");
    assert!(has_edge(
        &o,
        "aws_instance.web",
        "IN_NAMESPACE",
        "infra/prod"
    ));
    assert!(has_edge(&o, "infra/prod", "USES_MODULE", "module.vpc"));
}

/// The dependency graph Terraform derives from references, which is where
/// almost all of its structure lives.
#[test]
fn a_reference_is_a_dependency() {
    let o = run("infra/prod/main.tf", MAIN);

    // Through an argument, and through a nested block.
    assert!(has_edge(
        &o,
        "aws_instance.web",
        "DEPENDS_ON",
        "aws_subnet.main"
    ));
    // Through an index into a data source's attribute.
    assert!(has_edge(
        &o,
        "aws_subnet.main",
        "DEPENDS_ON",
        "data.aws_availability_zones.all"
    ));
    // A variable is its own edge kind, since it is configuration not order.
    assert!(has_edge(&o, "aws_instance.web", "USES_VARIABLE", "var.ami"));
    assert!(has_edge(&o, "module.vpc", "USES_VARIABLE", "var.ami"));
    // A provider edge, read off the type's prefix.
    assert!(has_edge(
        &o,
        "aws_instance.web",
        "PROVIDED_BY",
        "provider.aws"
    ));
    // An output publishes rather than depends.
    assert!(has_edge(
        &o,
        "output.instance_id",
        "EXPOSES",
        "aws_instance.web"
    ));

    // How the dependency was stated rides the edge.
    let e = o
        .edges
        .iter()
        .find(|e| e.src == "aws_instance.web" && e.dst == "aws_subnet.main")
        .unwrap();
    let p: Value = e.properties.parse().unwrap();
    assert_eq!(p["via"]["$value"].as_str(), Some("reference"));
}

/// An explicit `depends_on` is recorded, and says that it was explicit.
#[test]
fn an_explicit_depends_on_says_so() {
    let o = run(
        "infra/main.tf",
        "resource \"aws_db_instance\" \"db\" {}\nresource \"aws_instance\" \"web\" {\n  depends_on = [aws_db_instance.db]\n}\n",
    );
    let e = o
        .edges
        .iter()
        .find(|e| e.type_ == "DEPENDS_ON" && e.dst == "aws_db_instance.db")
        .expect("no depends_on edge");
    let p: Value = e.properties.parse().unwrap();
    assert_eq!(p["via"]["$value"].as_str(), Some("depends_on"));
}

/// `sensitive = true` is the author declaring it, and is the whole rule.
#[test]
fn a_sensitive_value_is_withheld_and_nothing_else_is() {
    let o = run("infra/prod/main.tf", MAIN);

    let v = node_at(&o, "var.db_password");
    assert_eq!(prop(v, "sensitive"), "true");
    assert_eq!(prop(v, "default"), "(sensitive)");

    let out = node_at(&o, "output.db_password");
    assert_eq!(prop(out, "sensitive"), "true");
    assert!(
        props(out)["value"].is_null(),
        "a sensitive output's value was recorded"
    );

    // The non-sensitive ones are recorded, which is the other half.
    assert_eq!(prop(node_at(&o, "var.ami"), "default"), "ami-0abc");
    assert_eq!(
        prop(node_at(&o, "output.instance_id"), "value"),
        "aws_instance.web.id"
    );

    // And the password is nowhere at all.
    let everything = format!("{:?}{:?}", o.nodes, o.edges);
    assert!(
        !everything.contains("hunter2"),
        "a sensitive default was recorded"
    );
}

/// A container image named in Terraform is the same node the Kubernetes
/// reader would mint, so the join to a commit works from here too.
#[test]
fn an_image_in_terraform_reaches_the_same_node() {
    let o = run("infra/prod/main.tf", MAIN);
    assert!(has_edge(
        &o,
        "aws_ecs_task_definition.api",
        "USES_IMAGE",
        "ghcr.io/acme/api:sha-9f2c1b7"
    ));
    let img = node_at(&o, "ghcr.io/acme/api:sha-9f2c1b7");
    assert_eq!(prop(img, "revision"), "9f2c1b7");
    assert_eq!(prop(img, "registry"), "ghcr.io");
}

/// A reference across files, which a per-chunk pass would miss.
#[test]
fn a_reference_resolves_across_chunks() {
    let o = run_chunks(&[
        (
            "infra/net.tf",
            "resource \"aws_vpc\" \"main\" {\n  cidr_block = \"10.0.0.0/16\"\n}\n",
        ),
        (
            "infra/app.tf",
            "resource \"aws_instance\" \"web\" {\n  vpc_id = aws_vpc.main.id\n}\n",
        ),
    ]);
    assert!(has_edge(
        &o,
        "aws_instance.web",
        "DEPENDS_ON",
        "aws_vpc.main"
    ));
    // One root-module node, though both files named it.
    assert_eq!(o.nodes.iter().filter(|n| n.key == "infra").count(), 1);
}

/// A reference to something no file declares is dropped, not left pointing
/// at a node nobody minted — and the report says how many.
#[test]
fn a_dangling_reference_is_dropped_and_counted() {
    let o = run(
        "infra/main.tf",
        "resource \"aws_instance\" \"web\" {\n  subnet_id = aws_subnet.elsewhere.id\n}\n",
    );
    assert!(!o.edges.iter().any(|e| e.dst == "aws_subnet.elsewhere"));
    assert!(
        o.report
            .notes
            .iter()
            .any(|n| n.contains("does not declare")),
        "{:?}",
        o.report.notes
    );
}

/// Terraform's meta-values are not references to anything declared.
#[test]
fn meta_roots_are_not_dependencies() {
    let o = run(
        "infra/main.tf",
        "resource \"aws_instance\" \"web\" {\n  count = 2\n  name  = \"web-${count.index}\"\n  dir   = path.module\n  tags  = local.tags\n}\n",
    );
    for dst in [
        "count.index",
        "path.module",
        "local.tags",
        "count",
        "path",
        "local",
    ] {
        assert!(
            !o.edges.iter().any(|e| e.dst == dst),
            "{dst} became a dependency"
        );
    }
}

/// Their extensions are claimed so that *nothing* reads them: the host gives
/// each file to one reader with no fallback, so declining here is what keeps
/// a `.tfvars` out of the document reader's prose.
#[test]
fn values_and_state_extensions_are_claimed_to_decline_them() {
    let claimed = <Terraform as Guest>::describe().extensions;
    for ext in ["tf", "tfvars", "tfstate"] {
        assert!(claimed.contains(&ext.to_string()), "{ext} is not claimed");
    }
}

/// Values and state are never read, whatever routes them here.
#[test]
fn tfvars_and_state_are_never_read() {
    for path in [
        "infra/prod.tfvars",
        "infra/secrets.auto.tfvars",
        "infra/terraform.tfstate",
        "infra/terraform.tfstate.backup",
    ] {
        let o = run(path, "db_password = \"hunter2\"\n");
        assert!(o.nodes.is_empty(), "{path} produced {:?}", o.nodes);
        assert_eq!(o.report.skipped, 1, "{path}");
        assert!(
            o.report.notes.iter().any(|n| n.contains("never read")),
            "{path}: {:?}",
            o.report.notes
        );
    }
}
