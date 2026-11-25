# TLA+ models

Models of rargo's concurrent protocols, checked with TLC.

- `StoreSync.tla`: syncing a worktree into the store (`Trees::sync`,
  `Trees::upload`, `Trees::hold`, `Store::pin_all`, `Store::evict`) with
  shared uploads, pins, eviction and cancelled requests. Checks that what a
  build holds is in the store, and that every sync ends.
  - `StoreSync_TRUE.cfg`: the code as it is. No error.
  - `StoreSync_FALSE.cfg`: without releasing claims on cancel. TLC shows a
    sync that waits forever (the test
    `a_cancelled_upload_does_not_block_others` is that trace).

Run with `dev/tla/run.sh StoreSync StoreSync_TRUE.cfg` (needs `java`;
`tla2tools.jar` from https://github.com/tlaplus/tlaplus/releases at
`$TLA2TOOLS` or `~/.local/share/tla/tla2tools.jar`). For a larger model, use
`Req = {r1, r2, r3}`, `Blob = {b1, b2}` and `Attempts = 3`: about 4 million
states in under two minutes.
