//! Temp probe: assemble a real tree, assert no duplicate node keys.
use std::collections::BTreeMap;

struct DirFiles(std::path::PathBuf);
impl drsg_py_parser::Files for DirFiles {
    fn read(&self, path: &str) -> Result<Vec<u8>, String> {
        std::fs::read(self.0.join(path)).map_err(|e| e.to_string())
    }
    fn list(&self, _suffix: &str) -> Result<Vec<String>, String> {
        fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<String>) {
            for e in std::fs::read_dir(dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    walk(root, &p, out);
                } else if p.extension().is_some_and(|x| x == "py") {
                    out.push(p.strip_prefix(root).unwrap().to_string_lossy().into_owned());
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.0, &self.0, &mut out);
        out.sort();
        Ok(out)
    }
    fn label(&self) -> Option<String> {
        None
    }
}

#[test]
fn no_duplicate_keys_on_real_tree() {
    let Ok(root) = std::env::var("PROBE_DIR") else {
        return;
    };
    let f = DirFiles(root.into());
    let paths = drsg_py_parser::Files::list(&f, "py").unwrap();
    let a = drsg_py_parser::assemble(drsg_py_parser::parse_chunk(&f, &paths, false));
    let mut count: BTreeMap<&str, usize> = BTreeMap::new();
    for n in &a.nodes {
        *count.entry(n.key.as_str()).or_default() += 1;
    }
    let dups: Vec<_> = count.iter().filter(|(_, c)| **c > 1).collect();
    assert!(
        dups.is_empty(),
        "dup keys: {:?}",
        &dups[..dups.len().min(5)]
    );
}
