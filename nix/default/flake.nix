{
  description = "Rargo's default system build dependencies";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      toolchainFile =
        if builtins.pathExists ./rust-toolchain.toml then ./rust-toolchain.toml
        else if builtins.pathExists ./rust-toolchain then ./rust-toolchain
        else null;
      toolchainText = if toolchainFile == null then "" else builtins.readFile toolchainFile;
      legacyChannel = builtins.match "([^\r\n]+)\r?\n?" toolchainText;
      toolchain =
        if toolchainFile == null then { channel = "nightly-2026-11-22"; }
        else if legacyChannel != null then { channel = builtins.head legacyChannel; }
        else (builtins.fromTOML toolchainText).toolchain;
      rust = pkgs.rust-bin.fromRustupToolchain (toolchain // {
        components = pkgs.lib.unique ((toolchain.components or []) ++ [ "clippy" "rustfmt" "rust-src" ]);
      });
      muslTarget = "x86_64-unknown-linux-musl";
      muslRust = pkgs.rust-bin.fromRustupToolchain (toolchain // {
        targets = [ muslTarget ];
      });
      muslCc = pkgs.pkgsCross.musl64.stdenv.cc;
      tools = with pkgs; [ gcc binutils pkg-config cmake gnumake perl python3 git ];
      libs = with pkgs; [
        openssl zlib sqlite
        udev alsa-lib wayland libxkbcommon vulkan-loader fontconfig freetype
        libx11 libxcursor libxi libxrandr libxext libxrender libxfixes libxcb
      ];
    in {
      devShells.${system} = {
        system = pkgs.mkShell {
          nativeBuildInputs = tools;
          buildInputs = libs;
        };
        default = pkgs.mkShell {
          # Native debug builds can run compiler probes with -O0 and -Werror.
          hardeningDisable = [ "fortify" ];
          nativeBuildInputs = [ rust ] ++ tools;
          buildInputs = libs;
        };
        install = pkgs.mkShell {
          nativeBuildInputs = [ muslRust muslCc ] ++ tools;
          CC_x86_64_unknown_linux_musl = "${muslCc}/bin/${muslCc.targetPrefix}cc";
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${muslCc}/bin/${muslCc.targetPrefix}cc";
        };
      };
    };
}
