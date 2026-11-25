fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "linux" {
        eprintln!(
            "rargo runs on Linux only: its client needs Linux's FUSE, inotify and mount namespaces, so there is no rargo for {os}. \
             A Linux machine or VM can build with rargo."
        );
        std::process::exit(1);
    }
}
