use std::collections::BTreeMap;
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

#[path = "src/test_modules.rs"]
mod test_modules;

// Keep the owner and its external tests in one scan. Edges can chain, so
// group the connected files even if one module also declares its own tests.
fn source_groups<'a>(files: &'a [String], root: &Path) -> Vec<Vec<&'a str>> {
    let parsed: Vec<_> = files.iter().filter(|file| file.ends_with(".rs")).filter_map(|file| {
        let text = std::fs::read_to_string(root.join(file)).unwrap();
        // The scanner itself reports syntax errors.
        syn::parse_file(&text).ok().map(|parsed| (file.as_str(), parsed))
    }).collect();
    let owners = test_modules::test_files(&parsed);
    let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for file in files {
        let mut owner = file.as_str();
        while let Some(parent) = owners.get(owner) {
            owner = parent;
        }
        groups.entry(owner).or_default().push(file);
    }
    groups.into_values().collect()
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
    // Keep small files together to limit test process overhead, but give
    // large modules their own budget.
    const SHARD_BYTES: u64 = 80_000;
    let mut shard = Vec::new();
    let mut bytes = 0;
    let mut index = 0;
    for group in source_groups(&files, &root) {
        let size: u64 = group.iter().map(|file| std::fs::metadata(root.join(file)).unwrap().len()).sum();
        if !shard.is_empty() && bytes + size > SHARD_BYTES {
            write_test(&mut text, index, &shard);
            index += 1;
            shard.clear();
            bytes = 0;
        }
        shard.extend(group);
        bytes += size;
    }
    write_test(&mut text, index, &shard);
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("sources.rs"), text).unwrap();
    println!("cargo:rerun-if-changed={}", root.join("crates").display());
    println!("cargo:rerun-if-changed={}", root.join("Cargo.toml").display());
}

fn write_test(text: &mut String, index: usize, files: &[&str]) {
    writeln!(text, "#[cfg(test)] #[test] fn workspace_purity_{index}() {{ tests::workspace_purity(&{files:?}); }}").unwrap();
}
