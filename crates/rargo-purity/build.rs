use std::fmt::Write;
use std::path::Path;

fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if path.is_dir() {
            if name != "target" && !name.starts_with('.') {
                walk(&path, root, out);
            }
        } else if name.ends_with(".rs") || name == "Cargo.toml" {
            out.push(path.strip_prefix(root).unwrap().to_string_lossy().to_string());
        }
    }
}

fn source_group(path: &str) -> &str {
    const CRATES: &str = "crates/";
    path.strip_prefix(CRATES)
        .and_then(|rest| rest.find('/').map(|end| &path[..=CRATES.len() + end]))
        .unwrap_or(path)
}

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&manifest).parent().unwrap().parent().unwrap().to_path_buf();
    let mut files = vec!["Cargo.toml".to_string()];
    walk(&root.join("crates"), &root, &mut files);
    let mut text = String::from("pub const SOURCES: &[(&str, &str)] = &[\n");
    for file in &files {
        writeln!(text, "    ({file:?}, include_str!({:?})),", root.join(file).display().to_string()).unwrap();
    }
    text.push_str("];\n");
    // Keep a crate's sources together so the scanner can associate external
    // test modules with the state machines they exercise.
    for files in files.chunk_by(|a, b| source_group(a) == source_group(b)) {
        let prefix = source_group(&files[0]);
        let name = prefix.strip_prefix("crates/").map_or("manifest".to_string(), |name| {
            name.trim_end_matches('/').replace('-', "_")
        });
        writeln!(text, "#[cfg(test)] #[test] fn workspace_purity_{name}() {{ tests::workspace_purity({prefix:?}); }}").unwrap();
    }
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("sources.rs"), text).unwrap();
    println!("cargo:rerun-if-changed={}", root.join("crates").display());
    println!("cargo:rerun-if-changed={}", root.join("Cargo.toml").display());
}
