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
        # Without a toolchain file: the nightly whose cargo the planner is
        # (forks/cargo/BASE), so what is planned is what that cargo plans.
        if toolchainFile == null then { channel = "nightly-2026-11-22"; }
        else if legacyChannel != null then { channel = builtins.head legacyChannel; }
        else (builtins.fromTOML toolchainText).toolchain;
      rust = pkgs.rust-bin.fromRustupToolchain (toolchain // {
        components = pkgs.lib.unique ((toolchain.components or []) ++ [ "clippy" "rustfmt" ]);
      });
      tools = with pkgs; [ gcc binutils pkg-config cmake gnumake perl python3 git ];
      libs = with pkgs; [ openssl zlib sqlite ];
    in {
      devShells.${system} = {
        system = pkgs.mkShell {
          nativeBuildInputs = tools;
          buildInputs = libs;
        };
        default = pkgs.mkShell {
          nativeBuildInputs = [ rust ] ++ tools;
          buildInputs = libs;
        };
      };
    };
}
