# rargo

rargo is remote cargo.

`rargo build`, `check`, `test`, `run`, `clippy` and `doc` work like the cargo commands.
They do not build on your machine.
Your files go to the server. The server builds them. You get the output and the files you asked for.
Everyone who uses the same server shares one cache.

Keep Cargo path dependencies inside the synced worktree. If a dependency lives in a sibling repository outside that worktree, move it inside and update the dependency path, or use `RARGO_FALLBACK=cargo` to let rargo build locally when it detects the external dependency.

## Use a server

You need Linux. To run what rargo builds on your machine, with `rargo run` or straight from `target/`, you also need [Nix](https://nixos.org/download/).
Built programs use the build server's C library from the Nix store; the first `rargo run` fetches it into the store, and the program then runs either way.

```sh
cargo install rargo                 # or: curl -fsSL https://rargo.rs/install.sh | sh
cd my-project
rargo @build.example                # your server, kept in ~/.config/rargo/config.toml for every project
rargo auth                          # enter the invitation your admin gave you
rargo build
```

## Run a server

The server needs Linux and root.

The easiest way is the prebuilt server: `curl -fsSL https://rargo.rs/install-server.sh | sh`.
To build it from this checkout:

```sh
forks/setup.sh cargo                # clones rust-lang/cargo into /root/rargo-work/cargo and applies forks/cargo
forks/build.sh                      # builds the planner against this checkout; prints its path
cargo build --release               # builds rargod and rargo
rargod token invite --state /var/lib/rargo --user ann --admin   # prints an invitation
rargod --state /var/lib/rargo --planner /root/rargo-work/cargo/target/release/cargo --listen 0.0.0.0:7459
```

`RARGO_WORK` puts the forks somewhere other than `/root/rargo-work`.
Run `forks/build.sh` again whenever `crates/rargo-plan` changes: the planner links it.

A server with no tokens listens on `127.0.0.1` only.

## Policy

A project can set rules in `Cargo.toml`:

```toml
[workspace.metadata.rargo.sync]
exclude = ["node_modules"]             # never sent to the server

[workspace.metadata.rargo.env]
pass = ["DATABASE_URL"]                # given to every action
```

## Toolchain and system libraries

The server builds in the dev shell of the project's `flake.nix` (`devShells.<system>.default`).
A project without one gets rargo's default flake. It has:

- the Rust toolchain that `rust-toolchain.toml` names (a pinned nightly if there is none), with `rust-src`, `clippy` and `rustfmt`;
- gcc, cmake, pkg-config, perl, python3 and git;
- the libraries openssl, zlib, sqlite, udev, alsa, wayland, xkbcommon, vulkan-loader, fontconfig, freetype and the X11 libraries.

If a build script needs another library, add a `flake.nix` that puts it in the shell's `buildInputs`.

`rargo --help` lists the other commands.

## Docs

Read the docs at [rargo.rs](https://rargo.rs/).
Their source is `web/`: the Book is `web/src/index.md`, the Reference `web/src/reference/index.md`. `cd web && nix develop -c make` rebuilds `web/site/`; commit both. A release deploys the site with `deploy/update-site.sh`.
