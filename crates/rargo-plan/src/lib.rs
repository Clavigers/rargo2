//! The plan the cargo fork hands to rargod.
//!
//! The fork runs cargo's real pipeline with process execution stubbed out.
//! Every process cargo would have started is recorded here exactly as cargo
//! built it. Values cargo only learns by running build scripts appear as
//! placeholder tokens (see [`token`]); rargod substitutes the real values
//! once the scripts have run.
//!
//! This crate is deliberately tiny and edition-2021 so the fork can depend
//! on it by path without pulling in anything else.

use serde::{Deserialize, Serialize};

/// Bumped whenever the format or the fork's behaviour changes; part of the
/// planner's cache key through the binary itself, checked on load.
pub const PLAN_VERSION: u32 = 3;

/// Environment variable naming the file the plan is written to. Its presence
/// switches the fork into planning mode.
pub const PLAN_ENV: &str = "__RARGO_PLAN";

/// Environment variable that turns the fork into a build-script-output
/// parsing service on stdin/stdout.
pub const PARSE_ENV: &str = "__RARGO_PARSE";

/// Placeholder tokens. All are of the form `__RARGO_S_<KIND>_<meta>__` where
/// `<meta>` is the hex metadata hash of the build-script run they stand for.
/// Tokens survive cargo's `envify` (upper-casing), so matching is
/// case-insensitive on the hex part.
pub mod token {
    pub const PREFIX: &str = "__RARGO_S_";
    pub const SUFFIX: &str = "__";
    /// `--cfg <token>`: one `--cfg` per cfg the script printed.
    pub const CFG: &str = "CFG";
    /// `--check-cfg <token>`.
    pub const CHECK_CFG: &str = "CHECKCFG";
    /// Environment variable named `<token>`: every `rustc-env` pair.
    pub const ENV: &str = "ENV";
    /// `-l <token>`: one `-l` per `rustc-link-lib`.
    pub const LINK_LIB: &str = "LL";
    /// `-L <out_dir>/<token>`: link-search paths inside the script's OUT_DIR.
    pub const LIBPATH_ARTIFACT: &str = "LPA";
    /// `-L <token>`: link-search paths elsewhere.
    pub const LIBPATH_EXTERNAL: &str = "LPE";
    /// `-C link-arg=<token>`: link args of one target kind. A run of
    /// consecutive link-arg tokens for one script expands to that script's
    /// link args of those kinds in the order the script printed them.
    pub const LINK_ARG: &str = "LA";
    /// Metadata key `<token>`: becomes `DEP_<LINKS>_<KEY>` variables.
    pub const METADATA: &str = "MD";
    /// Entry `<token>@<root>` in a dynamic library search path: every
    /// link-search path of the script, in order, that lies under `<root>`.
    pub const DYN_LIBPATHS: &str = "DYNLP";
    /// The set of all link-search paths of all scripts, sorted. As a `-L`
    /// argument it expands to one `-L` per path; as `<token>@<root>` in a
    /// search path variable it expands to the paths under `<root>`.
    pub const NATIVE_DIRS: &str = "__RARGO_S_NATIVE_DIRS__";

    pub fn make(kind: &str, meta: &str) -> String {
        format!("{PREFIX}{kind}_{meta}{SUFFIX}")
    }

    /// Link-arg token for a target kind (`ALL`, `CDYLIB`, `BIN`, `TEST`,
    /// `BENCH`, `EXAMPLE`).
    pub fn link_arg(kind: &str, meta: &str) -> String {
        format!("{PREFIX}{LINK_ARG}_{kind}_{meta}{SUFFIX}")
    }

    /// Link-arg token for one named binary.
    pub fn link_arg_bin(meta: &str, bin: &str) -> String {
        format!("{PREFIX}{LINK_ARG}_SB_{meta}{SUFFIX}{bin}")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    /// Variables cargo sets (`Some`) or removes (`None`) relative to its own
    /// environment.
    pub env: Vec<(String, Option<String>)>,
    pub cwd: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum UnitKind {
    /// A rustc invocation.
    Compile,
    /// A rustdoc invocation (`cargo doc`).
    Doc,
    /// Running a build script.
    RunScript,
    /// A unit with nothing to execute (doctest units, overridden scripts).
    Noop,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Verb {
    Compiling,
    Checking,
    Documenting,
    Scraping,
    /// Nothing is printed for this unit.
    Silent,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dep {
    pub unit: u32,
    /// The dependent only needs the dependency's `.rmeta`.
    pub only_rmeta: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutFile {
    pub path: String,
    /// Where cargo links the file for the user (`target/debug/foo`).
    pub hardlink: Option<String>,
    /// `--artifact-dir` copy.
    pub export_path: Option<String>,
    /// Cargo's `FileFlavor`, as debug text (`Normal`, `Rmeta`, `DebugInfo`...).
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

/// What rargod needs to know about a build-script run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Script {
    /// Hex metadata hash; identifies the run in placeholder tokens.
    pub meta: String,
    pub out_dir: String,
    /// Directory where cargo keeps the script's `output`, `stderr` and
    /// `root-output` files.
    pub run_dir: String,
    pub library_name: Option<String>,
    /// Package description used in cargo's error messages.
    pub pkg_descr: String,
    pub nightly_features_allowed: bool,
    pub targets: Vec<TargetBrief>,
    pub msrv: Option<String>,
    pub links: Option<String>,
    /// `[name version] ` prefix cargo puts before script output with `-vv`.
    pub vv_prefix: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Unit {
    pub kind: UnitKind,
    pub cmd: Option<Cmd>,
    pub deps: Vec<Dep>,
    pub outputs: Vec<OutFile>,
    /// The unit announces its `.rmeta` before it finishes and dependents may
    /// start on it.
    pub rmeta_early: bool,
    /// The unit needs the object code of everything it depends on (it is
    /// linked: a binary, test, proc macro, dylib or build script).
    pub needs_objects: bool,

    // Presentation.
    /// `foo v0.1.0 (/w/crates/foo)`, as cargo prints after `Compiling`.
    pub pkg: String,
    /// Unique per package; used for once-per-package status lines.
    pub pkg_id: String,
    pub pkg_name: String,
    pub pkg_version: String,
    /// `` `foo` (lib) ``, as cargo names the unit in summaries.
    pub desc: String,
    pub verb: Verb,
    pub is_local: bool,
    pub show_warnings: bool,
    /// Arguments for the `cargo fix` hint (`--lib -p foo`), when cargo would
    /// print one.
    pub fix_hint: Option<String>,

    // JSON messages.
    pub pkg_spec: String,
    pub manifest_path: String,
    /// The `target` object of cargo's JSON messages, already serialised.
    pub target_json: String,
    /// The `profile` object of the `compiler-artifact` message.
    pub profile_json: String,
    pub features: Vec<String>,
    pub executable: Option<String>,

    pub script: Option<Script>,
    /// Build-script runs of this unit's own package (hex metas), for showing
    /// their warnings when the unit fails.
    pub script_metas: Vec<String>,
    /// Failure of this unit only disables example scraping.
    pub scrape_can_fail: bool,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum When {
    Always,
    /// Not with `-v`.
    Concise,
    /// Only with `-v`.
    Verbose,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExecKind {
    Test,
    Bench,
    Doctest,
}

/// What happens after the build, in order.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Step {
    Status { verb: String, msg: String, when: When },
    /// Run a test binary or `rustdoc --test`.
    Exec {
        kind: ExecKind,
        unit: u32,
        cmd: Cmd,
        /// Arguments cargo suggests for rerunning (`-p foo --lib`).
        rerun: String,
        harness: bool,
    },
    /// `cargo run`: the client replaces itself with this process.
    Run { cmd: Cmd, display: String },
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
    /// The rustup toolchain the plan was made with, if rustup is in use.
    pub toolchain: Option<String>,
    /// The compiler is rargo's rustc fork (its sysroot carries the marker
    /// file `lib/rustlib/rargo-fork`): metadata-only compiles are available
    /// and crate metadata is identified by its crate hash.
    pub rustc_fork: bool,
    pub units: Vec<Unit>,
    pub roots: Vec<u32>,
    pub steps: Vec<Step>,
    /// `` `dev` profile [unoptimized + debuginfo] target(s) in `` (the
    /// elapsed time is appended).
    pub finished: String,
    pub message_format: MessageFormat,
    pub verbosity: Verbosity,
    pub extra_verbose: bool,
    pub keep_going: bool,
    pub deny_warnings: bool,
    pub no_fail_fast: bool,
    /// Test arguments contain `--nocapture`/`--no-capture`.
    pub nocapture: bool,
    /// Name of the variable holding the dynamic library search path.
    pub dylib_var: String,
    /// The compile step was requested at all (false for `cargo test
    /// --no-run`-like steps is still true; false only when cargo decided
    /// there is nothing to build).
    pub has_build: bool,
}

/// A request to the parsing service: the stdout of one build-script run.
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

/// A build script's parsed output, as cargo's own parser produced it.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BuildOutput {
    /// `(inside OUT_DIR, path)`.
    pub library_paths: Vec<(bool, String)>,
    pub library_links: Vec<String>,
    pub linker_args: Vec<(LinkArgTarget, String)>,
    pub cfgs: Vec<String>,
    pub check_cfgs: Vec<String>,
    pub env: Vec<(String, String)>,
    pub metadata: Vec<(String, String)>,
    pub rerun_if_changed: Vec<String>,
    pub rerun_if_env_changed: Vec<String>,
    /// `(is_error, message)`.
    pub log_messages: Vec<(bool, String)>,
}

pub type ParseResp = Result<BuildOutput, String>;

pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    bincode::serialize(v).expect("plan types serialise")
}

pub fn decode<T: for<'a> Deserialize<'a>>(b: &[u8]) -> Result<T, String> {
    bincode::deserialize(b).map_err(|e| e.to_string())
}
