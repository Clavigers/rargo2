//! Substituting real build-script results for the placeholder tokens in a
//! planned command (see `rargo_plan::token`).
//!
//! The planner ran cargo's own code against token-valued build-script
//! output, so each token sits exactly where cargo would have put the real
//! values. Expansion therefore only has to know, per token kind, how one
//! placeholder turns into zero or more real arguments or variables.

use anyhow::{Result, bail};
use rargo_plan::{BuildOutput, Cmd, LinkArgKind, LinkArgTarget, token};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type Scripts = HashMap<String, Arc<BuildOutput>>;

#[derive(Debug, PartialEq, Eq)]
enum Tok<'a> {
    Cfg(&'a str),
    CheckCfg(&'a str),
    Env(&'a str),
    LinkLib(&'a str),
    LibArtifact(&'a str),
    LibExternal(&'a str),
    LinkArg(&'a str, LinkArgSel),
    Metadata(&'a str),
    DynLibPaths(&'a str),
    NativeDirs,
}

#[derive(Debug, PartialEq, Eq, Clone)]
enum LinkArgSel {
    Kind(LinkArgKind),
    Bin(String),
}

const META_LEN: usize = 16;

/// Finds the first token in `s`. Returns its byte range and meaning.
fn find_token(s: &str) -> Option<(std::ops::Range<usize>, Tok<'_>)> {
    if let Some(pos) = s.find(token::NATIVE_DIRS) {
        return Some((pos..pos + token::NATIVE_DIRS.len(), Tok::NativeDirs));
    }
    let pos = s.find(token::PREFIX)?;
    let rest = &s[pos + token::PREFIX.len()..];
    let meta_at = |skip: usize| -> Option<(&str, usize)> {
        let m = rest.get(skip..skip + META_LEN)?;
        if !m.bytes().all(|b| b.is_ascii_hexdigit()) || !rest[skip + META_LEN..].starts_with(token::SUFFIX) {
            return None;
        }
        assert_eq!(m.len(), META_LEN);
        Some((m, pos + token::PREFIX.len() + skip + META_LEN + token::SUFFIX.len()))
    };
    const SIMPLE: [&str; 8] = ["CHECKCFG_", "CFG_", "ENV_", "LL_", "LPA_", "LPE_", "MD_", "DYNLP_"];
    for (n, tag) in SIMPLE.iter().enumerate() {
        if rest.starts_with(tag) {
            let (m, end) = meta_at(tag.len())?;
            let tok = match n {
                0 => Tok::CheckCfg(m),
                1 => Tok::Cfg(m),
                2 => Tok::Env(m),
                3 => Tok::LinkLib(m),
                4 => Tok::LibArtifact(m),
                5 => Tok::LibExternal(m),
                6 => Tok::Metadata(m),
                _ => Tok::DynLibPaths(m),
            };
            assert!(end <= s.len());
            assert!(pos < end);
            return Some((pos..end, tok));
        }
    }
    if let Some(after) = rest.strip_prefix("LA_") {
        if after.starts_with("SB_") {
            let (m, end) = meta_at("LA_SB_".len())?;
            // The binary name follows the token to the end of the string.
            return Some((pos..s.len(), Tok::LinkArg(m, LinkArgSel::Bin(s[end..].to_string()))));
        }
        for (tag, kind) in [
            ("ALL_", LinkArgKind::All),
            ("CDYLIB_", LinkArgKind::Cdylib),
            ("BIN_", LinkArgKind::Bin),
            ("TEST_", LinkArgKind::Test),
            ("BENCH_", LinkArgKind::Bench),
            ("EXAMPLE_", LinkArgKind::Example),
        ] {
            if after.starts_with(tag) {
                let (m, end) = meta_at("LA_".len() + tag.len())?;
                return Some((pos..end, Tok::LinkArg(m, LinkArgSel::Kind(kind))));
            }
        }
    }
    None
}

fn script<'a>(scripts: &'a Scripts, meta: &str) -> Result<&'a BuildOutput> {
    // Metadata tokens pass through cargo's `envify`, which upper-cases.
    let lower = meta.to_ascii_lowercase();
    match scripts.get(&lower) {
        Some(o) => Ok(o),
        None => bail!("internal error: no build script result for {lower}"),
    }
}

/// cargo's `envify`.
pub fn envify(s: &str) -> String {
    s.chars().flat_map(|c| c.to_uppercase()).map(|c| if c == '-' { '_' } else { c }).collect()
}

/// cargo's `get_dynamic_search_path`: strips a `KIND=` prefix.
fn dyn_path(p: &str) -> &str {
    match p.split_once('=') {
        Some(("native" | "crate" | "dependency" | "framework" | "all", rest)) => rest,
        _ => p,
    }
}

/// Every link-search path of every script, in cargo's `BTreeSet<PathBuf>`
/// order.
fn native_dirs(scripts: &Scripts) -> Vec<String> {
    let set: BTreeSet<PathBuf> =
        scripts.values().flat_map(|o| o.library_paths.iter().map(|(_, p)| PathBuf::from(p))).collect();
    set.into_iter().map(|p| p.to_string_lossy().into_owned()).collect()
}

fn link_arg_matches(t: &LinkArgTarget, sels: &[LinkArgSel]) -> bool {
    sels.iter().any(|s| match (t, s) {
        (LinkArgTarget::Kind(a), LinkArgSel::Kind(b)) => a == b,
        (LinkArgTarget::SingleBin(a), LinkArgSel::Bin(b)) => a == b,
        _ => false,
    })
}

/// Whether script-provided environment may replace an existing variable.
/// In rustc invocations cargo applies script variables last; in processes it
/// runs for the user (tests, `cargo run`) cargo's own `CARGO_*` come after.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EnvRule {
    ScriptWins,
    CargoVarsWin,
}

/// Expands every token in `cmd`.
pub fn expand(cmd: &Cmd, scripts: &Scripts, dylib_var: &str, rule: EnvRule) -> Result<Cmd> {
    let mut args: Vec<String> = Vec::with_capacity(cmd.args.len());
    let mut i = 0;
    while i < cmd.args.len() {
        let arg = &cmd.args[i];
        let Some((range, tok)) = find_token(arg) else {
            args.push(arg.clone());
            i += 1;
            continue;
        };
        // Tokens that stand for a whole argument take their flag with them:
        // `--cfg TOKEN` becomes `--cfg a --cfg b` or nothing at all.
        let flagged = |values: Vec<String>, args: &mut Vec<String>| -> Result<()> {
            let Some(flag) = args.pop() else { bail!("internal error: token `{arg}` without a flag") };
            for v in values {
                args.push(flag.clone());
                args.push(v);
            }
            Ok(())
        };
        match tok {
            Tok::Cfg(m) => flagged(script(scripts, m)?.cfgs.clone(), &mut args)?,
            Tok::CheckCfg(m) => flagged(script(scripts, m)?.check_cfgs.clone(), &mut args)?,
            Tok::LinkLib(m) => flagged(script(scripts, m)?.library_links.clone(), &mut args)?,
            Tok::LibArtifact(m) => flagged(
                script(scripts, m)?.library_paths.iter().filter(|(a, _)| *a).map(|(_, p)| p.clone()).collect(),
                &mut args,
            )?,
            Tok::LibExternal(m) => flagged(
                script(scripts, m)?.library_paths.iter().filter(|(a, _)| !*a).map(|(_, p)| p.clone()).collect(),
                &mut args,
            )?,
            Tok::NativeDirs if range == (0..arg.len()) => flagged(native_dirs(scripts), &mut args)?,
            Tok::LinkArg(m, first) => {
                if !arg.starts_with("link-arg=") {
                    bail!("internal error: unexpected link-arg token position in `{arg}`");
                }
                // Gather the run of `-C link-arg=TOKEN` pairs for this script.
                assert!(range.start >= "link-arg=".len());
                let mut sels = vec![first];
                let mut j = i + 1;
                while j + 1 < cmd.args.len() && cmd.args[j] == "-C" {
                    match find_token(&cmd.args[j + 1]) {
                        Some((_, Tok::LinkArg(m2, sel))) if m2 == m && cmd.args[j + 1].starts_with("link-arg=") => {
                            sels.push(sel);
                            j += 2;
                        }
                        _ => break,
                    }
                }
                let values = script(scripts, m)?
                    .linker_args
                    .iter()
                    .filter(|(t, _)| link_arg_matches(t, &sels))
                    .map(|(_, a)| format!("link-arg={a}"))
                    .collect();
                assert!(j > i);
                assert_eq!((j - i - 1) % 2, 0, "link args come in `-C` pairs");
                assert_eq!(sels.len(), 1 + (j - i - 1) / 2);
                flagged(values, &mut args)?;
                i = j;
                continue;
            }
            other => bail!("internal error: token {other:?} cannot appear in argument `{arg}`"),
        }
        i += 1;
    }

    let mut env: Vec<(String, Option<String>)> = Vec::with_capacity(cmd.env.len());
    let mut script_env: Vec<(String, String)> = Vec::new();
    for (k, v) in &cmd.env {
        match find_token(k) {
            Some((_, Tok::Env(m))) => {
                script_env.extend(script(scripts, m)?.env.iter().cloned());
                continue;
            }
            Some((range, Tok::Metadata(m))) => {
                let prefix = &k[..range.start];
                for (mk, mv) in &script(scripts, m)?.metadata {
                    env.push((format!("{prefix}{}", envify(mk)), Some(mv.clone())));
                }
                continue;
            }
            Some((_, other)) => bail!("internal error: token {other:?} cannot appear in variable name `{k}`"),
            None => {}
        }
        let v = match v {
            Some(v) if k == dylib_var && v.contains(token::PREFIX) => Some(expand_search_path(v, scripts)?),
            other => other.clone(),
        };
        env.push((k.clone(), v));
    }
    for (k, v) in script_env {
        if let Some(existing) = env.iter_mut().find(|(n, _)| *n == k) {
            if rule == EnvRule::ScriptWins || !k.starts_with("CARGO") {
                existing.1 = Some(v);
            }
        } else {
            env.push((k, Some(v)));
        }
    }

    assert!(env.len() >= cmd.env.iter().filter(|(k, _)| find_token(k).is_none()).count());
    let out = Cmd { program: cmd.program.clone(), args, env, cwd: cmd.cwd.clone() };
    // Anything left over is a token form this code does not know; failing
    // here is far better than passing a placeholder to a compiler.
    let leftover = out.args.iter().chain(out.env.iter().map(|(k, _)| k)).any(|s| s.contains(token::PREFIX))
        || out.env.iter().any(|(_, v)| v.as_deref().is_some_and(|v| v.contains(token::PREFIX)))
        || out.program.contains(token::PREFIX);
    if leftover {
        bail!("internal error: unexpanded build-script placeholder in `{}`", out.program);
    }
    Ok(out)
}

fn expand_search_path(value: &str, scripts: &Scripts) -> Result<String> {
    let mut out: Vec<String> = Vec::new();
    for entry in value.split(':') {
        let Some((range, tok)) = find_token(entry) else {
            out.push(entry.to_string());
            continue;
        };
        let Some(root) = entry[range.end..].strip_prefix('@') else {
            bail!("internal error: search path token without a root in `{entry}`");
        };
        let root = Path::new(root);
        let keep = |p: &str| -> Option<String> {
            let p = dyn_path(p);
            Path::new(p).starts_with(root).then(|| p.to_string())
        };
        match tok {
            Tok::DynLibPaths(m) => {
                out.extend(script(scripts, m)?.library_paths.iter().filter_map(|(_, p)| keep(p)))
            }
            Tok::NativeDirs => out.extend(native_dirs(scripts).iter().filter_map(|p| keep(p))),
            other => bail!("internal error: token {other:?} cannot appear in a search path"),
        }
    }
    Ok(out.join(":"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const M1: &str = "00112233aabbccdd";
    const M2: &str = "ffeeddcc99887766";

    fn scripts() -> Scripts {
        let mut s = Scripts::new();
        s.insert(
            M1.into(),
            Arc::new(BuildOutput {
                library_paths: vec![
                    (false, "/usr/lib/ext".into()),
                    (true, "native=/w/target/debug/build/a/out".into()),
                    (false, "/w/target/debug/shared".into()),
                ],
                library_links: vec!["static=foo".into(), "z".into()],
                linker_args: vec![
                    (LinkArgTarget::Kind(LinkArgKind::All), "-Wl,--first".into()),
                    (LinkArgTarget::Kind(LinkArgKind::Bin), "-Wl,--bins".into()),
                    (LinkArgTarget::SingleBin("tool".into()), "-Wl,--tool".into()),
                    (LinkArgTarget::Kind(LinkArgKind::Test), "-Wl,--tests".into()),
                    (LinkArgTarget::Kind(LinkArgKind::All), "-Wl,--last".into()),
                ],
                cfgs: vec!["has_x".into(), "feature=\"y\"".into()],
                check_cfgs: vec!["cfg(has_x)".into()],
                env: vec![("GENERATED".into(), "1".into()), ("CARGO_PKG_NAME".into(), "evil".into())],
                metadata: vec![("include".into(), "/inc".into()), ("root-dir".into(), "/r".into())],
                ..Default::default()
            }),
        );
        s.insert(
            M2.into(),
            Arc::new(BuildOutput {
                library_paths: vec![(true, "/w/target/debug/build/b/out/lib".into())],
                ..Default::default()
            }),
        );
        s
    }

    fn cmd(args: &[&str], env: &[(&str, &str)]) -> Cmd {
        Cmd {
            program: "rustc".into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: env.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect(),
            cwd: None,
        }
    }

    fn t(kind: &str, meta: &str) -> String {
        token::make(kind, meta)
    }

    #[test]
    fn flags_expand_to_zero_or_more_pairs() {
        let c = cmd(
            &[
                "--crate-name",
                "x",
                "--cfg",
                &t(token::CFG, M1),
                "--check-cfg",
                &t(token::CHECK_CFG, M1),
                "--cfg",
                &t(token::CFG, M2),
                "-l",
                &t(token::LINK_LIB, M1),
                "--edition=2021",
            ],
            &[],
        );
        let e = expand(&c, &scripts(), "LD_LIBRARY_PATH", EnvRule::ScriptWins).unwrap();
        assert_eq!(
            e.args,
            [
                "--crate-name", "x", "--cfg", "has_x", "--cfg", "feature=\"y\"", "--check-cfg", "cfg(has_x)", "-l",
                "static=foo", "-l", "z", "--edition=2021"
            ]
        );
    }

    #[test]
    fn library_paths_keep_cargo_order() {
        // cargo emits all artifact-type paths, then all external ones.
        let a1 = format!("/w/target/debug/build/a/out/{}", t(token::LIBPATH_ARTIFACT, M1));
        let a2 = format!("/w/target/debug/build/b/out/{}", t(token::LIBPATH_ARTIFACT, M2));
        let c = cmd(
            &["-L", &a1, "-L", &a2, "-L", &t(token::LIBPATH_EXTERNAL, M1), "-L", &t(token::LIBPATH_EXTERNAL, M2)],
            &[],
        );
        let e = expand(&c, &scripts(), "LD_LIBRARY_PATH", EnvRule::ScriptWins).unwrap();
        assert_eq!(
            e.args,
            [
                "-L",
                "native=/w/target/debug/build/a/out",
                "-L",
                "/w/target/debug/build/b/out/lib",
                "-L",
                "/usr/lib/ext",
                "-L",
                "/w/target/debug/shared"
            ]
        );
    }

    #[test]
    fn link_args_keep_script_order_within_selected_kinds() {
        let c = cmd(
            &[
                "-C",
                &format!("link-arg={}", token::link_arg("ALL", M1)),
                "-C",
                &format!("link-arg={}", token::link_arg("BIN", M1)),
                "-C",
                &format!("link-arg={}", token::link_arg_bin(M1, "tool")),
                "-C",
                "opt-level=3",
            ],
            &[],
        );
        let e = expand(&c, &scripts(), "LD_LIBRARY_PATH", EnvRule::ScriptWins).unwrap();
        assert_eq!(
            e.args,
            [
                "-C", "link-arg=-Wl,--first", "-C", "link-arg=-Wl,--bins", "-C", "link-arg=-Wl,--tool", "-C",
                "link-arg=-Wl,--last", "-C", "opt-level=3"
            ]
        );
        // A different binary's token selects nothing extra.
        let c = cmd(&["-C", &format!("link-arg={}", token::link_arg_bin(M1, "other")), "-g"], &[]);
        let e = expand(&c, &scripts(), "LD_LIBRARY_PATH", EnvRule::ScriptWins).unwrap();
        assert_eq!(e.args, ["-g"]);
    }

    #[test]
    fn env_metadata_and_search_paths() {
        let md = format!("DEP_Z_{}", envify(&t(token::METADATA, M1)));
        let ld = format!(
            "{}@/w/target/debug:/w/target/debug/deps:{}@/w/target/debug:/sysroot/lib",
            t(token::DYN_LIBPATHS, M1),
            token::NATIVE_DIRS
        );
        let c = cmd(
            &["-L", token::NATIVE_DIRS],
            &[(&t(token::ENV, M1), ""), (&md, ""), ("LD_LIBRARY_PATH", &ld), ("CARGO_PKG_NAME", "real")],
        );
        let s = scripts();
        let e = expand(&c, &s, "LD_LIBRARY_PATH", EnvRule::ScriptWins).unwrap();
        let get = |k: &str| e.env.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.clone());
        assert_eq!(get("GENERATED").as_deref(), Some("1"));
        assert_eq!(get("DEP_Z_INCLUDE").as_deref(), Some("/inc"));
        assert_eq!(get("DEP_Z_ROOT_DIR").as_deref(), Some("/r"));
        assert_eq!(get("CARGO_PKG_NAME").as_deref(), Some("evil"));
        // Script order for one script, then the sorted set of all; only
        // paths under the target root survive, without their kind prefix.
        assert_eq!(
            get("LD_LIBRARY_PATH").as_deref(),
            Some(
                "/w/target/debug/build/a/out:/w/target/debug/shared:/w/target/debug/deps:\
                 /w/target/debug/build/b/out/lib:/w/target/debug/shared:/w/target/debug/build/a/out:/sysroot/lib"
            )
        );
        // `-L` for doctests lists every path, unfiltered and sorted.
        assert_eq!(
            e.args,
            [
                "-L",
                "/usr/lib/ext",
                "-L",
                "/w/target/debug/build/b/out/lib",
                "-L",
                "/w/target/debug/shared",
                "-L",
                "native=/w/target/debug/build/a/out"
            ]
        );
        // For processes cargo runs for the user, its own variables win.
        let e = expand(&c, &s, "LD_LIBRARY_PATH", EnvRule::CargoVarsWin).unwrap();
        let get = |k: &str| e.env.iter().find(|(n, _)| n == k).and_then(|(_, v)| v.clone());
        assert_eq!(get("CARGO_PKG_NAME").as_deref(), Some("real"));
    }

    #[test]
    fn unknown_or_missing_is_an_error() {
        let c = cmd(&["--cfg", &t(token::CFG, "0000000000000000")], &[]);
        assert!(expand(&c, &scripts(), "LD_LIBRARY_PATH", EnvRule::ScriptWins).is_err());
        let c = cmd(&["__RARGO_S_BOGUS_00112233aabbccdd__"], &[]);
        assert!(expand(&c, &scripts(), "LD_LIBRARY_PATH", EnvRule::ScriptWins).is_err());
    }
}
