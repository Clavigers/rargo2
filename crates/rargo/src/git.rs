mod config;
mod ignore;
mod index;
mod object;
mod sha1;
mod write;

use object::{Id, Object, Store, Type, from_hex, to_hex};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Mode {
    Off,
    #[default]
    Detect,
    Head,
    History,
}

#[derive(Clone)]
pub struct Head {
    pub text: String,
    pub branch: Option<String>,
    pub commit: String,
    pub common: PathBuf,
    pub tags: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    File,
    Dir,
    Link,
}

pub trait Source {
    type Data: AsRef<[u8]>;
    fn read(&self, path: &Path) -> Option<Self::Data>;
    fn list(&self, dir: &Path) -> Option<Vec<(Vec<u8>, Kind)>>;
}

fn is_hex40(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn relative_to(base: &Path, name: &str) -> PathBuf {
    let path = PathBuf::from(name.trim());
    if path.is_absolute() { path } else { base.join(path) }
}

fn git_link(root: &Path, contents: &[u8]) -> Option<PathBuf> {
    let name = std::str::from_utf8(contents).ok()?.strip_prefix("gitdir:")?;
    Some(relative_to(root, name))
}

enum HeadRef<'a> {
    Detached(&'a str),
    Symbolic(&'a str),
}

fn parse_head(contents: &[u8]) -> Option<(&str, HeadRef<'_>)> {
    let text = std::str::from_utf8(contents).ok()?.trim();
    let value = match text.strip_prefix("ref: ") {
        Some(name) => HeadRef::Symbolic(name.trim()),
        None if is_hex40(text) => HeadRef::Detached(text),
        None => return None,
    };
    Some((text, value))
}

fn packed_refs(contents: &[u8]) -> impl Iterator<Item = (&str, &str)> {
    std::str::from_utf8(contents).unwrap_or("").lines().filter_map(|line| {
        let (id, name) = line.split_once(' ')?;
        is_hex40(id).then_some((name, id))
    })
}

fn packed_ref<'a>(contents: &'a [u8], wanted: &str) -> Option<&'a str> {
    packed_refs(contents).find_map(|(name, id)| (name == wanted).then_some(id))
}

fn loose_ref(contents: &[u8]) -> Option<&str> {
    let id = std::str::from_utf8(contents).ok()?.trim();
    is_hex40(id).then_some(id)
}

struct Repo {
    git_dir: PathBuf,
    common: PathBuf,
}

fn repo<S: Source>(src: &S, root: &Path) -> Option<Repo> {
    let dot_git = root.join(".git");
    let git_dir = match src.read(&dot_git) {
        Some(link) => git_link(root, link.as_ref())?,
        None => dot_git,
    };
    let common = src
        .read(&git_dir.join("commondir"))
        .map_or_else(|| git_dir.clone(), |name| relative_to(&git_dir, &String::from_utf8_lossy(name.as_ref())));
    Some(Repo { git_dir, common })
}

struct Resolved {
    text: String,
    branch: Option<String>,
    commit: String,
    common: PathBuf,
}

fn resolve<S: Source>(src: &S, root: &Path) -> Option<Resolved> {
    let Repo { git_dir, common } = repo(src, root)?;
    let contents = src.read(&git_dir.join("HEAD"))?;
    let (text, value) = parse_head(contents.as_ref())?;
    let (branch, commit) = match value {
        HeadRef::Detached(commit) => (None, commit.to_string()),
        HeadRef::Symbolic(name) => {
            let loose = src.read(&git_dir.join(name)).or_else(|| src.read(&common.join(name)));
            let commit = match loose {
                Some(bytes) => loose_ref(bytes.as_ref())?.to_string(),
                None => packed_ref(src.read(&common.join("packed-refs"))?.as_ref(), name)?.to_string(),
            };
            (Some(name.to_string()), commit)
        }
    };
    Some(Resolved { text: text.to_string(), branch, commit, common })
}

fn shallow<S: Source>(src: &S, common: &Path) -> BTreeSet<Id> {
    src.read(&common.join("shallow"))
        .map(|b| b.as_ref().split(|&c| c == b'\n').filter_map(|l| from_hex(l.trim_ascii())).collect())
        .unwrap_or_default()
}

fn parents<S: Source>(src: &S, store: &mut Store<S::Data>, shallow: &BTreeSet<Id>, id: &Id) -> Option<Vec<Id>> {
    if shallow.contains(id) {
        return Some(Vec::new());
    }
    let o = store.read(src, id).filter(|o| o.kind == Type::Commit)?;
    Some(object::commit(&o.data)?.parents)
}

fn first_parents<S: Source>(src: &S, store: &mut Store<S::Data>, shallow: &BTreeSet<Id>, head: Id) -> Option<Vec<Id>> {
    let mut chain = vec![head];
    while let Some(&next) = parents(src, store, shallow, chain.last()?)?.first() {
        chain.push(next);
    }
    Some(chain)
}

fn reachable<S: Source>(src: &S, store: &mut Store<S::Data>, shallow: &BTreeSet<Id>, from: Id) -> Option<BTreeSet<Id>> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![from];
    while let Some(c) = stack.pop() {
        if seen.insert(c) {
            stack.extend(parents(src, store, shallow, &c)?.into_iter().filter(|p| !seen.contains(p)));
        }
    }
    Some(seen)
}

fn owners<S: Source>(src: &S, store: &mut Store<S::Data>, shallow: &BTreeSet<Id>, oldest_first: &[Id]) -> Option<HashMap<Id, usize>> {
    let mut owner = HashMap::new();
    for (k, &c) in oldest_first.iter().enumerate() {
        let mut stack = vec![c];
        while let Some(x) = stack.pop() {
            if owner.contains_key(&x) {
                continue;
            }
            owner.insert(x, k);
            stack.extend(parents(src, store, shallow, &x)?.into_iter().filter(|p| !owner.contains_key(p)));
        }
    }
    Some(owner)
}

fn tag_refs<S: Source>(src: &S, common: &Path) -> BTreeMap<String, Id> {
    let mut refs = BTreeMap::new();
    if let Some(packed) = src.read(&common.join("packed-refs")) {
        for (name, id) in packed_refs(packed.as_ref()) {
            if name.starts_with("refs/tags/") {
                refs.insert(name.to_string(), from_hex(id.as_bytes()).expect("a checked id"));
            }
        }
    }
    let mut stack = vec!["refs/tags".to_string()];
    while let Some(dir) = stack.pop() {
        for (name, kind) in src.list(&common.join(&dir)).unwrap_or_default() {
            let name = format!("{dir}/{}", String::from_utf8_lossy(&name));
            match kind {
                Kind::Dir => stack.push(name),
                _ => {
                    if let Some(id) = src.read(&common.join(&name)).and_then(|b| loose_ref(b.as_ref()).and_then(|i| from_hex(i.as_bytes()))) {
                        refs.insert(name, id);
                    }
                }
            }
        }
    }
    refs
}

fn tags_listed<S: Source>(src: &S, store: &mut Store<S::Data>, common: &Path, head: Id, history: bool) -> Option<String> {
    let shallow = shallow(src, common);
    let mut reached = None;
    let mut listed = String::new();
    for (name, id) in tag_refs(src, common) {
        let o = store.read(src, &id)?;
        let peeled = match o.kind {
            Type::Tag => Some(object::tag(&o.data)?),
            Type::Commit => None,
            _ => continue,
        };
        let mut chain = vec![(id, o.kind)];
        while chain.len() < 64 && chain.last()?.1 == Type::Tag {
            let t = store.read(src, &chain.last()?.0)?;
            chain.push(object::tag(&t.data)?);
        }
        let keep = if history {
            let (end, kind) = *chain.last()?;
            if reached.is_none() {
                reached = Some(reachable(src, store, &shallow, head)?);
            }
            kind == Type::Commit && reached.as_ref()?.contains(&end)
        } else {
            chain.iter().any(|(at, _)| *at == head)
        };
        if keep {
            let peeled = peeled.map(|(t, _)| to_hex(&t)).unwrap_or_default();
            listed.push_str(&format!("{} {} {peeled} {name}\n", to_hex(&id), o.kind.name()));
        }
    }
    Some(listed)
}

fn stand_in_tags(listed: &[u8]) -> (String, Vec<String>) {
    let mut lines: Vec<(String, String, Option<String>)> = Vec::new();
    for line in String::from_utf8_lossy(listed).lines() {
        let parts: Vec<&str> = line.split(' ').collect();
        match parts.as_slice() {
            [oid, "tag", peeled, name] => lines.push((name.to_string(), oid.to_string(), Some(peeled.to_string()))),
            [oid, "commit", "", name] => lines.push((name.to_string(), oid.to_string(), None)),
            _ => {}
        }
    }
    lines.sort();
    let mut text = String::from("# pack-refs with: peeled fully-peeled sorted \n");
    let mut objects = Vec::new();
    for (name, oid, peeled) in lines {
        text.push_str(&format!("{oid} {name}\n"));
        if let Some(p) = peeled {
            text.push_str(&format!("^{p}\n"));
            objects.push(oid);
        }
    }
    (text, objects)
}

fn auto_abbrev(packed: usize) -> usize {
    let bits = (usize::BITS - packed.leading_zeros()) as usize + 1;
    bits.div_ceil(2).max(7)
}

struct Snapshot {
    objects: Vec<(Id, Object)>,
    index: Vec<write::IndexEntry>,
}

fn snapshot<S: Source>(src: &S, store: &mut Store<S::Data>, commit: Id, extra: &[Id]) -> Option<Snapshot> {
    let c = store.read(src, &commit).filter(|o| o.kind == Type::Commit)?;
    let root = object::commit(&c.data)?.tree;
    let mut objects = vec![(commit, c)];
    let mut seen = BTreeSet::new();
    let mut index = Vec::new();
    let mut stack = vec![(root, Vec::new())];
    while let Some((id, prefix)) = stack.pop() {
        let t = store.read(src, &id).filter(|o| o.kind == Type::Tree)?;
        for e in object::tree(&t.data)? {
            let path = if prefix.is_empty() { e.name.to_vec() } else { [prefix.as_slice(), b"/", e.name].concat() };
            match e.mode {
                0o40000 => stack.push((e.id, path)),
                0o120000 => {
                    if seen.insert(e.id) {
                        objects.push((e.id, store.read(src, &e.id)?));
                    }
                    index.push(write::IndexEntry { path, mode: 0o120000, id: e.id });
                }
                0o160000 => index.push(write::IndexEntry { path, mode: 0o160000, id: e.id }),
                m if m & 0o170000 == 0o100000 => {
                    index.push(write::IndexEntry { path, mode: if m & 0o111 != 0 { 0o100755 } else { 0o100644 }, id: e.id })
                }
                _ => {}
            }
        }
        if seen.insert(id) {
            objects.push((id, t));
        }
    }
    for id in extra {
        if seen.insert(*id) {
            objects.push((*id, store.read(src, id)?));
        }
    }
    Some(Snapshot { objects, index })
}

fn pack_of(objects: &[(Id, Object)]) -> write::Packed {
    let list: Vec<(Id, Type, &[u8])> = objects.iter().map(|(id, o)| (*id, o.kind, o.data.as_slice())).collect();
    write::pack(&list)
}

struct StandIn {
    resolved: Resolved,
    history: bool,
    configs: Vec<Vec<u8>>,
}

fn stand_in<S: Source>(src: &S, store: &mut Store<S::Data>, s: &StandIn) -> Option<Vec<(String, Vec<u8>)>> {
    let r = &s.resolved;
    let mut files = vec![("HEAD".to_string(), format!("{}\n", r.text).into_bytes())];
    if let Some(branch) = &r.branch {
        if branch.split('/').any(|c| c.is_empty() || c == "." || c == "..") || branch.starts_with('/') {
            return None;
        }
        files.push((branch.clone(), format!("{}\n", r.commit).into_bytes()));
    }
    if !s.history {
        files.push(("shallow".into(), format!("{}\n", r.commit).into_bytes()));
    }
    let commit = from_hex(r.commit.as_bytes())?;
    let listed = tags_listed(src, store, &r.common, commit, s.history)?;
    let (refs, tag_objects) = stand_in_tags(listed.as_bytes());
    files.push(("packed-refs".into(), refs.into_bytes()));
    let configs: Vec<&[u8]> = s.configs.iter().map(|c| c.as_slice()).collect();
    let min = config::abbrev(&configs).unwrap_or_else(|| auto_abbrev(store.packed_count()));
    let abbrev = store.unique_len(src, &commit, min).clamp(4, 40);
    files.push((
        "config".into(),
        format!("[core]\n\trepositoryformatversion = 0\n\tfilemode = true\n\tbare = false\n\tabbrev = {abbrev}\n").into_bytes(),
    ));
    let tag_ids: Vec<Id> = tag_objects.iter().filter_map(|t| from_hex(t.as_bytes())).collect();
    let snap = snapshot(src, store, commit, &tag_ids)?;
    let packed = pack_of(&snap.objects);
    let name = to_hex(&packed.checksum);
    files.push((format!("objects/pack/pack-{name}.pack"), packed.pack));
    files.push((format!("objects/pack/pack-{name}.idx"), packed.idx));
    files.push(("info/exclude".into(), format!("/{}\n", rargo_proto::GIT_SIDE).into_bytes()));
    files.push(("index".into(), write::index(snap.index)));
    Some(files)
}

fn ignored_in<S: Source>(src: &S, root: &Path, global: Option<&[u8]>) -> Option<Vec<Vec<u8>>> {
    let r = repo(src, root)?;
    let entries = match src.read(&r.git_dir.join("index")) {
        None => Vec::new(),
        Some(bytes) => {
            let parsed = index::parse(bytes.as_ref())?;
            match &parsed.link {
                None => parsed.entries,
                Some(link) => {
                    let shared = src.read(&r.git_dir.join(format!("sharedindex.{}", to_hex(&link.shared))))?;
                    index::merge(parsed, index::parse(shared.as_ref())?)?
                }
            }
        }
    };
    let mut lists = vec![ignore::patterns(global.unwrap_or_default(), b"")];
    let info = src.read(&r.common.join("info/exclude"));
    lists.push(ignore::patterns(info.as_ref().map(|b| b.as_ref()).unwrap_or_default(), b""));
    Some(ignore::ignored(src, root, &entries, lists))
}

pub enum Bytes {
    Owned(Vec<u8>),
    Mapped(memmap2::Mmap),
}

impl AsRef<[u8]> for Bytes {
    fn as_ref(&self) -> &[u8] {
        match self {
            Bytes::Owned(v) => v,
            Bytes::Mapped(m) => m,
        }
    }
}

struct Disk;

impl Source for Disk {
    type Data = Bytes;

    fn read(&self, path: &Path) -> Option<Bytes> {
        use std::io::Read;
        let mut f = std::fs::File::open(path).ok()?;
        let md = f.metadata().ok()?;
        if !md.is_file() {
            return None;
        }
        if md.len() >= 1 << 20 {
            return unsafe { memmap2::Mmap::map(&f) }.ok().map(Bytes::Mapped);
        }
        let mut v = Vec::with_capacity(md.len() as usize);
        f.read_to_end(&mut v).ok()?;
        Some(Bytes::Owned(v))
    }

    fn list(&self, dir: &Path) -> Option<Vec<(Vec<u8>, Kind)>> {
        use std::os::unix::ffi::OsStrExt;
        let mut out: Vec<(Vec<u8>, Kind)> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .filter_map(|e| {
                let t = e.file_type().ok()?;
                let kind = if t.is_symlink() {
                    Kind::Link
                } else if t.is_dir() {
                    Kind::Dir
                } else {
                    Kind::File
                };
                Some((e.file_name().as_bytes().to_vec(), kind))
            })
            .collect();
        out.sort();
        Some(out)
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from)
}

fn xdg_config() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME").filter(|x| !x.is_empty()).map(PathBuf::from).or_else(|| Some(home()?.join(".config")))
}

fn configs(git_dir: &Path, common: &Path) -> Vec<Vec<u8>> {
    let mut paths = Vec::new();
    if std::env::var_os("GIT_CONFIG_NOSYSTEM").is_none() {
        paths.push(PathBuf::from("/etc/gitconfig"));
    }
    paths.extend(xdg_config().map(|x| x.join("git/config")));
    paths.extend(home().map(|h| h.join(".gitconfig")));
    paths.push(common.join("config"));
    paths.push(git_dir.join("config.worktree"));
    paths.into_iter().filter_map(|p| std::fs::read(p).ok()).collect()
}

pub fn head(root: &Path) -> Option<Head> {
    let r = resolve(&Disk, root)?;
    let tags = tags_seal(&r.common);
    Some(Head { text: r.text, branch: r.branch, commit: r.commit, common: r.common, tags })
}

fn first_parent_text<S: Source>(src: &S, store: &mut Store<S::Data>, common: &Path, head: &str, limit: usize) -> Option<String> {
    let shallow = shallow(src, common);
    let mut at = from_hex(head.as_bytes())?;
    let mut text = String::new();
    for _ in 0..limit {
        text.push_str(&to_hex(&at));
        text.push('\n');
        match parents(src, store, &shallow, &at)?.first() {
            Some(p) => at = *p,
            None => break,
        }
    }
    Some(text)
}

pub fn first_parent_list(root: &Path, head: &str, limit: usize) -> Option<String> {
    let r = repo(&Disk, root)?;
    let mut store = Store::open(&Disk, &r.common.join("objects"));
    first_parent_text(&Disk, &mut store, &r.common, head, limit)
}

pub fn ignored(root: &Path) -> Option<Vec<Vec<u8>>> {
    let r = repo(&Disk, root)?;
    let configs = configs(&r.git_dir, &r.common);
    let refs: Vec<&[u8]> = configs.iter().map(|c| c.as_slice()).collect();
    let global = match config::get(&refs, "core", "excludesfile") {
        Some(p) if p.starts_with("~/") => home().map(|h| h.join(&p[2..])),
        Some(p) => Some(root.join(p)),
        None => xdg_config().map(|x| x.join("git/ignore")),
    };
    let global = global.and_then(|p| std::fs::read(p).ok());
    ignored_in(&Disk, root, global.as_deref())
}

fn tags_seal(common: &Path) -> u64 {
    use std::os::unix::fs::MetadataExt;
    let mut records = Vec::new();
    let mut add = |name: &[u8], m: &std::fs::Metadata| {
        records.push((name.to_vec(), m.len(), m.mtime(), m.mtime_nsec()));
    };
    if let Ok(m) = std::fs::metadata(common.join("packed-refs")) {
        add(b"packed-refs", &m);
    }
    let mut stack = vec![common.join("refs/tags")];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let Ok(m) = e.metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else {
                use std::os::unix::ffi::OsStrExt;
                add(e.path().as_os_str().as_bytes(), &m);
            }
        }
    }
    seal(records)
}

fn seal(records: Vec<(Vec<u8>, u64, i64, i64)>) -> u64 {
    let mut h = blake3::Hasher::new();
    for (name, len, seconds, nanos) in records {
        h.update(&name);
        h.update(&len.to_le_bytes());
        h.update(&seconds.to_le_bytes());
        h.update(&nanos.to_le_bytes());
    }
    u64::from_le_bytes(h.finalize().as_bytes()[..8].try_into().unwrap_or_default())
}

fn build(root: &Path, head: &Head, history: bool, dir: &Path) -> Option<()> {
    let r = repo(&Disk, root)?;
    let mut store = Store::open(&Disk, &head.common.join("objects"));
    let resolved = Resolved { text: head.text.clone(), branch: head.branch.clone(), commit: head.commit.clone(), common: head.common.clone() };
    let files = stand_in(&Disk, &mut store, &StandIn { resolved, history, configs: configs(&r.git_dir, &r.common) })?;
    std::fs::create_dir_all(dir.join("refs/heads")).ok()?;
    std::fs::create_dir_all(dir.join("objects/pack")).ok()?;
    for (rel, bytes) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(path, bytes).ok()?;
    }
    Some(())
}

pub fn thin(root: &Path, head: &Head, cache: &Path, history: bool) -> Option<PathBuf> {
    let key = blake3::hash(format!("v4\n{}\n{}\n{}\n{history}", head.text, head.commit, head.tags).as_bytes()).to_hex()[..32].to_string();
    let all = cache.join("git");
    let dir = all.join(&key);
    if dir.join("HEAD").is_file() {
        return Some(dir);
    }
    crate::tmp::sweep(&all, ".tmp-");
    let tmp = all.join(format!(".tmp-{}-{key}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let made = build(root, head, history, &tmp).and_then(|()| match std::fs::rename(&tmp, &dir) {
        Ok(()) => Some(()),
        Err(_) if dir.join("HEAD").is_file() => Some(()),
        Err(_) => None,
    });
    let _ = std::fs::remove_dir_all(&tmp);
    made?;
    for e in std::fs::read_dir(&all).into_iter().flatten().flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
        if e.path() != dir && old.is_some_and(|a| a.as_secs() > 14 * 86400) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    Some(dir)
}

const PIECES: [usize; 4] = [4096, 256, 16, 1];

pub fn history(head: &Head, cache: &Path) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let common = head.common.canonicalize().unwrap_or_else(|_| head.common.clone());
    let key = blake3::hash(common.as_os_str().as_bytes()).to_hex()[..32].to_string();
    let store_dir = cache.join("git-history").join(key);
    let at = store_dir.join("heads").join(&head.commit);
    if at.join("pack").is_dir() {
        return Some(at);
    }
    let pieces_dir = store_dir.join("pieces");
    std::fs::create_dir_all(&pieces_dir).ok()?;
    let mut store = Store::open(&Disk, &head.common.join("objects"));
    let shallow = shallow(&Disk, &head.common);
    let chain = first_parents(&Disk, &mut store, &shallow, from_hex(head.commit.as_bytes())?)?;
    let listed: Vec<String> = chain.iter().map(to_hex).collect();
    let pieces = history_pieces(&listed);
    let have = |piece: &HistoryPiece| ["pack", "idx"].iter().all(|ext| pieces_dir.join(format!("{}.{ext}", piece.key)).is_file());
    let owner = if pieces.iter().all(have) {
        None
    } else {
        let oldest_first: Vec<Id> = chain.iter().rev().copied().collect();
        Some(owners(&Disk, &mut store, &shallow, &oldest_first)?)
    };
    let tmp = store_dir.join("heads").join(format!(".tmp-{}-{}", std::process::id(), head.commit));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(tmp.join("pack")).ok()?;
    for piece in &pieces {
        let (pack, idx) = piece_files(&mut store, &pieces_dir, piece, owner.as_ref())?;
        for (src, ext) in [(&pack, "pack"), (&idx, "idx")] {
            std::fs::hard_link(src, tmp.join("pack").join(format!("pack-{}.{ext}", piece.key))).ok()?;
        }
    }
    match std::fs::rename(&tmp, &at) {
        Ok(()) => {}
        Err(_) if at.join("pack").is_dir() => {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Err(_) => {
            let _ = std::fs::remove_dir_all(&tmp);
            return None;
        }
    }
    sweep_history(&store_dir);
    Some(at)
}

struct HistoryPiece {
    key: String,
    first: usize,
    end: usize,
}

fn history_pieces(first_parent: &[String]) -> Vec<HistoryPiece> {
    let chain: Vec<&str> = first_parent.iter().rev().map(|s| s.as_str()).collect();
    let mut pieces = Vec::new();
    let mut start = 0;
    for size in PIECES {
        while start + size <= chain.len() {
            let before = start.checked_sub(1).map(|i| chain[i]);
            let last = chain[start + size - 1];
            let key = blake3::hash(format!("{}\n{last}", before.unwrap_or("")).as_bytes()).to_hex()[..32].to_string();
            pieces.push(HistoryPiece { key, first: start, end: start + size - 1 });
            start += size;
        }
    }
    pieces
}

fn piece_commits(owner: &HashMap<Id, usize>, piece: &HistoryPiece) -> Vec<Id> {
    let mut commits: Vec<Id> = owner.iter().filter(|(_, k)| (piece.first..=piece.end).contains(*k)).map(|(id, _)| *id).collect();
    commits.sort();
    commits
}

fn piece_files(store: &mut Store<Bytes>, dir: &Path, piece: &HistoryPiece, owner: Option<&HashMap<Id, usize>>) -> Option<(PathBuf, PathBuf)> {
    let (pack, idx) = (dir.join(format!("{}.pack", piece.key)), dir.join(format!("{}.idx", piece.key)));
    if pack.is_file() && idx.is_file() {
        let _ = std::fs::File::options().append(true).open(&pack).and_then(|f| f.set_modified(std::time::SystemTime::now()));
        return Some((pack, idx));
    }
    let mut objects = Vec::new();
    for id in piece_commits(owner?, piece) {
        objects.push((id, store.read(&Disk, &id)?));
    }
    let packed = pack_of(&objects);
    let base = dir.join(format!(".tmp-{}-{}", std::process::id(), piece.key));
    let (made_pack, made_idx) = (base.with_extension("pack"), base.with_extension("idx"));
    let moved = std::fs::write(&made_pack, &packed.pack).is_ok()
        && std::fs::write(&made_idx, &packed.idx).is_ok()
        && std::fs::rename(&made_idx, &idx).is_ok()
        && std::fs::rename(&made_pack, &pack).is_ok();
    let _ = std::fs::remove_file(&made_pack);
    let _ = std::fs::remove_file(&made_idx);
    (moved || (pack.is_file() && idx.is_file())).then_some((pack, idx))
}

fn sweep_history(store: &Path) {
    let stale = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|a| a.as_secs() > 14 * 86400);
    for e in std::fs::read_dir(store.join("heads")).into_iter().flatten().flatten() {
        if stale(&e.path()) {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
    use std::os::unix::fs::MetadataExt;
    for e in std::fs::read_dir(store.join("pieces")).into_iter().flatten().flatten() {
        if e.metadata().is_ok_and(|m| m.nlink() == 1) && stale(&e.path()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

const HISTORY_CRATES: &[&str] = &["vergen", "vergen-gitcl", "vergen-git2", "vergen-gix", "shadow-rs", "git-version", "built", "git-testament"];

fn history_input(rel: &[u8]) -> Option<&[u8]> {
    let name = rel.rsplit(|&b| b == b'/').next().unwrap_or(rel);
    matches!(name, b"build.rs" | b"Cargo.lock").then_some(name)
}

fn needs_history(name: &[u8], text: &str) -> bool {
    if name == b"build.rs" {
        text.contains("describe") || text.contains("rev-list")
    } else {
        text.lines().any(|l| l.strip_prefix("name = \"").and_then(|n| n.strip_suffix('"')).is_some_and(|n| HISTORY_CRATES.contains(&n)))
    }
}

pub fn asks_for_history<'a>(root: &Path, files: impl Iterator<Item = &'a [u8]>) -> bool {
    use std::os::unix::ffi::OsStrExt;
    for rel in files {
        let Some(name) = history_input(rel) else { continue };
        let Ok(text) = std::fs::read_to_string(root.join(std::ffi::OsStr::from_bytes(rel))) else { continue };
        if needs_history(name, &text) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn run(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn thin_git_answers_what_build_scripts_ask() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        std::os::unix::fs::symlink("src/lib.rs", repo.join("link")).unwrap();
        run(&repo, &["init", "-q"]);
        run(&repo, &["checkout", "-q", "-b", "feature/x"]);
        run(&repo, &["add", "-A"]);
        run(&repo, &["commit", "-q", "-m", "first"]);
        std::fs::write(repo.join("src/lib.rs"), "pub fn b() {}\n").unwrap();
        run(&repo, &["commit", "-q", "-am", "second"]);
        let commit = run(&repo, &["rev-parse", "HEAD"]);

        let h = head(&repo).unwrap();
        assert_eq!(h.commit, commit);
        assert_eq!(h.branch.as_deref(), Some("refs/heads/feature/x"));
        let cache = tmp.path().join("cache");
        let thin_dir = thin(&repo, &h, &cache, false).unwrap();
        assert_eq!(thin(&repo, &h, &cache, false).unwrap(), thin_dir, "made once per commit");

        let copy = tmp.path().join("copy");
        std::fs::create_dir_all(copy.join("src")).unwrap();
        std::fs::write(copy.join("src/lib.rs"), "pub fn b() {}\n").unwrap();
        std::os::unix::fs::symlink("src/lib.rs", copy.join("link")).unwrap();
        let status = Command::new("cp").arg("-r").arg(&thin_dir).arg(copy.join(".git")).status().unwrap();
        assert!(status.success());
        assert_eq!(run(&copy, &["rev-parse", "HEAD"]), commit);
        assert_eq!(run(&copy, &["log", "-1", "--format=%H %s"]), format!("{commit} second"));
        assert_eq!(run(&copy, &["rev-parse", "--abbrev-ref", "HEAD"]), "feature/x");
        assert_eq!(run(&copy, &["status", "--porcelain"]), "");
        let short = run(&repo, &["rev-parse", "--short", "HEAD"]);
        assert_eq!(run(&copy, &["describe", "--always", "--dirty"]), short);
        assert_eq!(run(&copy, &["log", "-1", "--format=%h"]), short);
        run(&repo, &["config", "core.abbrev", "11"]);
        let h11 = head(&repo).unwrap();
        let other_cache = tmp.path().join("cache11");
        let thin11 = thin(&repo, &h11, &other_cache, false).unwrap();
        assert!(std::fs::read_to_string(thin11.join("config")).unwrap().contains("abbrev = 11"));
        run(&repo, &["config", "--unset", "core.abbrev"]);
        std::fs::write(copy.join("src/lib.rs"), "pub fn c() {}\n").unwrap();
        assert_eq!(run(&copy, &["status", "--porcelain"]), "M src/lib.rs");
        assert!(run(&copy, &["describe", "--always", "--dirty"]).ends_with("-dirty"));
        assert_eq!(run(&copy, &["rev-list", "--count", "HEAD"]), "1");
        let size: u64 = walk(&thin_dir);
        assert!(size < 20_000, "{size} bytes");

        let wt = tmp.path().join("wt");
        run(&repo, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), "HEAD~1"]);
        let h2 = head(&wt).unwrap();
        assert_eq!(h2.commit, run(&repo, &["rev-parse", "HEAD~1"]));
        assert!(h2.branch.is_none());
        assert_ne!(thin(&wt, &h2, &cache, false).unwrap(), thin_dir);
        assert!(head(tmp.path()).is_none());
    }

    fn show(stand_in: &Path, history: Option<&Path>, copy: &Path) {
        let _ = std::fs::remove_dir_all(copy.join(".git"));
        assert!(Command::new("cp").arg("-r").arg(stand_in).arg(copy.join(".git")).status().unwrap().success());
        if let Some(h) = history {
            let status = Command::new("sh").arg("-c").arg(format!("cp -r '{}'/. '{}'", h.display(), copy.join(".git/objects").display())).status().unwrap();
            assert!(status.success());
        }
    }

    #[test]
    fn with_history_describe_and_counts_answer_as_on_the_client() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q"]);
        let commit = |msg: &str| {
            let file = if msg.starts_with("side") || msg == "elsewhere" { "g" } else { "f" };
            std::fs::write(repo.join(file), msg).unwrap();
            run(&repo, &["add", "-A"]);
            run(&repo, &["commit", "-q", "-m", msg]);
        };
        commit("one");
        run(&repo, &["tag", "-a", "v0.1.0", "-m", "first release"]);
        commit("two");
        commit("three");
        run(&repo, &["checkout", "-q", "-b", "side", "HEAD~1"]);
        commit("side one");
        run(&repo, &["tag", "side-light"]);
        commit("side two");
        run(&repo, &["checkout", "-q", "main"]);
        run(&repo, &["merge", "-q", "--no-ff", "-m", "merge side", "side"]);
        commit("four");
        run(&repo, &["tag", "-a", "v0.2.0", "-m", "second release"]);
        commit("five");
        commit("six");
        run(&repo, &["checkout", "-q", "side"]);
        commit("elsewhere");
        run(&repo, &["tag", "elsewhere"]);
        run(&repo, &["checkout", "-q", "main"]);

        let h = head(&repo).unwrap();
        let cache = tmp.path().join("cache");
        let objects = history(&h, &cache).unwrap();
        let stand_in = thin(&repo, &h, &cache, true).unwrap();
        assert!(!stand_in.join("shallow").exists());
        let copy = tmp.path().join("copy");
        std::fs::create_dir_all(&copy).unwrap();
        std::fs::write(copy.join("f"), "six").unwrap();
        std::fs::write(copy.join("g"), "side two").unwrap();
        show(&stand_in, Some(&objects), &copy);
        for q in [
            &["describe"][..],
            &["describe", "--tags"],
            &["describe", "--tags", "--long", "--abbrev=12"],
            &["describe", "--tags", "--abbrev=0"],
            &["describe", "--always", "--dirty"],
            &["rev-list", "--count", "HEAD"],
            &["rev-list", "--count", "v0.1.0..HEAD"],
            &["log", "-1", "--date=short", "--abbrev=9", "--format=%H %h %cd %(describe:tags)"],
            &["log", "--format=%s"],
            &["tag", "--merged", "HEAD"],
        ] {
            assert_eq!(run(&copy, q), run(&repo, q), "git {q:?}");
        }
        assert_eq!(run(&copy, &["rev-list", "--count", "HEAD"]), "9");
        assert_eq!(run(&copy, &["tag"]), run(&repo, &["tag", "--merged", "HEAD"]), "only the tags HEAD reaches");

        let pieces = objects.parent().unwrap().parent().unwrap().join("pieces");
        let before = packs(&pieces);
        commit("seven");
        let h7 = head(&repo).unwrap();
        let objects7 = history(&h7, &cache).unwrap();
        assert_eq!(objects7.parent(), objects.parent(), "one store per repository");
        assert_eq!(packs(&pieces), before + 1);
        assert_eq!(packs(&objects7.join("pack")), before + 1, "the first-parent chain is 8 commits: 8 pieces of one");
        let seen = thin(&repo, &h7, &cache, true).unwrap();
        run(&repo, &["tag", "-a", "v0.3.0", "-m", "third release"]);
        let tagged = head(&repo).unwrap();
        assert_ne!(tagged.tags, h7.tags, "the tags' seal changed");
        let stand_in = thin(&repo, &tagged, &cache, true).unwrap();
        assert_ne!(stand_in, seen);
        std::fs::write(copy.join("f"), "seven").unwrap();
        show(&stand_in, Some(&objects7), &copy);
        assert_eq!(run(&copy, &["describe"]), "v0.3.0");

        let bare = thin(&repo, &tagged, &cache, false).unwrap();
        show(&bare, None, &copy);
        assert_eq!(run(&copy, &["describe", "--tags"]), "v0.3.0");
        assert_eq!(run(&copy, &["rev-list", "--count", "HEAD"]), "1");
    }

    fn packs(dir: &Path) -> usize {
        std::fs::read_dir(dir).unwrap().flatten().filter(|e| e.path().extension().is_some_and(|x| x == "pack")).count()
    }

    struct Fixture {
        files: BTreeMap<PathBuf, (u8, Vec<u8>)>,
        expect: BTreeMap<String, Vec<u8>>,
    }

    const AT: &str = "/fx-root";

    fn fixture() -> Fixture {
        let b: &[u8] = include_bytes!("git/fixture.bin");
        let mut f = Fixture { files: BTreeMap::new(), expect: BTreeMap::new() };
        let mut i = 0;
        while i < b.len() {
            let j = i + b[i..].iter().position(|&c| c == 0).unwrap();
            let k = j + 1 + b[j + 1..].iter().position(|&c| c == 0).unwrap();
            let n: usize = std::str::from_utf8(&b[j + 1..k]).unwrap().parse().unwrap();
            let (kind, name) = (b[i], String::from_utf8(b[i + 1..j].to_vec()).unwrap());
            let data = b[k + 1..k + 1 + n].to_vec();
            i = k + 1 + n;
            if kind == b'x' {
                f.expect.insert(name, data);
            } else {
                f.files.insert(Path::new(AT).join(name), (kind, data));
            }
        }
        f
    }

    fn normal(path: &Path) -> PathBuf {
        let mut out = PathBuf::new();
        for c in path.components() {
            match c {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                other => out.push(other),
            }
        }
        out
    }

    impl Source for Fixture {
        type Data = Vec<u8>;

        fn read(&self, path: &Path) -> Option<Vec<u8>> {
            self.files.get(&normal(path)).filter(|(k, _)| *k == b'f').map(|(_, d)| d.clone())
        }

        fn list(&self, dir: &Path) -> Option<Vec<(Vec<u8>, Kind)>> {
            use std::os::unix::ffi::OsStrExt;
            let dir = &normal(dir);
            let mut out: BTreeMap<Vec<u8>, Kind> = BTreeMap::new();
            for (path, (kind, _)) in self.files.range(dir.to_path_buf()..) {
                let Ok(rest) = path.strip_prefix(dir) else { break };
                let mut parts = rest.components();
                let Some(first) = parts.next() else { continue };
                let name = first.as_os_str().as_bytes().to_vec();
                let k = match (parts.next(), kind) {
                    (Some(_), _) | (None, b'd') => Kind::Dir,
                    (None, b'l') => Kind::Link,
                    _ => Kind::File,
                };
                out.insert(name, k);
            }
            (!out.is_empty() || self.files.keys().any(|p| p == dir)).then(|| out.into_iter().collect())
        }
    }

    fn lines(b: &[u8]) -> Vec<String> {
        String::from_utf8_lossy(b).lines().map(str::to_string).collect()
    }

    fn id(s: &str) -> Id {
        from_hex(s.as_bytes()).unwrap()
    }

    #[test]
    fn every_object_of_a_real_repository_is_read_from_loose_files_and_both_kinds_of_delta() {
        let f = fixture();
        let objects = Path::new(AT).join("fx/.git/objects");
        let mut store = Store::open(&f, &objects);
        assert_eq!(store.packs_len(), 2, "one pack with ref deltas, one with offset deltas");
        let listed = lines(&f.expect["all-objects"]);
        assert!(listed.len() > 30);
        let mut loose = 0;
        for line in listed {
            let [hex, kind, size] = line.split(' ').collect::<Vec<_>>()[..] else { panic!("{line}") };
            let o = store.read(&f, &id(hex)).unwrap_or_else(|| panic!("{hex} is readable"));
            assert_eq!((o.kind.name(), o.data.len().to_string()), (kind, size.to_string()), "{hex}");
            assert_eq!(to_hex(&object::object_id(o.kind, &o.data)), hex, "the content hashes to its name");
            loose += f.files.contains_key(&objects.join(&hex[..2]).join(&hex[2..])) as usize;
        }
        assert!(loose > 3, "loose objects are read too");
        assert!(store.read(&f, &[7; 20]).is_none());
        let refs = store.delta_kinds();
        assert!(refs.iter().any(|&(r, _)| r > 0) && refs.iter().any(|&(_, o)| o > 0), "{refs:?}");
    }

    #[test]
    fn head_resolves_in_a_repository_its_linked_worktrees_and_a_submodule() {
        let f = fixture();
        let at = |p: &str| Path::new(AT).join(p);
        let main = resolve(&f, &at("fx")).unwrap();
        assert_eq!(main.commit, lines(&f.expect["head"])[0]);
        assert_eq!(main.branch.as_deref(), Some("refs/heads/main"));
        let wt = resolve(&f, &at("wt")).unwrap();
        assert_eq!((wt.commit.as_str(), wt.branch.as_deref()), (lines(&f.expect["wt-head"])[0].as_str(), Some("refs/heads/feature/x")));
        assert_eq!(wt.common, at("wt/../fx/.git/worktrees/wt/../.."));
        let detached = resolve(&f, &at("wt-detached")).unwrap();
        assert_eq!((detached.commit.as_str(), detached.branch), (lines(&f.expect["wt-detached-head"])[0].as_str(), None));
        let module = resolve(&f, &at("fx/nested/module")).unwrap();
        assert_eq!(module.commit, lines(&f.expect["module-head"])[0]);
        assert!(resolve(&f, &at("nowhere")).is_none());

        let mut shadowed = fixture();
        let own = "0123456789012345678901234567890123456789";
        shadowed.files.insert(at("fx/.git/worktrees/wt/refs/heads/feature/x"), (b'f', format!("{own}\n").into_bytes()));
        assert_eq!(resolve(&shadowed, &at("wt")).unwrap().commit, own, "the worktree's own ref shadows the shared one");
        let mut packed_only = fixture();
        packed_only.files.remove(&at("fx/.git/refs/heads/feature/x"));
        packed_only.files.insert(at("fx/.git/packed-refs"), (b'f', format!("{own} refs/heads/feature/x\n").into_bytes()));
        assert_eq!(resolve(&packed_only, &at("wt")).unwrap().commit, own, "and packed refs come last");
    }

    #[test]
    fn indexes_of_version_two_four_and_a_split_index_list_what_git_lists() {
        let f = fixture();
        for (git_dir, expect, version) in [("fx/.git", "stage", 2), ("fx/.git/worktrees/wt", "wt-stage", 4), ("fx/.git/worktrees/wt-detached", "wt-detached-stage", 2)] {
            let dir = Path::new(AT).join(git_dir);
            let bytes = f.read(&dir.join("index")).unwrap();
            assert_eq!(u32::from_be_bytes(bytes[4..8].try_into().unwrap()), version, "{git_dir}");
            let parsed = index::parse(&bytes).unwrap();
            assert_eq!(parsed.link.is_some(), expect == "wt-detached-stage", "{git_dir}");
            let entries = match &parsed.link {
                Some(link) => {
                    let shared = f.read(&dir.join(format!("sharedindex.{}", to_hex(&link.shared)))).unwrap();
                    index::merge(parsed, index::parse(&shared).unwrap()).unwrap()
                }
                None => parsed.entries,
            };
            let shown: Vec<String> = entries.iter().map(|e| format!("{:o} {} 0\t{}", e.mode, to_hex(&e.id), String::from_utf8_lossy(&e.path))).collect();
            assert_eq!(shown, lines(&f.expect[expect]), "{git_dir}");
        }
    }

    #[test]
    fn the_ignored_listing_is_what_git_ls_files_prints() {
        let f = fixture();
        let listed = ignored_in(&f, &Path::new(AT).join("fx"), None).unwrap();
        let git: Vec<&[u8]> = f.expect["ignored"].split(|&b| b == 0).filter(|p| !p.is_empty()).collect();
        let ours: Vec<&[u8]> = listed.iter().map(|p| p.as_slice()).collect();
        let show = |v: &[&[u8]]| v.iter().map(|p| String::from_utf8_lossy(p).into_owned()).collect::<Vec<_>>();
        assert_eq!(show(&ours), show(&git));
        let global = ignored_in(&f, &Path::new(AT).join("fx"), Some(b"*.rs\n")).unwrap();
        assert!(global.contains(&b"mixed/b.rs".to_vec()) && !global.contains(&b"src/lib.rs".to_vec()), "a global pattern, never a tracked file");
    }

    #[test]
    fn tags_history_and_the_stand_in_match_what_git_reports() {
        let f = fixture();
        let common = Path::new(AT).join("fx/.git");
        let mut store = Store::open(&f, &common.join("objects"));
        let head = id(&lines(&f.expect["head"])[0]);
        let chain: Vec<String> = first_parents(&f, &mut store, &BTreeSet::new(), head).unwrap().iter().map(to_hex).collect();
        assert_eq!(chain, lines(&f.expect["first-parent"]));
        let text = first_parent_text(&f, &mut store, &common, &chain[0], 64).unwrap();
        assert_eq!(text, String::from_utf8_lossy(&f.expect["first-parent"]));
        assert_eq!(lines(first_parent_text(&f, &mut store, &common, &chain[0], 2).unwrap().as_bytes()), chain[..2]);
        let parent = id(&chain[1]);
        assert_eq!(stand_in_tags(tags_listed(&f, &mut store, &common, parent, false).unwrap().as_bytes()), stand_in_tags(&f.expect["points-at"]));
        assert_eq!(stand_in_tags(tags_listed(&f, &mut store, &common, head, true).unwrap().as_bytes()), stand_in_tags(&f.expect["merged"]));

        let oldest_first: Vec<Id> = chain.iter().rev().map(|c| id(c)).collect();
        let owner = owners(&f, &mut store, &BTreeSet::new(), &oldest_first).unwrap();
        let n = oldest_first.len();
        let range = HistoryPiece { key: String::new(), first: n - 3, end: n - 2 };
        let mut expect: Vec<Id> = lines(&f.expect["range"]).iter().map(|c| id(c)).collect();
        expect.sort();
        assert_eq!(piece_commits(&owner, &range), expect, "rev-list HEAD~1 ^HEAD~3");
        assert_eq!(owner.len(), reachable(&f, &mut store, &BTreeSet::new(), head).unwrap().len());

        let shallow_common = Path::new(AT).join("shallow/.git");
        let mut shallow_store = Store::open(&f, &shallow_common.join("objects"));
        let cut = shallow(&f, &shallow_common);
        assert_eq!(cut.len(), 1);
        let shallow_chain: Vec<String> = first_parents(&f, &mut shallow_store, &cut, head).unwrap().iter().map(to_hex).collect();
        assert_eq!(shallow_chain, lines(&f.expect["shallow-first-parent"]));

        let short = lines(&f.expect["short"])[0].len();
        assert_eq!(store.unique_len(&f, &head, auto_abbrev(store.packed_count())), short);
        let snap = snapshot(&f, &mut store, head, &[]).unwrap();
        let ours: BTreeSet<String> = snap.objects.iter().filter(|(_, o)| o.kind != Type::Blob).map(|(i, _)| to_hex(i)).collect();
        let theirs: BTreeSet<String> = lines(&f.expect["objects"]).into_iter().collect();
        assert_eq!(ours, theirs, "rev-list --objects --filter=blob:none --no-walk");
        let tree: Vec<String> = f.expect["ls-tree"].split(|&b| b == 0).filter(|r| !r.is_empty()).map(|r| String::from_utf8_lossy(r).into_owned()).collect();
        let mut index = snap.index;
        index.sort_by(|a, b| a.path.cmp(&b.path));
        let shown: Vec<String> = index
            .iter()
            .map(|e| {
                let kind = if e.mode == 0o160000 { "commit" } else { "blob" };
                format!("{:06o} {kind} {}\t{}", e.mode, to_hex(&e.id), String::from_utf8_lossy(&e.path))
            })
            .collect();
        assert_eq!(shown, tree, "ls-tree -r");
        let links: BTreeSet<String> = tree.iter().filter(|l| l.starts_with("120000")).map(|l| l[12..52].to_string()).collect();
        assert!(!links.is_empty() && links.iter().all(|l| snap.objects.iter().any(|(i, _)| to_hex(i) == *l)), "symbolic links travel as blobs");
    }

    #[test]
    fn written_packs_and_indexes_read_back() {
        let f = fixture();
        let mut store = Store::open(&f, &Path::new(AT).join("fx/.git/objects"));
        let head = id(&lines(&f.expect["head"])[0]);
        let snap = snapshot(&f, &mut store, head, &[]).unwrap();
        let packed = pack_of(&snap.objects);
        assert_eq!(packed.pack[packed.pack.len() - 20..], sha1::sha1(&packed.pack[..packed.pack.len() - 20]));
        assert_eq!(packed.idx[packed.idx.len() - 20..], sha1::sha1(&packed.idx[..packed.idx.len() - 20]));
        let mut written = Fixture { files: BTreeMap::new(), expect: BTreeMap::new() };
        written.files.insert(PathBuf::from("/o/pack/pack-x.idx"), (b'f', packed.idx.clone()));
        written.files.insert(PathBuf::from("/o/pack/pack-x.pack"), (b'f', packed.pack.clone()));
        let mut back = Store::open(&written, Path::new("/o"));
        assert_eq!(back.packed_count(), snap.objects.len());
        for (i, o) in &snap.objects {
            assert_eq!(back.read(&written, i).as_ref(), Some(o));
        }
        let entries: Vec<index::Entry> = snap.index.iter().map(|e| index::Entry { path: e.path.clone(), mode: e.mode, id: e.id }).collect();
        let mut sorted = entries.clone();
        sorted.sort_by(|a, b| a.path.cmp(&b.path));
        let bytes = write::index(snap.index);
        assert_eq!(bytes[bytes.len() - 20..], sha1::sha1(&bytes[..bytes.len() - 20]));
        assert_eq!(index::parse(&bytes).unwrap().entries, sorted);
    }

    #[test]
    fn history_piece_boundaries_and_detection_are_pure() {
        let listed: Vec<String> = (0..17).rev().map(|i| format!("{i:040x}")).collect();
        let pieces = history_pieces(&listed);
        let key = |before: Option<u32>, last: u32| {
            let before = before.map(|b| format!("{b:040x}")).unwrap_or_default();
            blake3::hash(format!("{before}\n{last:040x}").as_bytes()).to_hex()[..32].to_string()
        };
        assert_eq!(pieces.len(), 2);
        assert_eq!((pieces[0].key.clone(), pieces[0].first, pieces[0].end), (key(None, 15), 0, 15));
        assert_eq!((pieces[1].key.clone(), pieces[1].first, pieces[1].end), (key(Some(15), 16), 16, 16));
        assert_eq!(auto_abbrev(0), 7);
        assert_eq!(auto_abbrev(1 << 20), 11);
        assert!(!needs_history(b"build.rs", "git rev-parse HEAD"));
        assert!(needs_history(b"build.rs", "git describe --tags"));
        assert!(needs_history(b"build.rs", "git rev-list HEAD"));
        assert!(needs_history(b"Cargo.lock", "name = \"vergen-gitcl\"\n"));
        assert!(!needs_history(b"Cargo.lock", "name = \"serde\"\n"));
        assert_eq!(history_input(b"nested/build.rs"), Some(b"build.rs".as_slice()));
        assert_eq!(history_input(b"nested/src/lib.rs"), None);
    }

    fn walk(dir: &Path) -> u64 {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| if e.path().is_dir() { walk(&e.path()) } else { e.metadata().unwrap().len() })
            .sum()
    }
}
