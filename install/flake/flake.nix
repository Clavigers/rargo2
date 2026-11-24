# The default flake for a project built with rargo.
#
# rargo builds a project inside the dev shell of its flake: what the shell
# has is what the build sees, on every server. Copy this file and its
# flake.lock to the top of your repository and commit both. Need another
# library? Add it to `packages` below.
{
  description = "Build environment (rargo's default: the build essentials)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in {
      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # Rust
            rustc cargo clippy rustfmt
            # C and C++, and what build scripts call
            gcc gnumake cmake pkg-config perl python3 git
            # Libraries Rust projects most often link
            openssl zlib sqlite
          ];
        };
      });
    };
}
