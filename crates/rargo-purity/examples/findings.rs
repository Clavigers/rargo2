//! Prints every finding of the purity checks, one per line: `check<TAB>file:line<TAB>what`.
//! `cargo run -p rargo-purity --example findings [check] [file-prefix]`

use rargo_purity::{scan, Check, SOURCES};

fn main() {
    let mut args = std::env::args().skip(1);
    let check = args.next().and_then(|name| Check::parse(&name));
    let prefix = args.next().unwrap_or_default();
    let mut findings = scan(SOURCES);
    findings.sort();
    for f in findings {
        if check.is_none_or(|c| c == f.check) && f.file.starts_with(&prefix) {
            println!("{}\t{}:{}\t{}", f.check.name(), f.file, f.line, f.what);
        }
    }
}
