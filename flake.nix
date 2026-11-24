{
  description = "Rargo build and test environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
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
      rust = pkgs.rust-bin.stable.latest.default.override {
        targets = [ "x86_64-unknown-linux-musl" ];
      };
      muslCC = pkgs.pkgsCross.musl64.stdenv.cc;
      muslLinker = "${muslCC}/bin/${muslCC.targetPrefix}cc";
    in {
      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [ rust gcc cmake gnumake pkg-config perl git ];
        buildInputs = with pkgs; [ openssl zlib ];
        CC_x86_64_unknown_linux_musl = muslLinker;
        CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER = muslLinker;
      };
    };
}
