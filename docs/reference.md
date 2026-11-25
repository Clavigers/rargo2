---
title: Reference
subtitle: Installing the client, installing a server, and what happens in between
lang: en
toc-title: Contents
---

# The Client

Linux on x86_64. Either of these puts one `rargo` binary on your machine; the first builds it, the second downloads it.

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

`rargo @host` writes the server's name into the project's `Cargo.toml`, under `[workspace.metadata.rargo]`. Cargo ignores that table, so plain cargo still works exactly as before. The server belongs to the project: two projects on one machine can build on two servers.

`rargo auth` prints a short code and opens a page. Check that the page shows the same code, then prove who you are: the page asks for what the server's administrator gave you, an invitation or a sign-in they have allowed. Your terminal receives a token and stores it in `~/.config/rargo/credentials.toml`. That is all the setup there is.

The public farm at `rargo.rs` is one machine and admits only people its owner has let in. To ask for access, [open a request on GitHub](https://github.com/sadosystems/rargo/issues/new?title=Request%20access%20to%20the%20Rargo%20Farm) with your GitHub username. You can also use a server of your own: [installing one](#a-server) is one command, it prints an invitation for you, and `rargo @your-server:7459` takes the place of `rargo @rargo.rs`. Everything else on this page is the same.

<aside>
A machine without a browser is fine. Open the address `rargo auth` prints on any other device.
</aside>

Everything that is not a build (`cargo add`, `cargo login`, `cargo fmt`) is handed to your real cargo untouched.

## Commands that are rargo's own

| | |
|---|---|
| `rargo @host` | name this project's server (`rargo.rs`, or `build.example:7459`) |
| `rargo auth` | get this machine a token for that server |
| `rargo auth --token T` | keep a token an administrator made (`rargod token create`) |
| `rargo auth status` | which server this project builds on, and as whom |
| `rargo auth remove` | revoke the token on the server and forget it here |
| `rargo auth tokens` | your tokens on that server; an administrator sees all of them |
| `rargo auth invite`, `rargo auth create`, `rargo auth revoke`, `rargo auth allow`, `rargo auth disallow` | manage who may connect (see [Who may connect](#who-may-connect)) |
| `rargo clean` | like `cargo clean`, and lets go of a mounted `target/` first |
| `rargo --rargo-version` | the client's version and protocol |
| `rargo --rargo-status` | what the server has been doing |
| `rargo --rargo-daemon-status` | what the local daemon is doing |
| `rargo --rargo-daemon-stop` | stop the local daemon |

## Nextest

`rargo nextest` builds the test binaries and runs nextest on the build server. It returns nextest's output, exit status, and reports such as JUnit XML. The client passes `NEXTEST_*` variables.

Set `nextest = "X.Y.Z"` under `[workspace.metadata.rargo]`, or set `nextest-version` in `.config/nextest.toml`, to pin the version. Without a project version, rargo uses the version installed on the client if available, then the newest release. The server verifies the download's checksum and keeps each version installed separately.

Each test process is cached by its command line and what it reads. A changed fixture reruns the tests that read it. Failures are always rerun, so nextest handles retries and flaky reports.

## Environment variables

| | |
|---|---|
| `RARGO_SERVER` | overrides the server the project names |
| `RARGO_TOKEN` | a token to use instead of the stored one (for CI) |
| `RARGO_FALLBACK=cargo` | build with plain cargo when the server cannot be reached |
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
rargo auth revoke ann            # her builds stop within seconds
```
</figure>

The same commands run on the server itself as `rargod token invite|create|list|revoke|allow|disallow --state DIR` (`--state` is `/var/lib/rargo/state` after the installer). That is where the first administrator comes from: the installer's invitation, or `rargod token create --user NAME --admin`. Anyone else may list and revoke their own tokens.

Signing in with GitHub needs a GitHub OAuth application of your own, whose callback is `https://YOUR-NAME/auth/github/callback`, given to the server once with `rargod token github`. Without one, `rargo auth` asks for an invitation.

## A name instead of an address

`rargo @build.example:7459` needs nothing else: a name with a port is the server itself.

A bare name like `rargo.rs` is asked over HTTPS where its server is. To use one, serve `https://YOUR-NAME/.well-known/rargo.json` and the paths under `/auth` from the build server, which answers them as plain HTTP on its TCP port. Any reverse proxy will do.

## What builds can see

Builds run on the server, in a sandbox, inside the dev shell of the project's `flake.nix`. What the shell has is what the build sees: the compiler, the linker, `pkg-config`, the system libraries. Nothing is installed on the server for a project, and a commit builds the same way on every server.

Every project has a flake. One that has no needs of its own starts from the default, the build essentials, as `flake.nix` at the top of the repository:

<figure class="example">
```nix
{
  description = "Build environment (rargo's default: the build essentials)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust
            rustc cargo clippy rustfmt
            # C and C++, and what build scripts call
            gcc gnumake cmake pkg-config perl python3 git
            # Libraries Rust projects most often link
            openssl zlib sqlite
          ];
        };
      });
    };
}
```
</figure>

<cmd>nix flake lock && git add flake.nix flake.lock</cmd>

A project that needs another library adds it to `packages`. `nix develop` gives the same environment on your own machine.

<aside>
A build of a project with no `flake.nix` sees whatever the server happens to have installed, which is nothing to rely on.
</aside>

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

In the project's `Cargo.toml`:

<figure class="example">
```toml
[workspace.metadata.rargo]
host = "rargo.rs"
```
</figure>

What a build is allowed to see beyond its sources goes in the same place, in the manifest at the top of the repository. All of it is optional.

<figure class="example">
```toml
[workspace.metadata.rargo.env]
pass = ["DATABASE_URL", "PROTOC*"]   # reach every build step

[workspace.metadata.rargo.network]
test-packages = ["integration-tests"]  # these tests may use the network
build-scripts = ["some-sys"]           # so may these build scripts

[workspace.metadata.rargo.sync]
exclude = ["node_modules"]           # never sent to the server
gitignored = false                   # nor anything git ignores
include = ["config/local.toml"]      # but this, although git ignores it
git = "history"                      # build scripts see every commit and tag

[workspace.metadata.rargo.flake]
enabled = false                      # do not build inside the repo's flake dev shell
```
</figure>

What git ignores is synced unless `gitignored = false` says otherwise, because a build may read it: a generated file, a local `.env`, a file `include_str!` embeds. With the option it is not scanned, watched or sent, and a build that fails for want of such a file ends with a note naming it; `include` takes it back. `include` and `exclude` take a name (`node_modules`, anywhere) or a path from the top of the repository.

Build scripts that ask git about the commit (`git rev-parse HEAD`, `git log -1`, `git describe --dirty`) get the answers they would get on your machine; `.git` itself is never sent, only a stand-in with HEAD and the tags on it. A build script that asks about history (`git describe --tags`, `git rev-list --count`) also gets the commits before HEAD, without their contents. rargo sends them where a `build.rs` mentions `describe` or `rev-list`, or `Cargo.lock` names vergen, shadow-rs, git-version, built or git-testament. `git = "history"` sends them always, `git = "head"` never, `git = false` sends no `.git` at all.

A package that is not in a workspace writes `package.metadata.rargo` instead.

On your machine, rargo keeps two things: the tokens in `~/.config/rargo/credentials.toml`, and a cache in `~/.cache/rargo`.

# Troubleshooting

**"this machine is not authenticated"**, or "the server does not know this token, or it was revoked or has expired". Run `rargo auth`.

**The page asks for an invitation I do not have.** The server you pointed the project at admits only people its administrator has let in. Ask them, or install [a server of your own](#a-server) and point the project at it with `rargo @your-server:7459`.

**"cannot reach the rargo server".** Check the name with `rargo auth status`. Then check that the server's port is open for UDP and TCP. `RARGO_TRANSPORT=tcp rargo build` tells you whether only UDP is blocked.

**A library is missing** (`pkg-config`, `cannot find -lfoo`). The build ran on the server and the server does not have it. See *What builds can see*.

**A build script cannot download something.** Build scripts have no network unless the project allows it under `network` in its manifest (see *Configuration*).

**Client and server disagree about the protocol.** They must be the same release line. `rargo --rargo-version` and `rargod --version` print the protocol number; bring both to the same release.

**`cargo clean` fails with "Device or resource busy".** `target/` is mounted. Use `rargo clean`.

**Something is stuck.** `rargo --rargo-daemon-status` shows what the daemon is doing and `~/.cache/rargo/daemon.log` what it said. `rargo --rargo-daemon-stop` ends it; the next command starts a new one.

**rargo says directories are not watched.** The daemon watches every directory it syncs with inotify, so a command after a change looks only where something changed. The kernel limits how many directories one user may watch (`fs.inotify.max_user_watches`, shared with editors and other tools). Past it, rargo says how many directories go unwatched and which value to set. Those directories are looked at again by every command; the rest stay watched. Raise the limit with the `sysctl` it names, and add the setting to `/etc/sysctl.d/` to keep it.

**A version string differs from the one stock cargo builds** (`git describe` in a build script). The build script asks git for history in a way rargo did not recognise. Set `git = "history"` under `[workspace.metadata.rargo.sync]`.

**It is rargo's fault.** `RARGO_FALLBACK=cargo` builds locally in the meantime.
