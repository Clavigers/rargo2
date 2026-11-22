# rargo

Read `DESIGN.md` first. It is the spec. `KICKOFF.md` is the standing brief for
the implementation run.

## This machine

A shared Hetzner box (32 threads, 124 GB RAM, ext4 on RAID1). It runs live
services that must not be disturbed:

- Do not reboot, and do not restart or stop any existing systemd service,
  container or VM.
- Do not touch `/srv/*`, `/opt/*`, or other directories under `/root` besides
  this repo and `/root/rargo-work`.
- Do not bind ports already in use (check with `ss -tuln`), and do not change
  firewall or network configuration outside namespaces you create.
- Keep heavy builds under a CPU and memory cap (for example
  `systemd-run --scope -p CPUQuota=2400% -p MemoryMax=90G`), so the existing
  services keep headroom.
- Put large scratch data (rustc and cargo checkouts, benchmark workspaces,
  loopback filesystems) under `/root/rargo-work`. About 1 TB is free.

Facts that affect the design:

- The running kernel is 6.8, which has no FUSE passthrough (needs 6.9+). A
  7.0 HWE kernel is available in apt, but using it needs a reboot, which is
  not allowed. Build so that passthrough is used when the kernel has it and a
  userspace read path is used otherwise.
- The root filesystem is ext4 (no reflinks). A loopback btrfs or XFS image
  under `/root/rargo-work` is fine for the content store.
- Nix on this box is 2.18.

## Working rules

- Commit early and often on `main`. Push to `origin` if one is configured.
- No stubs, no TODO placeholders, no spikes. Each piece is implemented and
  tested before moving on.
- Do not ask the user to make decisions. Decide, record the decision and the
  reason in `DECISIONS.md`, and continue.
- Report results as they are: what was verified, how, and what was not.
