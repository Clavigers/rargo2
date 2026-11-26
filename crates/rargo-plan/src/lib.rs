use serde::{Deserialize, Serialize};

pub const PLAN_VERSION: u32 = 6;

pub const UNUSED_ENV: &str = "__RARGO_UNUSED";

pub fn split_env(name: &str, value: &str) -> Vec<(String, String)> {
    const PART: usize = 96 << 10;
    const _: () = assert!(PART >= 4, "a part holds at least one UTF-8 character");
    const _: () = assert!(PART < 128 << 10, "a part stays under the kernel's limit");
    let mut out = Vec::new();
    let mut rest = value;
    loop {
        let mut cut = rest.len().min(PART);
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        assert!(cut > 0 || rest.is_empty(), "every part but an empty value's makes progress");
        assert!(cut <= PART);
        let key = if out.is_empty() { name.to_string() } else { format!("{name}_{}", out.len()) };
        out.push((key, rest[..cut].to_string()));
        rest = &rest[cut..];
        if rest.is_empty() {
            assert_eq!(out.iter().map(|(_, v)| v.len()).sum::<usize>(), value.len());
            assert_eq!(out[0].0, name);
            return out;
        }
    }
}

/// Joins the value `split_env` spread over `name`, `name_1`, `name_2`, ... back together. `var` reads one variable of the caller's environment.
pub fn joined_env(name: &str, var: impl Fn(&str) -> Option<String>) -> String {
    let mut value = var(name).unwrap_or_default();
    for i in 1.. {
        match var(&format!("{name}_{i}")) {
            Some(part) => value.push_str(&part),
            None => break,
        }
    }
    value
}

pub const PLAN_ENV: &str = "__RARGO_PLAN";

pub const PARSE_ENV: &str = "__RARGO_PARSE";

pub mod token {
    pub const PREFIX: &str = "__RARGO_S_";
    pub const SUFFIX: &str = "__";
    pub const CFG: &str = "CFG";
    pub const CHECK_CFG: &str = "CHECKCFG";
    pub const ENV: &str = "ENV";
    pub const LINK_LIB: &str = "LL";
    pub const LIBPATH_ARTIFACT: &str = "LPA";
    pub const LIBPATH_EXTERNAL: &str = "LPE";
    pub const LINK_ARG: &str = "LA";
    pub const METADATA: &str = "MD";
    pub const DYN_LIBPATHS: &str = "DYNLP";
    pub const NATIVE_DIRS: &str = "__RARGO_S_NATIVE_DIRS__";

    pub fn make(kind: &str, meta: &str) -> String {
        format!("{PREFIX}{kind}_{meta}{SUFFIX}")
    }

    pub fn link_arg(kind: &str, meta: &str) -> String {
        format!("{PREFIX}{LINK_ARG}_{kind}_{meta}{SUFFIX}")
    }

    pub fn link_arg_bin(meta: &str, bin: &str) -> String {
        format!("{PREFIX}{LINK_ARG}_SB_{meta}{SUFFIX}{bin}")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, Option<String>)>,
    pub cwd: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UnitKind {
    Compile,
    Doc,
    RunScript,
    Noop,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Verb {
    Compiling,
    Checking,
    Documenting,
    Scraping,
    Silent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dep {
    pub unit: u32,
    pub only_rmeta: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutFile {
    pub path: String,
    pub hardlink: Option<String>,
    pub export_path: Option<String>,
    pub flavor: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TargetBrief {
    pub name: String,
    pub is_bin: bool,
    pub is_cdylib: bool,
    pub is_test: bool,
    pub is_bench: bool,
    pub is_exe_example: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Script {
    pub meta: String,
    pub out_dir: String,
    pub run_dir: String,
    pub library_name: Option<String>,
    pub pkg_descr: String,
    pub nightly_features_allowed: bool,
    pub targets: Vec<TargetBrief>,
    pub msrv: Option<String>,
    pub links: Option<String>,
    pub vv_prefix: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unit {
    pub kind: UnitKind,
    pub cmd: Option<Cmd>,
    pub deps: Vec<Dep>,
    pub outputs: Vec<OutFile>,
    pub rmeta_early: bool,
    pub needs_objects: bool,

    pub pkg: String,
    pub pkg_id: String,
    pub pkg_name: String,
    pub pkg_version: String,
    pub desc: String,
    pub verb: Verb,
    pub is_local: bool,
    pub repository: Option<String>,
    pub show_warnings: bool,
    pub fix_hint: Option<String>,

    pub pkg_spec: String,
    pub manifest_path: String,
    pub target_json: String,
    pub profile_json: String,
    pub features: Vec<String>,
    pub executable: Option<String>,

    pub script: Option<Script>,
    pub script_metas: Vec<String>,
    pub scrape_can_fail: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum When {
    Always,
    Concise,
    Verbose,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExecKind {
    Test,
    Bench,
    Doctest,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Step {
    Status { verb: String, msg: String, when: When },
    Exec {
        kind: ExecKind,
        unit: u32,
        cmd: Cmd,
        rerun: String,
        harness: bool,
    },
    Run { cmd: Cmd, display: String },
    Open { cmd: Cmd },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageFormat {
    Human,
    Short,
    Json { render_diagnostics: bool, short: bool, ansi: bool },
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Verbosity {
    Verbose,
    Normal,
    Quiet,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub version: u32,
    pub ws_root: String,
    pub cwd: String,
    pub target_dir: String,
    pub build_dir: String,
    pub host: String,
    pub toolchain: Option<String>,
    pub rustc_fork: bool,
    pub dep_info_basedir: Option<String>,
    pub units: Vec<Unit>,
    pub roots: Vec<u32>,
    pub steps: Vec<Step>,
    pub finished: String,
    pub message_format: MessageFormat,
    pub verbosity: Verbosity,
    pub extra_verbose: bool,
    pub keep_going: bool,
    pub future_incompat_report: bool,
    pub future_incompat_display: bool,
    pub timings: bool,
    pub deny_warnings: bool,
    pub no_fail_fast: bool,
    pub nocapture: bool,
    pub dylib_var: String,
    pub has_build: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParseReq {
    pub script: Script,
    pub stdout: Vec<u8>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub enum LinkArgKind {
    All,
    Cdylib,
    Bin,
    Test,
    Bench,
    Example,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum LinkArgTarget {
    Kind(LinkArgKind),
    SingleBin(String),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildOutput {
    pub library_paths: Vec<(bool, String)>,
    pub library_links: Vec<String>,
    pub linker_args: Vec<(LinkArgTarget, String)>,
    pub cfgs: Vec<String>,
    pub check_cfgs: Vec<String>,
    pub env: Vec<(String, String)>,
    pub metadata: Vec<(String, String)>,
    pub rerun_if_changed: Vec<String>,
    pub rerun_if_env_changed: Vec<String>,
    pub log_messages: Vec<(bool, String)>,
}

pub type ParseResp = Result<BuildOutput, String>;

pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    bincode::serialize(v).expect("plan types serialise")
}

pub fn decode<T: for<'a> Deserialize<'a>>(b: &[u8]) -> Result<T, String> {
    bincode::deserialize(b).map_err(|e| e.to_string())
}

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn long_values_split_and_join() {
        let value: String = (0..300_000).map(|i| if i % 7 == 0 { 'é' } else { 'x' }).collect();
        let parts = split_env("__RARGO_TEST_SPLIT", &value);
        assert!(parts.len() > 2 && parts.iter().all(|(_, v)| v.len() <= 96 << 10));
        assert_eq!(parts[0].0, "__RARGO_TEST_SPLIT");
        assert_eq!(parts[1].0, "__RARGO_TEST_SPLIT_1");
        let var = |name: &str| parts.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        assert_eq!(joined_env("__RARGO_TEST_SPLIT", var), value);
        assert_eq!(split_env("X", ""), vec![("X".to_string(), String::new())]);
        assert_eq!(joined_env("__RARGO_TEST_ABSENT", var), "");
    }
}
