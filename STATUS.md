# Status

## Nix store trees

A cold build is a fresh one-core server and a fresh worker, building a five-crate workspace, with the compiles that do not fit on the server sent to the worker. Runs alternate between the server without store-path manifests and the current server.

Without the manifests: 11.87s, 12.37s, 12.15s (median 12.15s). Nix store lookups 505, 501 and 502, with 3 actions dispatched each time.

With the manifests: 11.65s, 8.53s, 12.38s (median 11.65s). Nix store lookups 500, 332 and 504, with 3, 2 and 3 actions dispatched. The 8.53s run dispatched two actions. Every run served the files the worker asked for.

Hashing the closure is off that path. On the one-core server it starts once the build has released the core: 87,426 files, 2,505,913,120 bytes, in 3.02s, 3.09s and 3.10s.

On the worker suite, an action after the worker holds the closure adds no nix store lookups: 321 then 321 on the wide worker, 107 then 107 on the one-core worker. The suite passed 35 checks. The same suite without the manifests asked for 803 lookups on the wide worker and 624 on the one-core worker, and took 23s.

A flake build on a worker with no Nix installed passes. The first build asked for 66 nix store lookups while the closure was still being hashed. The next build, after the hash, asked for none more (66 then 66).
