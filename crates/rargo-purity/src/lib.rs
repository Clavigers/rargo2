use std::collections::{BTreeMap, BTreeSet};

use quote::ToTokens;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, Expr, ImplItem, Item, Path, Token, UseTree};

include!(concat!(env!("OUT_DIR"), "/sources.rs"));

pub const ALLOW: &str = include_str!("../allow.txt");

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Check {
    Async,
    AmbientIo,
    Locks,
    HashOrder,
    PureTests,
    SelfCheck,
}

impl Check {
    pub const ALL: [Check; 6] =
        [Check::Async, Check::AmbientIo, Check::Locks, Check::HashOrder, Check::PureTests, Check::SelfCheck];

    pub fn name(self) -> &'static str {
        match self {
            Check::Async => "async",
            Check::AmbientIo => "ambient-io",
            Check::Locks => "locks",
            Check::HashOrder => "hash-order",
            Check::PureTests => "pure-tests",
            Check::SelfCheck => "self-check",
        }
    }

    pub fn parse(name: &str) -> Option<Check> {
        Check::ALL.into_iter().find(|c| c.name() == name)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Finding {
    pub check: Check,
    pub file: String,
    pub line: usize,
    pub what: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub check: Check,
    pub file: String,
    pub count: usize,
    pub reason: String,
}

pub fn parse_allow(text: &str) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let mut cols = line.splitn(4, '\t');
        let (Some(check), Some(file), Some(count), Some(reason)) = (cols.next(), cols.next(), cols.next(), cols.next())
        else {
            return Err(format!("allow.txt:{}: want check, file, count and reason separated by tabs", n + 1));
        };
        let check = Check::parse(check).ok_or_else(|| format!("allow.txt:{}: no check named {check}", n + 1))?;
        let count = count.parse().map_err(|_| format!("allow.txt:{}: count {count} is not a number", n + 1))?;
        out.push(Entry { check, file: file.to_string(), count, reason: reason.trim().to_string() });
    }
    Ok(out)
}

pub fn compare(check: Check, findings: &[Finding], allow: &[Entry]) -> Result<(), String> {
    let mut found: BTreeMap<&str, Vec<&Finding>> = BTreeMap::new();
    for f in findings.iter().filter(|f| f.check == check) {
        found.entry(f.file.as_str()).or_default().push(f);
    }
    let allowed: BTreeMap<&str, usize> =
        allow.iter().filter(|e| e.check == check).map(|e| (e.file.as_str(), e.count)).collect();
    let mut report = String::new();
    for (file, list) in &found {
        let limit = allowed.get(file).copied().unwrap_or(0);
        if list.len() > limit {
            report.push_str(&format!(
                "{file}: {} found where allow.txt permits {limit}; remove what was added:\n",
                list.len()
            ));
            for f in list {
                report.push_str(&format!("    {}:{}: {}\n", f.file, f.line, f.what));
            }
        }
    }
    for (file, limit) in &allowed {
        let n = found.get(file).map_or(0, |l| l.len());
        if n < *limit {
            report.push_str(&format!(
                "{file}: allow.txt permits {limit} but the code has {n}; lower the entry to {n}{}\n",
                if n == 0 { " by removing it" } else { "" }
            ));
        }
    }
    if report.is_empty() { Ok(()) } else { Err(format!("check {}:\n{report}", check.name())) }
}

pub fn is_shell(file: &str) -> bool {
    file.ends_with("/io.rs") || file.contains("/io/") || file.contains("/examples/") || file.ends_with("build.rs")
}

const AMBIENT: &[&str] = &[
    "std::fs",
    "std::net",
    "std::process",
    "std::os::unix::net",
    "std::os::unix::fs",
    "std::time::Instant::now",
    "std::time::SystemTime::now",
    "std::thread::sleep",
    "std::thread::spawn",
    "std::thread::scope",
    "std::thread::Builder",
    "std::env::var",
    "std::env::var_os",
    "std::env::vars",
    "std::env::vars_os",
    "getrandom",
    "ureq",
    "tempfile",
];

const LOCKS: &[&str] = &[
    "std::sync::Mutex",
    "std::sync::RwLock",
    "std::sync::Condvar",
    "std::sync::OnceLock",
    "std::sync::LazyLock",
    "std::sync::Once",
    "std::sync::Barrier",
    "std::sync::atomic",
    "std::sync::mpsc",
    "parking_lot",
    "dashmap",
];

const HASHED: &[(&str, usize)] = &[
    ("std::collections::HashMap", 3),
    ("std::collections::HashSet", 2),
    ("std::collections::hash_map::HashMap", 3),
    ("std::collections::hash_set::HashSet", 2),
];

const GLOBS: &[&str] = &[
    "std::sync",
    "std::sync::atomic",
    "std::collections",
    "std::fs",
    "std::time",
    "std::thread",
    "std::process",
    "std::net",
    "std::env",
];

const MODULES: &[&str] = &[
    "std::fs",
    "std::net",
    "std::process",
    "std::os::unix::net",
    "std::os::unix::fs",
    "std::thread",
    "std::env",
    "std::time",
    "std::sync",
    "std::sync::atomic",
    "std::sync::mpsc",
    "std::collections",
    "parking_lot",
    "dashmap",
    "getrandom",
    "ureq",
    "tempfile",
];

const GLOB_NAMES: &[&str] = &[
    "File",
    "OpenOptions",
    "DirEntry",
    "ReadDir",
    "Metadata",
    "Command",
    "Child",
    "Stdio",
    "TcpStream",
    "TcpListener",
    "UdpSocket",
    "UnixStream",
    "UnixListener",
    "UnixDatagram",
    "Sender",
    "Receiver",
    "SyncSender",
    "channel",
    "sync_channel",
];

fn under(full: &str, prefix: &str) -> bool {
    full == prefix || full.strip_prefix(prefix).is_some_and(|rest| rest.starts_with("::"))
}

fn normalize(full: String) -> String {
    for root in ["core::", "alloc::"] {
        if let Some(rest) = full.strip_prefix(root) {
            return format!("std::{rest}");
        }
    }
    full
}

fn cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|a| {
        let p = a.path();
        if p.is_ident("test") || p.segments.last().is_some_and(|s| s.ident == "test") {
            return true;
        }
        if !p.is_ident("cfg") {
            return false;
        }
        let text: String = a.meta.to_token_stream().to_string().chars().filter(|c| !c.is_whitespace()).collect();
        text.contains("test") && !text.contains("not(test")
    })
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

#[derive(Default)]
struct Asserts {
    asserts: bool,
    invariants: bool,
}

impl<'a> Visit<'a> for Asserts {
    fn visit_macro(&mut self, m: &'a syn::Macro) {
        let name = m.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
        if name.starts_with("assert") || name.starts_with("debug_assert") || name == "panic" || name == "unreachable" {
            self.asserts = true;
        }
        if let Ok(args) = m.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated) {
            for a in &args {
                self.visit_expr(a);
            }
        }
    }

    fn visit_expr_method_call(&mut self, e: &'a syn::ExprMethodCall) {
        if e.method == "invariants" {
            self.asserts = true;
            self.invariants = true;
        }
        visit::visit_expr_method_call(self, e);
    }

    fn visit_expr_call(&mut self, e: &'a syn::ExprCall) {
        if let Expr::Path(p) = &*e.func
            && p.path.segments.last().is_some_and(|s| s.ident == "invariants")
        {
            self.asserts = true;
            self.invariants = true;
        }
        visit::visit_expr_call(self, e);
    }
}

fn flatten(prefix: &str, tree: &UseTree, names: &mut BTreeMap<String, String>, globs: &mut BTreeSet<String>) {
    let join = |name: &str| if prefix.is_empty() { name.to_string() } else { format!("{prefix}::{name}") };
    match tree {
        UseTree::Path(p) => flatten(&join(&p.ident.to_string()), &p.tree, names, globs),
        UseTree::Name(n) if n.ident == "self" => {
            if let Some(last) = prefix.rsplit("::").next() {
                names.insert(last.to_string(), normalize(prefix.to_string()));
            }
        }
        UseTree::Name(n) => {
            names.insert(n.ident.to_string(), normalize(join(&n.ident.to_string())));
        }
        UseTree::Rename(r) => {
            names.insert(r.rename.to_string(), normalize(join(&r.ident.to_string())));
        }
        UseTree::Glob(_) => {
            globs.insert(normalize(prefix.to_string()));
        }
        UseTree::Group(g) => g.items.iter().for_each(|t| flatten(prefix, t, names, globs)),
    }
}

struct Uses {
    names: BTreeMap<String, String>,
    globs: BTreeSet<String>,
}

impl<'a> Visit<'a> for Uses {
    fn visit_item_use(&mut self, i: &'a syn::ItemUse) {
        flatten("", &i.tree, &mut self.names, &mut self.globs);
    }
}

struct Method {
    predicate: bool,
    ty: String,
    name: String,
    line: usize,
    steps: bool,
    asserts: bool,
}

struct Scan<'f> {
    file: &'f str,
    whole_test: bool,
    shell: bool,
    tests: usize,
    uses: Uses,
    findings: Vec<Finding>,
    methods: Vec<Method>,
    tests_call_invariants: bool,
}

impl Scan<'_> {
    fn testing(&self) -> bool {
        self.whole_test || self.tests > 0
    }

    fn add(&mut self, check: Check, line: usize, what: String) {
        self.findings.push(Finding { check, file: self.file.to_string(), line, what });
    }

    fn resolve(&self, path: &Path) -> String {
        let segs: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        let Some(first) = segs.first() else { return String::new() };
        if let Some(full) = self.uses.names.get(first) {
            let mut out = full.clone();
            for s in &segs[1..] {
                out.push_str("::");
                out.push_str(s);
            }
            return normalize(out);
        }
        let plain = normalize(segs.join("::"));
        if segs.len() == 1 {
            for glob in &self.uses.globs {
                if GLOBS.contains(&glob.as_str()) {
                    let candidate = format!("{glob}::{plain}");
                    let typed = plain.starts_with("Atomic") || GLOB_NAMES.contains(&plain.as_str());
                    let hit = |p: &str| under(&candidate, p) && (p.len() > glob.len() || typed);
                    if AMBIENT.iter().chain(LOCKS).any(|p| hit(p)) || HASHED.iter().any(|(p, _)| hit(p)) {
                        return candidate;
                    }
                }
            }
        }
        plain
    }

    fn denied(&mut self, full: &str, line: usize) {
        if under(full, "std::sync::atomic::Ordering") {
            return;
        }
        let testing = self.testing();
        if let Some(p) = AMBIENT.iter().find(|p| under(full, p)) {
            if testing {
                self.add(Check::PureTests, line, format!("{full} ({p})"));
            } else if !self.shell {
                self.add(Check::AmbientIo, line, full.to_string());
            }
        }
        if let Some(p) = LOCKS.iter().find(|p| under(full, p)) {
            if testing {
                self.add(Check::PureTests, line, format!("{full} ({p})"));
            } else if !self.shell {
                self.add(Check::Locks, line, full.to_string());
            }
        }
    }
}

impl<'a> Visit<'a> for Scan<'_> {
    fn visit_item_use(&mut self, _: &'a syn::ItemUse) {}

    fn visit_attribute(&mut self, a: &'a Attribute) {
        let first = a.path().segments.first().map(|s| s.ident.to_string()).unwrap_or_default();
        if first == "tokio" || first == "async_trait" {
            let line = a.path().segments[0].ident.span().start().line;
            self.add(Check::Async, line, format!("#[{}]", a.path().to_token_stream().to_string().replace(' ', "")));
        }
        if a.path().is_ident("ignore") && self.testing() {
            self.add(Check::PureTests, a.path().segments[0].ident.span().start().line, "#[ignore]".into());
        }
    }

    fn visit_item(&mut self, i: &'a Item) {
        let test = cfg_test(item_attrs(i));
        if test {
            if !self.testing() {
                let mut a = Asserts::default();
                a.visit_item(i);
                self.tests_call_invariants |= a.invariants;
            }
            self.tests += 1;
        }
        visit::visit_item(self, i);
        if test {
            self.tests -= 1;
        }
    }

    fn visit_impl_item(&mut self, i: &'a ImplItem) {
        let test = match i {
            ImplItem::Fn(f) => cfg_test(&f.attrs),
            _ => false,
        };
        if test {
            self.tests += 1;
        }
        visit::visit_impl_item(self, i);
        if test {
            self.tests -= 1;
        }
    }

    fn visit_item_impl(&mut self, i: &'a syn::ItemImpl) {
        if !self.testing()
            && !self.shell
            && i.trait_.is_none()
            && let syn::Type::Path(p) = &*i.self_ty
            && let Some(last) = p.path.segments.last()
        {
            let ty = last.ident.to_string();
            for item in &i.items {
                let ImplItem::Fn(f) = item else { continue };
                if cfg_test(&f.attrs) {
                    continue;
                }
                let steps = matches!(
                    f.sig.receiver(),
                    Some(r) if r.reference.is_some() && r.mutability.is_some()
                );
                let mut a = Asserts::default();
                a.visit_block(&f.block);
                self.methods.push(Method {
                    predicate: !matches!(f.sig.output, syn::ReturnType::Default),
                    ty: ty.clone(),
                    name: f.sig.ident.to_string(),
                    line: f.sig.ident.span().start().line,
                    steps,
                    asserts: a.asserts,
                });
            }
        }
        visit::visit_item_impl(self, i);
    }

    fn visit_item_static(&mut self, i: &'a syn::ItemStatic) {
        if matches!(i.mutability, syn::StaticMutability::Mut(_)) {
            let line = i.ident.span().start().line;
            if self.testing() {
                self.add(Check::PureTests, line, format!("static mut {}", i.ident));
            } else if !self.shell {
                self.add(Check::Locks, line, format!("static mut {}", i.ident));
            }
        }
        visit::visit_item_static(self, i);
    }

    fn visit_signature(&mut self, s: &'a syn::Signature) {
        if s.asyncness.is_some() {
            self.add(Check::Async, s.ident.span().start().line, format!("async fn {}", s.ident));
        }
        visit::visit_signature(self, s);
    }

    fn visit_expr_async(&mut self, e: &'a syn::ExprAsync) {
        self.add(Check::Async, e.async_token.span.start().line, "async block".into());
        visit::visit_expr_async(self, e);
    }

    fn visit_expr_await(&mut self, e: &'a syn::ExprAwait) {
        self.add(Check::Async, e.await_token.span.start().line, ".await".into());
        visit::visit_expr_await(self, e);
    }

    fn visit_expr_closure(&mut self, e: &'a syn::ExprClosure) {
        if let Some(a) = &e.asyncness {
            self.add(Check::Async, a.span.start().line, "async closure".into());
        }
        visit::visit_expr_closure(self, e);
    }

    fn visit_expr_call(&mut self, e: &'a syn::ExprCall) {
        if let Expr::Path(p) = &*e.func {
            let full = self.resolve(&p.path);
            let last = full.rsplit("::").next().unwrap_or("");
            if full.starts_with("libc::") && last.chars().next().is_some_and(|c| c.is_ascii_lowercase()) {
                let line = p.path.segments[0].ident.span().start().line;
                if self.testing() {
                    self.add(Check::PureTests, line, format!("{full} (libc)"));
                } else if !self.shell {
                    self.add(Check::AmbientIo, line, full.clone());
                }
            }
        }
        visit::visit_expr_call(self, e);
    }

    fn visit_macro(&mut self, m: &'a syn::Macro) {
        if let Ok(args) = m.parse_body_with(Punctuated::<Expr, Token![,]>::parse_terminated) {
            for a in &args {
                self.visit_expr(a);
            }
        }
        visit::visit_macro(self, m);
    }

    fn visit_path(&mut self, path: &'a Path) {
        let Some(first) = path.segments.first() else { return };
        let line = first.ident.span().start().line;
        let full = self.resolve(path);
        let local = path.segments.len() == 1 && MODULES.contains(&full.as_str());
        if !local {
            self.denied(&full, line);
        }
        let last = path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
        if (last == "Future" || last == "IntoFuture")
            && (path.segments.len() == 1 || under(&full, "std::future"))
        {
            self.add(Check::Async, line, full.clone());
        }
        if !self.testing() && !self.shell {
            for (p, args) in HASHED {
                if !under(&full, p) {
                    continue;
                }
                let after = full[p.len()..].trim_start_matches("::");
                let explicit = path.segments.iter().any(|s| {
                    (s.ident == "HashMap" || s.ident == "HashSet")
                        && matches!(&s.arguments, syn::PathArguments::AngleBracketed(a) if a.args.len() >= *args)
                });
                if !explicit && after != "with_hasher" && after != "with_capacity_and_hasher" {
                    self.add(Check::HashOrder, line, full.clone());
                }
            }
        }
        visit::visit_path(self, path);
    }
}

fn test_files(parsed: &[(&str, syn::File)]) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (file, parsed) in parsed {
        let dir = match file.rsplit_once('/') {
            Some((d, name)) if name == "lib.rs" || name == "main.rs" || name == "mod.rs" => d.to_string(),
            Some(_) => file.trim_end_matches(".rs").to_string(),
            None => continue,
        };
        for item in &parsed.items {
            if let Item::Mod(m) = item
                && m.content.is_none()
                && cfg_test(&m.attrs)
            {
                for candidate in [format!("{dir}/{}.rs", m.ident), format!("{dir}/{}/mod.rs", m.ident)] {
                    out.insert(candidate, file.to_string());
                }
            }
        }
    }
    out
}

fn manifest(file: &str, text: &str, findings: &mut Vec<Finding>) {
    let mut section = String::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.trim_matches(|c| c == '[' || c == ']').to_string();
            if let Some((_, name)) = section.clone().rsplit_once("dependencies.") {
                dependency(file, n + 1, &section, name.trim_matches('"'), findings);
            }
            continue;
        }
        if !section.ends_with("dependencies") || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().split('.').next().unwrap_or("").trim_matches('"');
        let name = value
            .split("package")
            .nth(1)
            .and_then(|rest| rest.split('"').nth(1))
            .unwrap_or(key)
            .to_string();
        dependency(file, n + 1, &section, &name, findings);
    }
}

fn dependency(file: &str, line: usize, section: &str, name: &str, findings: &mut Vec<Finding>) {
    if name == "tokio" || name.starts_with("tokio-") || name.starts_with("futures") || name.starts_with("async-") || name == "mio" {
        findings.push(Finding { check: Check::Async, file: file.into(), line, what: format!("depends on {name}") });
    }
    if name == "tempfile" && section.contains("dev-dependencies") {
        findings.push(Finding {
            check: Check::PureTests,
            file: file.into(),
            line,
            what: "tempfile is a dev-dependency".into(),
        });
    }
}

pub fn scan(sources: &[(&str, &str)]) -> Vec<Finding> {
    let mut findings = Vec::new();
    let mut parsed = Vec::new();
    for (file, text) in sources {
        if file.ends_with("Cargo.toml") {
            manifest(file, text, &mut findings);
        } else {
            match syn::parse_file(text) {
                Ok(p) => parsed.push((*file, p)),
                Err(e) => findings.push(Finding {
                    check: Check::SelfCheck,
                    file: file.to_string(),
                    line: e.span().start().line,
                    what: format!("does not parse: {e}"),
                }),
            }
        }
    }
    let texts: BTreeMap<&str, &str> = sources.iter().copied().collect();
    let tests = test_files(&parsed);
    let mut machines: BTreeMap<String, (Vec<Method>, bool)> = BTreeMap::new();
    for (file, parsed) in &parsed {
        let text = texts[file];
        let mut uses = Uses { names: BTreeMap::new(), globs: BTreeSet::new() };
        uses.visit_file(parsed);
        let whole_test = file.contains("/tests/") || file.contains("/benches/") || tests.contains_key(*file);
        let mut scan = Scan {
            file,
            whole_test,
            shell: is_shell(file),
            tests: 0,
            uses,
            findings: Vec::new(),
            methods: Vec::new(),
            tests_call_invariants: false,
        };
        scan.visit_file(parsed);
        findings.append(&mut scan.findings);
        let owner = tests.get(*file).cloned().unwrap_or_else(|| file.to_string());
        let calls = scan.tests_call_invariants || (whole_test && text.contains("invariants()"));
        let slot = machines.entry(owner).or_default();
        slot.0.append(&mut scan.methods);
        slot.1 |= calls;
    }
    for (file, (methods, tested)) in machines {
        let types: BTreeSet<&str> =
            methods.iter().filter(|m| m.name == "invariants").map(|m| m.ty.as_str()).collect();
        if types.is_empty() {
            continue;
        }
        for m in methods.iter().filter(|m| types.contains(m.ty.as_str())) {
            if m.name == "invariants" && !m.asserts && !m.predicate {
                findings.push(Finding {
                    check: Check::SelfCheck,
                    file: file.clone(),
                    line: m.line,
                    what: format!("{}::invariants asserts nothing", m.ty),
                });
            } else if m.steps && !m.asserts {
                findings.push(Finding {
                    check: Check::SelfCheck,
                    file: file.clone(),
                    line: m.line,
                    what: format!("{}::{} changes the state and asserts nothing", m.ty, m.name),
                });
            }
        }
        if !tested {
            findings.push(Finding {
                check: Check::SelfCheck,
                file: file.clone(),
                line: 1,
                what: format!("no test calls invariants() on {}", types.into_iter().collect::<Vec<_>>().join(", ")),
            });
        }
    }
    findings.sort();
    findings
}

#[cfg(test)]
mod tests;
