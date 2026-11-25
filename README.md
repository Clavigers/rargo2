# rargo

rargo is remote cargo. A build server does your Rust builds.

`rargo build`, `check`, `test`, `run`, `clippy` and `doc` work like the cargo commands.
They do not build on your machine.
Your files go to the server. The server builds them. You get the output and the files you asked for.

Everyone who uses the same server shares one cache.
What one machine builds is a hit on every other machine.

## Start

You need Linux.

```sh
cargo install rargo                 # or: curl -fsSL https://rargo.rs/install.sh | sh
cd my-project
rargo @build.example                # the name of your server
rargo auth                          # enter the invitation your admin gave you
rargo build
```

## Docs

Read [docs/reference.md](docs/reference.md) for the client, the server, the policy in `Cargo.toml` and fixes for problems.
