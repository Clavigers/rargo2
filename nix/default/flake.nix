{
  description = "rargo's default system build dependencies";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [ gcc binutils pkg-config cmake gnumake ];
        buildInputs = with pkgs; [ openssl zlib sqlite ];
      };
    };
}
