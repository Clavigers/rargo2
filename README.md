# rargo

rargo is remote cargo.

`rargo build`, `check`, `test`, `run`, `clippy` and `doc` work like the cargo commands.
They build on the server when invoked directly. Cargo aliases such as `lint` or `xtask` are passed to local Cargo, which expands and runs them on your machine. Rargo prints a notice on stderr when it hands off these commands.
Your files go to the server. The server builds them. You get the output and the files you asked for.
Everyone who uses the same server shares one cache.

Keep Cargo path dependencies inside the synced worktree. If a dependency lives in a sibling repository outside that worktree, move it inside and update the dependency path, or use `RARGO_FALLBACK=cargo` to let rargo build locally when it detects the external dependency.

## Use a server

You need Linux. To run what rargo builds on your machine, with `rargo run` or straight from `target/`, you also need [Nix](https://nixos.org/download/).
Built programs use the build server's C library from the Nix store; the first `rargo run` fetches it into the store, and the program then runs either way.
Paths compiled into a program, `env!("CARGO_MANIFEST_DIR")` and whatever macros such as `include_dir!` or `sqlx::migrate!` derive from it, name `/w`, which is your checkout only under `rargo run`.
A program that finds its files that way must be started with `rargo run`, and reports those paths as `/w/...`; a program that does not use such paths also runs straight from `target/`.

The path compiled in by `env!("CARGO")` points to the Farm’s Cargo in its Nix store and may be missing on your machine. To launch Cargo from your program, read `CARGO` at runtime with `std::env::var_os("CARGO")`. When Cargo is installed locally, `rargo run` sets that variable to its path.

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
It puts `rargod` in `/usr/local/bin`, so the Reference's `rargod token ...` commands run as written.
To build it from this checkout:

```sh
forks/setup.sh cargo                # clones rust-lang/cargo into /root/rargo-work/cargo and applies forks/cargo
forks/build.sh                      # builds the planner against this checkout; prints its path
cargo build --release               # builds rargod and rargo
target/release/rargod token invite --state /var/lib/rargo --user ann --admin   # prints an invitation
target/release/rargod --state /var/lib/rargo --planner /root/rargo-work/cargo/target/release/cargo --listen 0.0.0.0:7459
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

The `dev` profile's default debug info follows the Cargo version shipped with the project's Rust toolchain.

Remote builds reject `+TOOLCHAIN` and nonempty `RUSTUP_TOOLCHAIN` overrides.
To build for another target, list it in `rust-toolchain.toml` (`targets = ["wasm32-unknown-unknown"]`, beside a `channel`) or in your flake's toolchain; `rustup target add` does not change the server's toolchain.

If a build script needs another library, add a `flake.nix` that puts it in the shell's `buildInputs`.
A build script that needs a tool, such as `protoc` (prost-build, tonic-build) or libclang (bindgen), needs it in the shell too: `protobuf`, or `llvmPackages.libclang` with `LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";`, in its `nativeBuildInputs`.

`rargo --help` lists the other commands.

## Docs

Read the docs at [rargo.rs](https://rargo.rs/).
Their source is `web/`: the Book is `web/src/index.md`, the Reference `web/src/reference/index.md`. `cd web && nix develop -c make` rebuilds `web/site/`; commit both. A release deploys the site with `deploy/update-site.sh`.
