{
  description = "Rargo development shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  inputs.rust-overlay.url = "github:oxalica/rust-overlay";
  inputs.rust-overlay.inputs.nixpkgs.follows = "nixpkgs";

  outputs = { nixpkgs, rust-overlay, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
    in {
      devShells = nixpkgs.lib.genAttrs systems (system:
        let
          pkgs = import nixpkgs { inherit system; overlays = [ rust-overlay.overlays.default ]; };
          muslTarget = "x86_64-unknown-linux-musl";
          muslCC = pkgs.pkgsMusl.stdenv.cc;
          rust = pkgs.rust-bin.stable."1.97.1".default.override {
            targets = pkgs.lib.optional (system == "x86_64-linux") muslTarget;
          };
        in {
          default = pkgs.mkShell ({
            RUSTFLAGS = "-Dwarnings";
            packages = with pkgs; [
              rust gcc gnumake cmake pkg-config perl python3 git openssl zlib sqlite
            ];
          } // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
            CC_x86_64_unknown_linux_musl = "${muslCC}/bin/${muslCC.targetPrefix}cc";
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = "${muslCC}/bin/${muslCC.targetPrefix}cc";
          });
        });
    };
}
