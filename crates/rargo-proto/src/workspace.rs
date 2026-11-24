//! Cargo manifest selection shared by the worktree scanner and the snapshot reader.

use std::path::{Component, Path, PathBuf};

/// The directory for manifest discovery, relative to the synced worktree.
pub fn command_dir(root: &Path, cwd: &Path, args: &[String]) -> Option<PathBuf> {
    let mut cargo_args = args.iter().take_while(|arg| arg.as_str() != "--");
    let mut start = cwd.to_path_buf();
    while let Some(arg) = cargo_args.next() {
        let path = if arg == "--manifest-path" {
            cargo_args.next()?.as_str()
        } else if let Some(path) = arg.strip_prefix("--manifest-path=") {
            path
        } else {
            continue;
        };
        start = cwd.join(path).parent()?.to_owned();
        break;
    }
    let mut dir = PathBuf::new();
    for component in start.strip_prefix(root).ok()?.components() {
        match component {
            Component::Normal(name) => dir.push(name),
            Component::CurDir => {}
            Component::ParentDir if dir.pop() => {}
            _ => return None,
        }
    }
    Some(dir)
}

/// The nearest enclosing workspace, or the nearest package outside a workspace.
pub fn manifest_for(start: &Path, read: impl Fn(&Path) -> Option<String>) -> Option<(PathBuf, &'static str)> {
    let mut package = None;
    for dir in start.ancestors() {
        let path = dir.join("Cargo.toml");
        let Some(table) = read(&path).and_then(|text| text.parse::<toml::Table>().ok()) else { continue };
        if table.contains_key("workspace") {
            return Some((path, "workspace"));
        }
        if package.is_none() && table.contains_key("package") {
            package = Some((path, "package"));
        }
    }
    package
}
