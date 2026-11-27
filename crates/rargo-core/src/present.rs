use crate::io::UnboundedSender;
use crate::proto::ServerMsg;

#[derive(Clone, Debug, Default)]
pub struct Translator {
    pairs: Vec<(String, String)>,
}

fn is_path_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/' | b'~' | b'+')
}

fn boundary_before(s: &[u8], pos: usize) -> bool {
    if pos == 0 {
        return true;
    }
    let c = s[pos - 1];
    if !is_path_byte(c) {
        return true;
    }
    if c == b'm' {
        let mut i = pos - 1;
        while i > 0 && (s[i - 1].is_ascii_digit() || s[i - 1] == b';') {
            i -= 1;
        }
        return i >= 2 && s[i - 1] == b'[' && s[i - 2] == 0x1b;
    }
    false
}

fn boundary_after(s: &[u8], pos: usize) -> bool {
    match s.get(pos) {
        None => true,
        Some(c) => !is_path_byte(*c) || *c == b'/',
    }
}

impl Translator {
    pub fn new(mut pairs: Vec<(String, String)>) -> Translator {
        pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
        Translator { pairs }
    }

    pub fn text(&self, s: &str) -> String {
        if self.pairs.is_empty() || !self.pairs.iter().any(|(c, _)| s.contains(c.as_str())) {
            return s.to_string();
        }
        let b = s.as_bytes();
        let mut out = String::with_capacity(s.len() + 64);
        let mut i = 0;
        let mut copied = 0;
        'outer: while i < b.len() {
            if b[i] == b'/' {
                for (canon, real) in &self.pairs {
                    let cb = canon.as_bytes();
                    if b[i..].starts_with(cb) && boundary_before(b, i) && boundary_after(b, i + cb.len()) {
                        out.push_str(&s[copied..i]);
                        out.push_str(real);
                        i += cb.len();
                        copied = i;
                        continue 'outer;
                    }
                }
            }
            i += 1;
        }
        out.push_str(&s[copied..]);
        out
    }

    pub fn path(&self, p: &str) -> String {
        for (canon, real) in &self.pairs {
            if p == canon {
                return real.clone();
            }
            if let Some(rest) = p.strip_prefix(canon.as_str()) {
                if rest.starts_with('/') {
                    return format!("{real}{rest}");
                }
            }
        }
        p.to_string()
    }

    pub fn to_canonical(&self, p: &str) -> String {
        self.pairs
            .iter()
            .filter_map(|(canon, real)| {
                let rest = p.strip_prefix(real.as_str())?;
                (rest.is_empty() || rest.starts_with('/')).then_some((real.len(), canon, rest))
            })
            .max_by_key(|(len, _, _)| *len)
            .map_or_else(|| p.to_string(), |(_, canon, rest)| format!("{canon}{rest}"))
    }
}

pub fn strip_ansi(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == 0x1b && i + 1 < b.len() {
            match b[i + 1] {
                b'[' => {
                    i += 2;
                    while i < b.len() && !(0x40..=0x7e).contains(&b[i]) {
                        i += 1;
                    }
                    i += 1;
                    continue;
                }
                b']' => {
                    i += 2;
                    while i < b.len() {
                        if b[i] == 0x07 {
                            i += 1;
                            break;
                        }
                        if b[i] == 0x1b && b.get(i + 1) == Some(&b'\\') {
                            i += 2;
                            break;
                        }
                        i += 1;
                    }
                    continue;
                }
                _ => {}
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

const GREEN: &str = "\x1b[1m\x1b[92m";
const YELLOW: &str = "\x1b[1m\x1b[93m";
const RED: &str = "\x1b[1m\x1b[91m";
const CYAN: &str = "\x1b[1m\x1b[96m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

pub struct Output {
    tx: UnboundedSender<ServerMsg>,
    pub color_err: bool,
    pub color_out: bool,
    pub quiet: bool,
    pub verbose: bool,
    pub tr: Translator,
    kept: Option<std::rc::Rc<std::cell::RefCell<Vec<u8>>>>,
}

impl Output {
    pub fn new(tx: UnboundedSender<ServerMsg>, color_err: bool, color_out: bool) -> Output {
        Output { tx, color_err, color_out, quiet: false, verbose: false, tr: Translator::default(), kept: None }
    }

    pub fn derive(&self, quiet: bool, verbose: bool, tr: Translator) -> Output {
        Output {
            tx: self.tx.clone(),
            color_err: self.color_err,
            color_out: self.color_out,
            quiet,
            verbose,
            tr,
            kept: self.kept.clone(),
        }
    }

    pub fn keeping_stdout(&self, into: std::rc::Rc<std::cell::RefCell<Vec<u8>>>) -> Output {
        Output { kept: Some(into), ..self.derive(self.quiet, self.verbose, self.tr.clone()) }
    }

    pub fn send(&self, m: ServerMsg) {
        let _ = self.tx.send(m);
    }

    pub fn raw_err(&self, b: &[u8]) {
        if !b.is_empty() {
            self.send(ServerMsg::Stderr(b.to_vec()));
        }
    }

    pub fn raw_out(&self, b: &[u8]) {
        if let Some(k) = &self.kept {
            k.borrow_mut().extend_from_slice(b);
            return;
        }
        if !b.is_empty() {
            self.send(ServerMsg::Stdout(b.to_vec()));
        }
    }

    pub fn err_text(&self, s: &str) {
        let t = self.tr.text(s);
        let t = if self.color_err { t } else { strip_ansi(&t) };
        self.raw_err(t.as_bytes());
    }

    pub fn out_text(&self, s: &str) {
        let t = self.tr.text(s);
        let t = if self.color_out { t } else { strip_ansi(&t) };
        self.raw_out(t.as_bytes());
    }

    pub fn status(&self, verb: &str, msg: &str) {
        if self.quiet {
            return;
        }
        self.err_text(&format!("{GREEN}{verb:>12}{RESET} {msg}\n"));
    }

    pub fn warn(&self, msg: &str) {
        if self.quiet {
            return;
        }
        self.err_text(&format!("{YELLOW}warning{RESET}{BOLD}:{RESET} {msg}\n"));
    }

    pub fn error(&self, msg: &str) {
        self.err_text(&format!("{RED}error{RESET}{BOLD}:{RESET} {msg}\n"));
    }

    pub fn help(&self, msg: &str) {
        if self.quiet {
            return;
        }
        self.err_text(&format!("{CYAN}help{RESET}{BOLD}:{RESET} {msg}\n"));
    }

    pub fn note(&self, msg: &str) {
        if self.quiet {
            return;
        }
        self.err_text(&format!("{CYAN}note{RESET}{BOLD}:{RESET} {msg}\n"));
    }
}

pub fn elapsed(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{:02}s", secs, d.subsec_nanos() / 10_000_000)
    }
}

pub fn display_cmd(program: &str, args: &[String]) -> String {
    let mut s = String::from("`");
    s.push_str(program);
    for a in args {
        s.push(' ');
        s.push_str(&shell_escape::escape(a.as_str().into()));
    }
    s.push('`');
    s
}

pub fn describe_status(status: i32) -> String {
    if libc::WIFEXITED(status) {
        return format!("exit status: {}", libc::WEXITSTATUS(status));
    }
    if libc::WIFSIGNALED(status) {
        let sig = libc::WTERMSIG(status);
        let name = match sig {
            libc::SIGABRT => ", SIGABRT: process abort signal",
            libc::SIGALRM => ", SIGALRM: alarm clock",
            libc::SIGFPE => ", SIGFPE: erroneous arithmetic operation",
            libc::SIGHUP => ", SIGHUP: hangup",
            libc::SIGILL => ", SIGILL: illegal instruction",
            libc::SIGINT => ", SIGINT: terminal interrupt signal",
            libc::SIGKILL => ", SIGKILL: kill",
            libc::SIGPIPE => ", SIGPIPE: write on a pipe with no one to read",
            libc::SIGQUIT => ", SIGQUIT: terminal quit signal",
            libc::SIGSEGV => ", SIGSEGV: invalid memory reference",
            libc::SIGTERM => ", SIGTERM: termination signal",
            libc::SIGBUS => ", SIGBUS: access to undefined memory",
            libc::SIGSYS => ", SIGSYS: bad system call",
            libc::SIGTRAP => ", SIGTRAP: trace/breakpoint trap",
            _ => "",
        };
        return format!("signal: {sig}{name}");
    }
    format!("status: {status}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tr() -> Translator {
        Translator::new(vec![("/w".into(), "/home/me/proj".into()), ("/c".into(), "/home/me/.cargo".into())])
    }

    #[test]
    fn translates_paths_at_boundaries_only() {
        let t = tr();
        assert_eq!(t.text("  --> /w/src/lib.rs:3:5"), "  --> /home/me/proj/src/lib.rs:3:5");
        assert_eq!(t.text("\"manifest_path\":\"/w/Cargo.toml\""), "\"manifest_path\":\"/home/me/proj/Cargo.toml\"");
        assert_eq!(t.text("foo v0.1.0 (/w)"), "foo v0.1.0 (/home/me/proj)");
        assert_eq!(t.text("at /c/registry/src/x/lib.rs"), "at /home/me/.cargo/registry/src/x/lib.rs");
        assert_eq!(t.text("see http://example.com/w/page"), "see http://example.com/w/page");
        assert_eq!(t.text("/work/x and /w2 and a/w/b"), "/work/x and /w2 and a/w/b");
        assert_eq!(t.text("/usr/lib/w/x"), "/usr/lib/w/x");
        assert_eq!(t.text("\x1b[1m\x1b[94m/w/src/a.rs\x1b[0m"), "\x1b[1m\x1b[94m/home/me/proj/src/a.rs\x1b[0m");
        assert_eq!(t.text("from/w/x"), "from/w/x");
        assert_eq!(t.path("/w/a"), "/home/me/proj/a");
        assert_eq!(t.path("/w"), "/home/me/proj");
        assert_eq!(t.path("/wx"), "/wx");
        assert_eq!(t.to_canonical("/home/me/proj/sub"), "/w/sub");
        assert_eq!(t.to_canonical("/home/me/project"), "/home/me/project");
    }

    #[test]
    fn strips_colour() {
        assert_eq!(strip_ansi("\x1b[1m\x1b[91merror\x1b[0m: x"), "error: x");
        assert_eq!(strip_ansi("\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\ ok"), "link ok");
    }

    #[test]
    fn elapsed_like_cargo() {
        assert_eq!(elapsed(std::time::Duration::from_millis(1234)), "1.23s");
        assert_eq!(elapsed(std::time::Duration::from_secs(75)), "1m 15s");
    }
}
