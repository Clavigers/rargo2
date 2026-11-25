use rargo_core::db::{Db, Table};
use rargo_core::incr::StateMeta;
use rargo_core::store::Store;
use rargo_core::trace::{MEntry, Manifest};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, state, out] = &args[..] else {
        anyhow::bail!("usage: incrdump STATE_DIR OUT");
    };
    let state = PathBuf::from(state);
    let db = Arc::new(Db::open(&state.join("db.redb"))?);
    let store = Store::open(&state.join("store"), db.clone())?;
    let mut states: Vec<(u64, StateMeta)> = Vec::new();
    db.for_each(Table::Incr, |k, v| {
        let Some(id) = k.get(32..40) else { return };
        let id = u64::from_be_bytes(id.try_into().unwrap());
        let Ok(meta) = bincode::deserialize::<StateMeta>(v) else { return };
        states.push((id, meta));
    });
    let mut newest: BTreeMap<String, (u64, Manifest)> = BTreeMap::new();
    for (id, meta) in states {
        let manifest: Manifest = bincode::deserialize(&store.read(&meta.manifest)?)?;
        let Some(name) = manifest.entries.iter().find_map(|(p, _)| {
            let p = String::from_utf8_lossy(p);
            let rest = &p[p.find("/incremental/")? + "/incremental/".len()..];
            Some(rest.split('/').next()?.to_string())
        }) else {
            continue;
        };
        if newest.get(&name).is_none_or(|(n, _)| *n < id) {
            newest.insert(name, (id, manifest));
        }
    }
    let (mut files, mut bytes) = (0u64, 0u64);
    for (name, (_, manifest)) in &newest {
        let dir = PathBuf::from(out).join(name);
        for (path, entry) in &manifest.entries {
            let MEntry::File { hash, .. } = entry else { continue };
            let p = String::from_utf8_lossy(path);
            let Some(at) = p.find(&format!("/incremental/{name}/")) else { continue };
            let rel = &p[at + "/incremental/".len() + name.len() + 1..];
            let dest = dir.join(rel);
            std::fs::create_dir_all(dest.parent().unwrap())?;
            let data = store.read(hash)?;
            bytes += data.len() as u64;
            files += 1;
            std::fs::write(&dest, data)?;
        }
    }
    println!("{} crates, {files} files, {bytes} bytes", newest.len());
    Ok(())
}
