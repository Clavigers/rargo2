# Forks

rargo carries its changes to cargo (and later rustc) as patch series against
pinned upstream commits. The working checkouts live under `/root/rargo-work`.

- `forks/cargo/BASE` is the upstream commit; `forks/cargo/*.patch` apply on top.
- `forks/setup.sh` clones upstream at the base and applies the patches.
- `forks/export.sh` regenerates the patch files from the working checkout.
- `forks/build.sh` builds the forked cargo (`rargo-cargo`).
