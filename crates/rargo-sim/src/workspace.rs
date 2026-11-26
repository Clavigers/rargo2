//! A generated cargo workspace and the model planner that turns it into rargo's unit graph.
//!
//! The planner resolves features the way cargo does (unified per crate, optional dependencies
//! switched on by features, `dep/feature` turning on a dependency's feature) and emits
//! `rargo_plan::Unit`s: a build script's compile and run, a library, and a member's binary.

use crate::rng::Rng;
use rargo_plan as rp;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    /// Another feature of the same crate.
    Feature(String),
    /// An optional dependency, by its index in the crate's `deps`.
    Dep(usize),
    /// A feature of a dependency (`dep/feature`), which also switches the dependency on.
    DepFeature(usize, String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepSpec {
    pub krate: usize,
    pub optional: bool,
    pub default_features: bool,
    pub features: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Crate {
    pub name: String,
    pub member: bool,
    pub files: Vec<usize>,
    pub build_script: Option<usize>,
    pub bin: Option<usize>,
    pub deps: Vec<DepSpec>,
    pub features: BTreeMap<String, Vec<Item>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Workspace {
    pub crates: Vec<Crate>,
    pub sources: usize,
}

impl Workspace {
    pub fn generate(rng: &mut Rng) -> Workspace {
        let count = rng.range(1, 7) as usize;
        let mut sources = 0;
        let file = |sources: &mut usize| {
            *sources += 1;
            *sources - 1
        };
        let mut crates: Vec<Crate> = Vec::new();
        for i in 0..count {
            let files = (0..rng.range(1, 3)).map(|_| file(&mut sources)).collect();
            let build_script = rng.chance(250).then(|| file(&mut sources));
            let member = i + 1 == count || rng.chance(300);
            let bin = (member && rng.chance(400)).then(|| file(&mut sources));
            let mut deps = Vec::new();
            for j in 0..i {
                if rng.chance(450) {
                    let named: Vec<String> = crates[j].features.keys().filter(|f| *f != "default").cloned().collect();
                    let features = named.into_iter().filter(|_| rng.chance(300)).collect();
                    deps.push(DepSpec { krate: j, optional: rng.chance(300), default_features: rng.chance(800), features });
                }
            }
            let mut features: BTreeMap<String, Vec<Item>> = BTreeMap::new();
            for (d, dep) in deps.iter().enumerate() {
                if dep.optional {
                    features.insert(crates[dep.krate].name.clone(), vec![Item::Dep(d)]);
                }
            }
            for k in 0..rng.range(0, 3) {
                let mut items = Vec::new();
                for _ in 0..rng.range(0, 2) {
                    let own: Vec<String> = features.keys().cloned().collect();
                    match rng.below(3) {
                        0 if !own.is_empty() => items.push(Item::Feature(own[rng.below(own.len() as u64) as usize].clone())),
                        1 if !deps.is_empty() => {
                            let d = rng.below(deps.len() as u64) as usize;
                            let theirs: Vec<&String> = crates[deps[d].krate].features.keys().filter(|f| *f != "default").collect();
                            if let Some(f) = theirs.get(rng.below(theirs.len().max(1) as u64) as usize) {
                                items.push(Item::DepFeature(d, (*f).clone()));
                            } else if deps[d].optional {
                                items.push(Item::Dep(d));
                            }
                        }
                        _ => {}
                    }
                }
                features.insert(format!("f{k}"), items);
            }
            let default: Vec<Item> =
                features.keys().filter(|_| rng.chance(400)).map(|f| Item::Feature(f.clone())).collect();
            if !default.is_empty() {
                features.insert("default".to_string(), default);
            }
            crates.push(Crate { name: format!("c{i}"), member, files, build_script, bin, deps, features });
        }
        let ws = Workspace { crates, sources };
        ws.check();
        ws
    }

    pub fn check(&self) {
        assert!(self.crates.iter().any(|c| c.member), "a workspace has a member");
        let mut seen = BTreeSet::new();
        for (i, c) in self.crates.iter().enumerate() {
            for d in &c.deps {
                assert!(d.krate < i, "dependencies come first, so the graph has no cycle");
                for f in &d.features {
                    assert!(self.crates[d.krate].features.contains_key(f), "{} asks {} for no such feature {f}", c.name, d.krate);
                }
            }
            for items in c.features.values() {
                for item in items {
                    match item {
                        Item::Feature(f) => assert!(c.features.contains_key(f)),
                        Item::Dep(d) => assert!(c.deps[*d].optional, "only an optional dependency is switched on"),
                        Item::DepFeature(d, f) => assert!(self.crates[c.deps[*d].krate].features.contains_key(f)),
                    }
                }
            }
            for f in c.files.iter().chain(&c.build_script).chain(&c.bin) {
                assert!(*f < self.sources && seen.insert(*f), "each source file belongs to one crate");
            }
        }
    }
}

/// What the feature resolver decided: which crates are built, with which features and dependencies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolve {
    pub features: BTreeMap<usize, BTreeSet<String>>,
    pub deps: BTreeMap<usize, BTreeSet<usize>>,
}

impl Resolve {
    pub fn of(ws: &Workspace) -> Resolve {
        let mut r = Resolve::default();
        for (i, c) in ws.crates.iter().enumerate() {
            if c.member {
                r.activate(ws, i, true);
            }
        }
        for (c, deps) in &r.deps {
            for d in deps {
                assert!(r.features.contains_key(d), "crate {c} depends on {d}, which is built");
            }
        }
        r
    }

    fn activate(&mut self, ws: &Workspace, c: usize, default: bool) {
        if !self.features.contains_key(&c) {
            self.features.insert(c, BTreeSet::new());
            self.deps.insert(c, BTreeSet::new());
            for d in 0..ws.crates[c].deps.len() {
                if !ws.crates[c].deps[d].optional {
                    self.dep(ws, c, d);
                }
            }
        }
        if default && ws.crates[c].features.contains_key("default") {
            self.enable(ws, c, "default");
        }
    }

    fn dep(&mut self, ws: &Workspace, c: usize, d: usize) {
        let spec = &ws.crates[c].deps[d];
        self.deps.get_mut(&c).expect("active").insert(spec.krate);
        self.activate(ws, spec.krate, spec.default_features);
        for f in &spec.features {
            self.enable(ws, spec.krate, f);
        }
    }

    fn enable(&mut self, ws: &Workspace, c: usize, f: &str) {
        assert!(self.features.contains_key(&c), "a feature is enabled on a built crate");
        if !self.features.get_mut(&c).expect("active").insert(f.to_string()) {
            return;
        }
        for item in &ws.crates[c].features[f] {
            match item {
                Item::Feature(g) => self.enable(ws, c, g),
                Item::Dep(d) => self.dep(ws, c, *d),
                Item::DepFeature(d, g) => {
                    self.dep(ws, c, *d);
                    self.enable(ws, ws.crates[c].deps[*d].krate, g);
                }
            }
        }
    }
}

/// The unit graph of a workspace, and which source files each unit reads itself.
pub struct Planned {
    pub plan: rp::Plan,
    pub sources: Vec<Vec<usize>>,
}

fn unit(kind: rp::UnitKind, c: &Crate, desc: &str, features: &BTreeSet<String>) -> rp::Unit {
    rp::Unit {
        kind,
        cmd: None,
        deps: Vec::new(),
        outputs: Vec::new(),
        rmeta_early: false,
        needs_objects: false,
        pkg: c.name.clone(),
        pkg_id: format!("{} 0.1.0", c.name),
        pkg_name: c.name.clone(),
        pkg_version: "0.1.0".to_string(),
        desc: format!("{} {desc}", c.name),
        verb: rp::Verb::Compiling,
        is_local: c.member,
        repository: None,
        show_warnings: c.member,
        fix_hint: None,
        pkg_spec: c.name.clone(),
        manifest_path: format!("/ws/{}/Cargo.toml", c.name),
        target_json: String::new(),
        profile_json: String::new(),
        features: features.iter().cloned().collect(),
        executable: None,
        script: None,
        script_metas: Vec::new(),
        scrape_can_fail: false,
    }
}

fn out(path: String, flavor: &str) -> rp::OutFile {
    rp::OutFile { path, hardlink: None, export_path: None, flavor: flavor.to_string() }
}

pub fn plan(ws: &Workspace) -> Planned {
    let resolve = Resolve::of(ws);
    let mut units: Vec<rp::Unit> = Vec::new();
    let mut sources: Vec<Vec<usize>> = Vec::new();
    let mut lib_of: BTreeMap<usize, u32> = BTreeMap::new();
    let mut roots = Vec::new();
    for (&i, features) in &resolve.features {
        let c = &ws.crates[i];
        let dep = |unit: u32, only_rmeta| rp::Dep { unit, only_rmeta };
        let mut lib = unit(rp::UnitKind::Compile, c, "lib", features);
        if let Some(file) = c.build_script {
            let mut build = unit(rp::UnitKind::Compile, c, "build script", features);
            build.needs_objects = true;
            build.outputs.push(out(format!("/t/build/{}/build-script-build", c.name), "Normal"));
            units.push(build);
            sources.push(vec![file]);
            let mut run = unit(rp::UnitKind::RunScript, c, "build script run", features);
            run.deps.push(dep(units.len() as u32 - 1, false));
            units.push(run);
            sources.push(Vec::new());
            lib.deps.push(dep(units.len() as u32 - 1, false));
        }
        for d in &resolve.deps[&i] {
            lib.deps.push(dep(lib_of[d], true));
        }
        lib.rmeta_early = true;
        lib.outputs.push(out(format!("/t/deps/lib{}.rmeta", c.name), "Rmeta"));
        lib.outputs.push(out(format!("/t/deps/lib{}.rlib", c.name), "Normal"));
        let lib_deps = lib.deps.clone();
        units.push(lib);
        sources.push(c.files.clone());
        let at = units.len() as u32 - 1;
        lib_of.insert(i, at);
        if c.member {
            roots.push(at);
        }
        if let (Some(file), true) = (c.bin, c.member) {
            let mut bin = unit(rp::UnitKind::Compile, c, "bin", features);
            bin.needs_objects = true;
            bin.deps.push(dep(at, false));
            bin.deps.extend(lib_deps.into_iter().filter(|d| d.only_rmeta));
            bin.outputs.push(out(format!("/t/{}", c.name), "Normal"));
            units.push(bin);
            sources.push(vec![file]);
            roots.push(units.len() as u32 - 1);
        }
    }
    assert_eq!(units.len(), sources.len());
    let plan = rp::Plan {
        version: rp::PLAN_VERSION,
        ws_root: "/ws".to_string(),
        cwd: "/ws".to_string(),
        target_dir: "/t".to_string(),
        build_dir: "/t".to_string(),
        host: "x86_64-unknown-linux-gnu".to_string(),
        toolchain: None,
        rustc_fork: false,
        dep_info_basedir: None,
        units,
        roots,
        steps: Vec::new(),
        finished: String::new(),
        message_format: rp::MessageFormat::Human,
        verbosity: rp::Verbosity::Normal,
        extra_verbose: false,
        keep_going: false,
        future_incompat_report: false,
        future_incompat_display: false,
        timings: false,
        deny_warnings: false,
        no_fail_fast: false,
        nocapture: false,
        dylib_var: "LD_LIBRARY_PATH".to_string(),
        has_build: ws.crates.iter().any(|c| c.build_script.is_some()),
    };
    Planned { plan, sources }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(krate: usize, optional: bool, features: &[&str]) -> DepSpec {
        DepSpec { krate, optional, default_features: true, features: features.iter().map(|f| f.to_string()).collect() }
    }

    fn krate(name: &str, member: bool, files: Vec<usize>, deps: Vec<DepSpec>, features: &[(&str, Vec<Item>)]) -> Crate {
        let features = features.iter().map(|(f, items)| (f.to_string(), items.clone())).collect();
        Crate { name: name.to_string(), member, files, build_script: None, bin: None, deps, features }
    }

    fn show(p: &Planned) -> String {
        let mut text = String::new();
        for (i, u) in p.plan.units.iter().enumerate() {
            let deps: Vec<String> = u.deps.iter().map(|d| format!("{}{}", d.unit, if d.only_rmeta { "m" } else { "" })).collect();
            text += &format!("{i} {} [{}] deps [{}] reads {:?}\n", u.desc, u.features.join(","), deps.join(" "), p.sources[i]);
        }
        text + &format!("roots {:?}\n", p.plan.roots)
    }

    fn example() -> Workspace {
        let mut script = krate("sys", false, vec![0], vec![], &[("fast", vec![]), ("default", vec![Item::Feature("fast".into())])]);
        script.build_script = Some(1);
        let unused = krate("unused", false, vec![2], vec![], &[]);
        let mut app = krate(
            "app",
            true,
            vec![3, 4],
            vec![DepSpec { default_features: false, ..dep(0, false, &[]) }, dep(1, true, &[])],
            &[("unused", vec![Item::Dep(1)])],
        );
        app.bin = Some(5);
        let tool = krate("tool", true, vec![6], vec![dep(0, false, &[]), dep(2, false, &[])], &[]);
        Workspace { crates: vec![script, unused, app, tool], sources: 7 }
    }

    #[test]
    fn a_workspace_plans_to_units() {
        let ws = example();
        ws.check();
        assert_eq!(
            show(&plan(&ws)),
            "\
0 sys build script [default,fast] deps [] reads [1]
1 sys build script run [default,fast] deps [0] reads []
2 sys lib [default,fast] deps [1] reads [0]
3 app lib [] deps [2m] reads [3, 4]
4 app bin [] deps [3 2m] reads [5]
5 tool lib [] deps [2m 3m] reads [6]
roots [3, 4, 5]
"
        );
    }

    #[test]
    fn a_feature_switches_on_an_optional_dependency_and_its_features() {
        let mut ws = example();
        ws.crates[3].features.insert("all".into(), vec![Item::DepFeature(1, "unused".into())]);
        ws.crates[3].features.insert("default".into(), vec![Item::Feature("all".into())]);
        let r = Resolve::of(&ws);
        assert_eq!(r.deps[&2], BTreeSet::from([0, 1]), "app's feature `unused` turns its optional dependency on");
        assert!(r.features[&1].is_empty());
        assert_eq!(r.features[&2], BTreeSet::from(["unused".to_string()]));
        assert_eq!(show(&plan(&ws)).lines().filter(|l| l.contains("unused lib")).count(), 1);
    }

    #[test]
    fn the_pure_plan_code_decides_what_each_action_reads() {
        let mut rng = Rng::new(1);
        let project = crate::model::Project::plan(&mut rng, &example());
        let deps: Vec<&[usize]> = project.actions.iter().map(|a| a.deps.as_slice()).collect();
        // A library sees its dependencies' metadata and the build script run whole, which sees the
        // script's binary; a binary links everything below it.
        assert_eq!(deps, vec![&[][..], &[0], &[0, 1], &[0, 1, 2], &[0, 1, 2, 3], &[0, 1, 2, 3]]);
    }

    #[test]
    fn seeds_vary_the_workspace_shape() {
        let shapes: Vec<Workspace> = (0..200).map(|s| Workspace::generate(&mut Rng::new(s))).collect();
        let crates: BTreeSet<usize> = shapes.iter().map(|w| w.crates.len()).collect();
        assert!(crates.len() >= 5, "crate counts {crates:?}");
        let all = || shapes.iter().flat_map(|w| &w.crates);
        assert!(all().any(|c| c.build_script.is_some()), "build scripts");
        assert!(all().any(|c| c.bin.is_some()), "binaries");
        assert!(all().any(|c| c.deps.iter().any(|d| d.optional)), "optional dependencies");
        assert!(all().any(|c| c.deps.iter().any(|d| !d.default_features)), "dependencies without default features");
        assert!(all().any(|c| c.features.values().flatten().any(|i| matches!(i, Item::DepFeature(..)))), "dep/feature");
        assert!(all().any(|c| c.features.contains_key("default")), "default features");
        assert!(shapes.iter().any(|w| w.crates.iter().filter(|c| c.member).count() > 1), "several members");
        let unbuilt = shapes.iter().any(|w| Resolve::of(w).features.len() < w.crates.len());
        assert!(unbuilt, "some crate is left out of the build");
        let units: BTreeSet<usize> = shapes.iter().map(|w| plan(w).plan.units.len()).collect();
        assert!(units.len() >= 8, "unit counts {units:?}");
        for (s, w) in shapes.iter().enumerate() {
            assert_eq!(*w, Workspace::generate(&mut Rng::new(s as u64)), "a seed gives one workspace");
        }
    }
}
