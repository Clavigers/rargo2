# rargo

rargo is remote cargo. A build server does your Rust builds.

`rargo build`, `check`, `test`, `run`, `clippy` and `doc` work like the cargo commands.
They do not build on your machine.
Your files go to the server. The server builds them. You get the output and the files you asked for.
Everyone who uses the same server shares one cache.

## Use a server

You need Linux.

```sh
cargo install rargo                 # or: curl -fsSL https://rargo.rs/install.sh | sh
cd my-project
rargo @build.example                # the name of your server
rargo auth                          # enter the invitation your admin gave you
rargo build
```

## Run a server

The server needs Linux and root.

```sh
forks/setup.sh                      # builds the planner
cargo build --release               # builds rargod and rargo
rargod token invite --state /var/lib/rargo --user ann --admin   # prints an invitation
rargod --state /var/lib/rargo --planner /path/to/cargo-fork/target/release/cargo --listen 0.0.0.0:7459
```

A server with no tokens listens on `127.0.0.1` only.

## Policy

A project can set rules in `Cargo.toml`:

```toml
[workspace.metadata.rargo]
host = "build.example"

[workspace.metadata.rargo.sync]
exclude = ["node_modules"]             # never sent to the server

[workspace.metadata.rargo.env]
pass = ["DATABASE_URL"]                # given to every action
```

`rargo --help` lists the other commands.

## Docs

Read the docs at [rargo.rs](https://rargo.rs).
