---
title: Reference
subtitle: "rargo {{RARGO_VERSION}}: installing the client, installing a server, and what happens in between"
lang: en
toc-title: Contents
---

# The Client

Linux on x86_64, where the kernel lets rargo use io_uring. Docker's default seccomp profile does not: start a container with `--security-opt seccomp=unconfined`. Either of these puts one `rargo` binary on your machine; the first builds it, the second downloads it.

<cmd-list>
<cmd>cargo install rargo</cmd>
<cmd>curl -fsSL https://rargo.rs/install.sh | sh</cmd>
</cmd-list>

Then, inside a cargo project:

<cmd-list>
<cmd>rargo @rargo.rs</cmd>
<cmd>rargo auth</cmd>
<cmd>rargo build</cmd>
</cmd-list>

`rargo @host` writes the server's name to `~/.config/rargo/config.toml` (under `$XDG_CONFIG_HOME` if you set it), beside the tokens. It is your setting, not the project's: every Rust project on the machine builds there, fresh clones included, and no file in the project changes. `RARGO_SERVER` overrides it for one command.

`rargo auth` prints a short code and opens a page. Check that the page shows the same code, then prove who you are: the page asks for what the server's administrator gave you, an invitation or a sign-in they have allowed. If you have neither, the page and `rargo auth` both say how that server lets people in. Your terminal receives a token and stores it in `~/.config/rargo/credentials.toml`. That is all the setup there is.

The public farm at `rargo.rs` is one machine and admits only people its owner has let in. To ask for access, [open a request on GitHub](https://github.com/sadosystems/rargo/issues/new?title=Request%20access%20to%20the%20Rargo%20Farm) with your GitHub username. You can also use a server of your own: [installing one](#a-server) is one command, it prints an invitation for you, and `rargo @your-server:7459` takes the place of `rargo @rargo.rs`. Everything else on this page is the same.

<aside>
A machine without a browser is fine. Open the address `rargo auth` prints on any other device.
</aside>

Everything that is not a build (`cargo add`, `cargo login`, `cargo fmt`) is handed to your real cargo untouched.

## Commands that are rargo's own

| | |
|---|---|
| `rargo @host` | name your server (`rargo.rs`, or `build.example:7459`) |
| `rargo status` | your server, account, daemon, mount and versions |
| `rargo auth [HOST]` | get this machine a token for your server, or for HOST |
| `rargo auth --token T [HOST]` | keep a token an administrator made (`rargod token create`) |
| `rargo auth status` | which server you build on, what named it, and as whom |
| `rargo auth remove [HOST]` | revoke the token on the server and forget it here |
| `rargo auth tokens` | your tokens on that server; an administrator sees all of them |
| `rargo auth invite`, `rargo auth create`, `rargo auth revoke`, `rargo auth allow`, `rargo auth disallow` | manage who may connect (see [Who may connect](#who-may-connect)) |
| `rargo clean` | like `cargo clean`, and lets go of a mounted `target/` first |
| `rargo --rargo-version` | the client's version and protocol |
| `rargo --rargo-status` | what the server has been doing |
| `rargo --rargo-settle` | return when workers have what was sent and state that is due has come home |
| `rargo --rargo-daemon-status` | what the local daemon is doing |
| `rargo --rargo-daemon-sync` | send the worktree to the server and return when it holds it |
| `rargo --rargo-daemon-stop` | stop every local daemon of this cache |

## Nextest

`rargo nextest` builds the test binaries and runs nextest on the build server. It returns nextest's output, exit status, and reports such as JUnit XML. The client passes `NEXTEST_*` variables.

Set `nextest = "X.Y.Z"` under `[workspace.metadata.rargo]` in the project's `Cargo.toml`, or set `nextest-version` in `.config/nextest.toml`, to pin the version. Without a project version, rargo uses the version installed on the client if available, then the newest release. The server verifies the download's checksum and keeps each version installed separately.

Each test process is cached by its command line and what it reads. A changed fixture reruns the tests that read it. Failures are always rerun, so nextest handles retries and flaky reports.

## Environment variables

| | |
|---|---|
| `RARGO_SERVER` | overrides the server `~/.config/rargo/config.toml` names |
| `RARGO_TOKEN` | a token to use instead of the stored one (for CI) |
| `RARGO_FALLBACK=cargo` | build with plain cargo when rargo cannot build: the server cannot be reached or does not let this machine in, or this machine cannot run rargo |
| `RARGO_OUTPUT=copy` | copy results into `target/` instead of serving it |
| `RARGO_TRANSPORT=udp` or `tcp` | force one transport |
| `RARGO_CACHE_DIR` | where the client keeps its cache (default `~/.cache/rargo`) |

# A Server

Linux on x86_64, glibc 2.39 or newer (Ubuntu 24.04, Debian 13), root, FUSE, a C toolchain for the crates that need one, and Nix with flakes for the projects' environments. One command installs `rargod`, a systemd service for it, and rustup for the toolchains your projects ask for.

<cmd>curl -fsSL https://rargo.rs/install-server.sh | sudo sh</cmd>

It ends by printing an invitation for the first user and the three commands to run on the machine you build from. The server listens on port 7459 and must be reachable there over **both UDP and TCP**.

<aside>
The client speaks UDP and falls back to TCP on networks that drop it. Open both.
</aside>

## Who may connect

Tokens, managed while the server runs. An administrator does it from any machine that is authenticated to the server:

<figure class="example">
```sh
rargo auth invite --user ann     # a code ann enters on the rargo auth page
rargo auth allow ann-on-github   # may sign in with GitHub instead
rargo auth tokens
rargo auth revoke ann            # her next build is refused
```
</figure>

The same commands run on the server itself as `rargod token invite|create|list|revoke|allow|disallow --state DIR` (`--state` is `/var/lib/rargo/state` after the installer). That is where the first administrator comes from: the installer's invitation, or `rargod token create --user NAME --admin`. Anyone else may list and revoke their own tokens.

Signing in with GitHub needs a GitHub OAuth application of your own, whose callback is `https://YOUR-NAME/auth/github/callback`, given to the server once with `rargod token github`. Without one, `rargo auth` asks for an invitation; with one, it takes either. `rargod token access --state DIR TEXT` sets what the page and `rargo auth` tell someone not let in yet: "To build here, TEXT." Without it they name `rargod token allow` and `rargo auth invite`.

## A name instead of an address

`rargo @build.example:7459` needs nothing else: a name with a port is the server itself.

A bare name like `rargo.rs` is asked over HTTPS where its server is. To use one, serve `https://YOUR-NAME/.well-known/rargo.json` and the paths under `/auth` from the build server, which answers them as plain HTTP on its TCP port. Any reverse proxy will do.

## What builds can see

A project without a `flake.nix` builds in rargo's pinned default Nix
shell. It provides Rust with Cargo, Clippy, rustfmt and the standard
library's source, GCC for C and C++, binutils, Make, CMake, pkg-config,
Perl, Python 3 and Git, and the libraries OpenSSL, zlib, SQLite, udev,
ALSA, Wayland, xkbcommon, the Vulkan loader, fontconfig, FreeType and
the X11 libraries. The project's `rust-toolchain.toml` or
`rust-toolchain` selects Rust. The sandbox exposes the selected closure and keeps
the server's system libraries outside the build. The server needs Nix with flakes.

From your Cargo workspace, download the shell for the release you use
and inspect its files. Here `version` is the version published at
rargo.rs:

<figure class="example">
```sh
version=$(curl -fsSL https://rargo.rs/dl/VERSION)
shell_dir=$(mktemp -d)
curl -fsSL "https://rargo.rs/dl/$version/rargo-default-flake.tar.gz" \
  | tar -xz -C "$shell_dir"
cat "$shell_dir/default-flake/flake.nix"
```
</figure>

To run stock Cargo in the same system environment with your local Rust
toolchain, enter the bundle's `system` shell. Run these commands from
the workspace:

<figure class="example">
```sh
nix develop "path:$shell_dir/default-flake#system" --command \
  pkg-config --modversion openssl zlib sqlite3
nix develop "path:$shell_dir/default-flake#system" --command cargo build
```
</figure>

If the project needs another library, copy the bundled `flake.nix` and
`flake.lock` into the workspace. For example, add `xz` to the `libs`
list to build against liblzma. Commit both files. The workspace's
`default` dev shell then supplies the tools and libraries for
`rargo build` and local `nix develop --command cargo build`.

<figure class="example">
```sh
cp "$shell_dir/default-flake/flake.nix" "$shell_dir/default-flake/flake.lock" .
# Edit flake.nix before committing it.
git add flake.nix flake.lock
git commit -m "Set the workspace build environment"
rargo build
```
</figure>

# How It Works

Your machine holds your source. The server holds everything else.

1. **Sync.** A small daemon on your machine watches the project and sends the server what changed, as you save. Files are named by their content, so a file the server has seen, from you or from a teammate, is never sent twice.
2. **Plan.** The server runs cargo's own planning for your command and gets the list of compiler runs it implies.
3. **Run or reuse.** Each run happens in a sandbox that records every file and environment variable it read. A later request that would read the same things gets the earlier result. This holds across worktrees, branches and people.
4. **Return.** Output streams back as it happens. Your `target/` directory is served by the daemon: files appear as soon as the server has them and are downloaded only when something opens them.

So a second worktree, a fresh clone, or a colleague building the commit you just built costs a round trip, not a build.

<aside>
`target/` is mounted with FUSE when the client can. Otherwise results are copied into it, which works everywhere and costs the download.
</aside>

# Configuration

Your server is yours, in `~/.config/rargo/config.toml`, and `rargo @host` writes it:

<figure class="example">
```toml
host = "rargo.rs"
```
</figure>

What a build is allowed to see beyond its sources belongs to the project, under `[workspace.metadata.rargo]` in the manifest at the top of the repository. All of it is optional.

<figure class="example">
```toml
[workspace.metadata.rargo.env]
pass = ["DATABASE_URL", "PROTOC*"]   # reach every build step

[workspace.metadata.rargo.network]
tests = false                          # true: every test may use the network
test-packages = ["integration-tests"]  # these tests may use the network
build-scripts = ["some-sys"]           # so may these build scripts
git-build-scripts-offline = false      # true: git dependencies' build scripts lose it too

[workspace.metadata.rargo.sync]
exclude = ["node_modules"]           # never sent to the server
gitignored = false                   # nor anything git ignores
include = ["config/local.toml"]      # but this, although git ignores it
git = "history"                      # build scripts see every commit and tag

[workspace.metadata.rargo.flake]
files = ["nix/build-settings.json"]   # extra files the workspace flake reads
```
</figure>

What git ignores is synced unless `gitignored = false` says otherwise, because a build may read it: a generated file, a local `.env`, a file `include_str!` embeds. With the option it is not scanned, watched or sent, and a build that fails for want of such a file ends with a note naming it; `include` takes it back. `include` and `exclude` take a name (`node_modules`, anywhere) or a path from the top of the repository.

Build scripts that ask git about the commit (`git rev-parse HEAD`, `git log -1`, `git describe --dirty`) get the answers they would get on your machine; `.git` itself is never sent, only a stand-in with HEAD and the tags on it. A build script that asks about history (`git describe --tags`, `git rev-list --count`) also gets the commits before HEAD, without their contents. rargo sends them where a `build.rs` mentions `describe` or `rev-list`, or `Cargo.lock` names vergen, shadow-rs, git-version, built or git-testament. `git = "history"` sends them always, `git = "head"` never, `git = false` sends no `.git` at all.

A package that is not in a workspace writes `package.metadata.rargo` instead.

On your machine, rargo keeps three things: your server in `~/.config/rargo/config.toml`, the tokens in `~/.config/rargo/credentials.toml`, and a cache in `~/.cache/rargo`.

# Troubleshooting

**"this machine is not authenticated"**, or the server no longer knows its token (it was revoked or has expired). Run `rargo auth`.

**The page will not give me a token.** The server you named admits only people its administrator has let in, and the page and `rargo auth` say how to ask ("To build here, …"). Do that, or install [a server of your own](#a-server) and name it with `rargo @your-server:7459`. Meanwhile `RARGO_FALLBACK=cargo` builds with plain cargo.

**"cannot reach the rargo server".** Check the name with `rargo status`. Then check that the server's port is open for UDP and TCP. `RARGO_TRANSPORT=tcp rargo build` tells you whether only UDP is blocked.

**A library is missing** (`pkg-config`, `cannot find -lfoo`). Add it to the workspace flake and commit `flake.nix` and `flake.lock`. See [What builds can see](#what-builds-can-see).

**A build script cannot download something.** Build scripts of crates.io dependencies and of the workspace have no network unless the project allows it under `network` in its manifest (see *Configuration*). Build scripts of git dependencies have it, unless the project sets `git-build-scripts-offline = true`.

**Client and server do not match.** They must be the same release line. The error names the server's version and the command that installs a client that matches it, `cargo install rargo --version X.Y.Z`. `rargo --rargo-version` and `rargod --version` print both versions.

**`cargo clean` fails with "Device or resource busy".** `target/` is mounted. Use `rargo clean`.

**Something is stuck.** `rargo --rargo-daemon-status` shows what the daemon is doing and `~/.cache/rargo/daemon.log` what it said. `rargo --rargo-daemon-stop` ends it; the next command starts a new one.

**rargo says directories are not watched.** The daemon watches every directory it syncs with inotify, so a command after a change looks only where something changed. The kernel limits how many directories one user may watch (`fs.inotify.max_user_watches`, shared with editors and other tools). Past it, rargo says how many directories go unwatched and which value to set. Those directories are looked at again by every command; the rest stay watched. Raise the limit with the `sysctl` it names, and add the setting to `/etc/sysctl.d/` to keep it.

**A version string differs from the one stock cargo builds** (`git describe` in a build script). The build script asks git for history in a way rargo did not recognise. Set `git = "history"` under `[workspace.metadata.rargo.sync]`.

**It is rargo's fault.** `RARGO_FALLBACK=cargo` builds locally in the meantime.
