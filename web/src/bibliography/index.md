---
pagetitle: Bibliography
lang: en
---

I considered naming this page prior art. Some of these references contain ideas I fully ripped off. Some explore the same problems in different ways. I started this project with the goal of replacing Bazel + BuildBuddy for my use cases. Despite that, I landed on a design that is fundamentally unlike Bazel. As a result, this "bibliography" is a bit all over the place.<hr/>

- [Commandments of Reproducible Builds](https://reproducible-builds.org/docs/commandments/)
- [Reproducibility Testing for the Rust Toolchain](https://github.com/rust-lang/rust/pull/139793)
- [reprotest](https://salsa.debian.org/reproducible-builds/reprotest)
- [crunch](https://github.com/liamaharon/crunch) [Crunch is the most keep-it-simple-stupid style solution to this problem. It's basically rsync sources to a remote and run cargo over SSH. I actually used crunch for quite a while and I hacked in some changes to make it more suitable to me.]{.note}
- [Fast Rust Builds](https://matklad.github.io/2021/09/04/fast-rust-builds.html) [I read everything Alex Kladov writes.]{.note}
- [The Cargo Book: Build Performance](https://doc.rust-lang.org/cargo/guide/build-performance.html)
- [Build Systems à la Carte](https://www.microsoft.com/en-us/research/uploads/prod/2018/03/build-systems.pdf) [This is a "must-read" paper on build systems, so I read it. The closest thing to Rargo in this paper is Tup... I guess? It is not a great match.]{.note}
- [sccache](https://github.com/mozilla/sccache)
- [sccache is pretty okay](https://brokenco.de/2025/08/25/sccache-is-pretty-okay.html)
- [Benchmarking Rust Compilation Speedups and Slowdowns from sccache](https://neosmart.net/blog/benchmarking-rust-compilation-speedups-and-slowdowns-from-sccache-and-zthreads/) [I have used sccache, and I noticed for my own purposes that it was sometimes faster sometimes slower and sometimes flaky. I spent a weekend reading through the code path that gets touched when using sccache to cache Rust compilation with the local disk cache. My conclusion from this exercise was that sccache could be a lot better for Rust if it had been designed for Rust and not as a more general tool. I intend to write more about this.]{.note}
- [Bazel Basics: Hermeticity](https://bazel.build/basics/hermeticity)
- [Persistent Workers](https://bazel.build/versions/7.0.0/remote/persistent)
- [rules_rs](https://github.com/hermeticbuild/rules_rs)
- [Scaling Rust Builds with Bazel](https://mmapped.blog/posts/17-scaling-rust-builds-with-bazel)
- [Bazel linux-sandbox and cgroups](https://fzakaria.com/2025/05/14/bazel-linux-sandbox-and-cgroups)
- [Hermetic, but at What Cost?](https://fzakaria.com/2024/02/27/hermetic-but-at-what-cost)
- [Incremental Rust Compilation via Bazel-Managed Tree Artifacts](https://github.com/bazelbuild/rules_rust/pull/3925)
- [Bazel's Original Sins](https://fzakaria.com/2025/06/22/bazel-s-original-sins)
- [The Next Generation of Bazel Builds](https://blogsystem5.substack.com/p/bazel-next-generation) [The world has no shortage of Bazel haters. Guess what? I am not one of them! Bazel is very good. For a multi-language monorepo, there is no better tool. But if you can avoid it you should. Sticking to one language keeps complexity way down. My aspiration with Rargo is to bring the up-sides of Bazel to projects that decide to stick to Rust as their one language.]{.note}
- [Sapling](https://github.com/facebook/sapling)
- [EdenFS](https://github.com/facebook/sapling/blob/main/eden/fs/docs/Overview.md)
- [fractal-fuse](https://github.com/fractalbits-labs/fractalbits-main/tree/main/crates/fs_server/fractal-fuse)  [How I learned about FUSE-over-io_uring]{.note}
- [Whatever Happened to sandboxfs?](https://blogsystem5.substack.com/p/whatever-happened-to-sandboxfs) [At some point I thought hey maybe Rargo should be a FUSE thing at least on the build farm side]{.note}
- [2025 Recap: vixen](https://fasterthanli.me/articles/2025-recap#vixen)
- [Modern CI Is Too Complex and Misdirected](https://gregoryszorc.com/blog/2021/04/07/modern-ci-is-too-complex-and-misdirected/)
- [I Want a Better Build Executor](https://jyn.dev/i-want-a-better-build-executor/)
- [The Postmodern Build System](https://jade.fyi/blog/the-postmodern-build-system/)
- [cargo-xtask](https://github.com/matklad/cargo-xtask)
- [Remote Cache CDC: Reusing Bytes](https://www.buildbuddy.io/blog/content-defined-chunking)
- [Bonanza](https://github.com/buildbarn/bonanza)
- [BuildBuddy Remote Bazel](https://www.buildbuddy.io/docs/remote-bazel/)
- [How Bonanza Encodes Trees of Files into DAGs of Objects](https://github.com/buildbarn/bonanza/blob/main/docs/filesystem_merkletree.md)[BuildBuddy's "Remote Bazel" and Ed Schouten's Bonanza converged on the idea that analysis should largely happen on the build server. I agree with this conclusion, and that informed a lot of the design for Rargo, especially the ways the protocol differs from REAPI. I have a deep fondness for Bonanza.]{.note}
- [Maelstrom](https://github.com/maelstrom-software/maelstrom)[There's much to admire in this project which sadly died. Before I started working on Rargo I couldn't really answer the question, "What *is* a container?" Reading through the very nice container runtime implementation in cargo-maelstrom was educational for me.]{.note}
- [cargo-nextest](https://nexte.st/) 
- [Why Nextest is Process per Test](https://sunshowers.io/posts/nextest-process-per-test/)
<!-- - [How (and Why) Nextest uses Tokio](https://sunshowers.io/posts/nextest-and-tokio/) [Rain Paharia is an engineer I deeply admire. I learned a lot about cargo from her code and prose.]{.note} -->
- [hurry](https://github.com/attunehq/hurry) [The hurry team was pursuing this mostly from the angle of a better sccache. After they decided to pivot, they were kind enough to get on a call with me and talk through some of the challenges they faced. Surely the conversation was a waste of their time but it was helpful to me so I am grateful.]{.note}
- [cargo2nix](https://github.com/cargo2nix/cargo2nix)
- [garnix](https://github.com/garnix-io/garnix-ci)
- [crane](https://github.com/ipetkov/crane)
- [Snix](https://snix.dev/)
<!-- [My currently-very-hacky server side implementation of Rargo is a now-grotesquely-mutated Snix fork. The Snix castore is very nice.]{.note} -->
- [cross](https://github.com/cross-rs/cross) [cross-rs has Docker images for building targeting various platforms.]{.note}
- [cargo-chef](https://github.com/LukeMathWalker/cargo-chef)
- [sccache-docker-test](https://github.com/benmarten/sccache-docker-test)
- [Better support of Docker layer caching in Cargo](https://hackmd.io/@kobzol/S17NS71bh)
- [Faster Rust Builds with DOCKER_HOST](https://fenollp.github.io/faster-rust-builds-docker_host)
- [supergreen](https://github.com/fenollp/supergreen) [My least loved trope in software marketing is: `f"{product} is the LLVM IR of {category}"`. People say BuildKit's LLB is like the LLVM IR of remote build systems. On a purely pattern matching basis I wondered, "would BuildKit / LLB based approach make sense for Rargo?" After a bit of googling I found that it had already been done! It's called supergreen. I think I 60% understand it.]{.note}
- [Kache](https://github.com/kunobi-ninja/kache) [I think this is another "modern sccache replacement". I have not gotten a chance to thoroughly check this out.]{.note}
<!-- - [ClearCase / clearmake](https://news.ycombinator.com/item?id=36149537) [I learned about ClearCase and clearmake from this h*******s thread. The very next day I learned that we use clearmake at my day job but I have never had to touch it. If you squint a shared build cache and a VCS with checked in build artifacts is the same thing, (if the build process is pure and hermetic). This makes me feel strangely disoriented. It also uses a form of trace based input tracking. So... basically the same idea in Rargo. I might have called it ClearCargo if I wanted people to be maximally biased against it in an accurate way.]{.note} -->