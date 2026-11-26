---
title: Rargo
subtitle: Remote Cargo, a fast hermetic way to build and test Rust code
small_subtitle: '`{`Fast, Correct, Cargo`}` — choose 3'
author: '[William Murray](https://murraypedia.com/index.php/William_Murray)'
lang: en
toc-title: Contents
---

# Why (In 95 Words)
**Warning! Rargo is in the middle of an in-progress rewrite. This site is under construction**


Rargo is remote Cargo. You might want Rargo if you are facing slow or un&shy;reliable builds. 

Offload compi&shy;lation to a build farm. Share a cache across a team. Se&shy;lectively run tests affected by a change. Gain repro&shy;ducibility without changing your code.

It's a worn out trope, but Rargo fixes the "works on my ma&shy;chine" problem. Rejoice, there is no mi&shy;gration to do. Rargo is Cargo com&shy;patible; you can always go back to building locally.

# Getting Started

The  <code>rargo CLI</code> is the official Rargo client. All <code>cargo</code> commands work with <code>rargo</code>.

<cmd>cargo install rargo</cmd>

To try Rargo, point your client at the public farm. To con&shy;figure rargo to run builds on the public build farm, run this command:  

<aside>
The *Farm* is a Hetzner AX102 in Hel&shy;sinki. It's a nice ma&shy;chine but it's just *one* ma&shy;chine. Please bear in mind, I pay for this out of pocket. If you queue a&shy;lot of builds I might rate limit you.
</aside>

<cmd>rargo @rargo.rs</cmd>

That will write your server to `~/.config/rargo/config.toml` (under `$XDG_CONFIG_HOME` if you set it), beside the tokens `rargo auth` keeps. It is your setting, not the project's: every Rust project on the machine builds there, fresh clones included, and no file in the project changes.

<figure class="example">
```toml
host = "rargo.rs"
```
</figure>

Alter&shy;natively, you can set this environ&shy;ment vari&shy;able, which comes first:

<cmd>export RARGO_SERVER=rargo.rs</cmd>

Now, the rargo client knows which rargo server it should send actions requests to, but that it is not enough. The client must authenti&shy;cate with the server before it can send requests.

<cmd>rargo auth</cmd>

<aside>
The Farm lets in the people its owner has let in. `rargo auth` opens a page that asks you to prove you are one of them. If you need access, [open a request on GitHub](https://github.com/sadosystems/rargo/issues/new?title=Request%20access%20to%20the%20Rargo%20Farm) with your GitHub username. You can also skip the Farm: [a server of your own](/reference#a-server) is one command, and `rargo @HOST` puts it in the same user config file.
</aside>

Once you are authenti&shy;cated with the server, you can run any cargo command with rargo, the work will be done on the public farm and the result will be streamed back.

<cmd-list>
<cmd>rargo build</cmd>
<cmd>rargo test</cmd>
<cmd>rargo run -\- -\-help</cmd>
</cmd-list>

<aside>
What the Farm builds links against the Farm's own C library, from the Nix store. To run it on your machine, with `rargo run` or straight from `target/`, you need [Nix](https://nixos.org/download/) installed. The first `rargo run` fetches the library into your store; after that the program runs either way.
</aside>


# Getting Just a Bit More Started
TODO: add more of a guide

example of the code block formatting.
<figure class="example">
```css
:root {
  --font-family: "Alegreya", serif;
  --line-height: 1.2;
  --border-thickness: 1.5px;
  --text-color: #000;
  --background-color: #fff;
}
```
</figure>

# Rargo vs Other Solutions
TODO: Write the comparison
<!-- "Why not use X?" if X is in the biblio&shy;graphy I tried it and I walked away still wanting some&shy;thing else. There are many suitable so&shy;lutions to this problem Rargo is the one that works best for Rust work&shy;spaces. -->

# Tough Topics
TODO: Write about sharp corners / hermeticity stuff

# Things are Gonna Get Easier
TODO: Write about ways to prevent non determinism

# SolidGoldFerris
TODO: Write about ways to detect and fix non determinism

# Afterword {.unnumbered}
<!-- ## Thank You Reader {.unnumbered}
If you read this, I sin&shy;cerely thank you. Rargo was person-produced. I sat for a long time thinking and typing. -->


## Thank You Stranger {.unnumbered}
This site has a [biblio&shy;graphy page](https://rargo.rs/bibliography), I am grateful to those behind the works listed there. Below I have listed other sources for this site.

<figure class="example">
<hr />
</figure>

- [Oskar Wickström](https://wickstrom.tech/) [This site is a remix of Oskar Wickström's "the-proportional-web".]{.note}
- [Alex Kladov](https://matklad.github.io/) [I yoinked the code snippet css from Alex Kladov's site.]{.note}
- [David Jonathan Ross](https://djr.com/) [The top bar for this site is stolen from djr's site. The main font on this site, besides Jetbrains mono is djr's Fern which I gladly paid for.]{.note}

<footer>Copyright 2026 William Murray</footer>
