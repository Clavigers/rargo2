# Reflink store verification

Added `rargod --store DIR` so the content store can live on a separate
filesystem while the database, socket and sandbox mount stay under
`--state`. The store defaults to `<state>/store`. The e2e suite creates
unique scratch and state directories and places the store and worktrees
under `RARGO_E2E_ROOT` when set.

The machine had `mkfs.btrfs` available, so the test used the requested btrfs
fallback. The 20 GiB sparse image is at
`/root/rargo-work/xfs-store/store.btrfs`. It was mounted at
`/root/rargo-work/xfs-store/mnt` for verification and is now unmounted.

## Verification

- `dev/e2e.sh` passed all 81 checks on btrfs and all 81 on ext4. This
  included stock Cargo build, test and doc using the same target directory.
- `cargo test -p rargo-core --offline` passed 53 tests with one ignored
  throughput test.
- `filefrag -v` showed the two btrfs app executables had separate inodes
  and identical shared extents. The btrfs integration test executable also
  had shared extents. On ext4 the two app executables shared a hard-linked
  inode. With a btrfs store and ext4 worktrees, the two app executables were
  byte-identical copies with distinct inodes and distinct physical extents.

## Disk use

Started with a fresh store and the e2e fixture on each filesystem, then ran
`rargo build` and `rargo test --no-run` in each of two identical worktrees.
The baseline was taken after daemon startup and fixture setup, before
adding the second worktree. The database and daemon log stayed on the host
filesystem. The ext4 baseline used a separate loop image to isolate its
`df` delta from other jobs on the host.

| Store / worktrees | Combined `du -B1 -c` bytes | Filesystem `df -B1` used delta bytes |
| --- | ---: | ---: |
| ext4 / ext4 | 109,924,352 | 107,950,080 |
| btrfs / btrfs | 128,651,264 | 58,818,560 |
| btrfs / ext4 | 128,774,144 | 129,409,024 |

The split delta sums 58,621,952 bytes on btrfs and 70,787,072 on ext4.
`du` counts shared extents per inode, so the btrfs total overstates unique
allocation. `df` also includes filesystem accounting and fixture changes
such as the lockfile and second worktree. For this workload the same-volume
btrfs allocation grew by 46.86 MiB less than the same-volume ext4
allocation, a 45.5% reduction.

Logs are under `/root/rargo-work/codex-btrfs-e2e-final.log`,
`/root/rargo-work/codex-ext4-e2e.log`, and
`/root/rargo-work/codex-core-test.log`. The measurement script and three
logs are in `/root/rargo-work/codex-measure.McHX1sly/`.
