{
  description = "updatable-cli — reusable self-update plumbing for Rust CLIs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs =
    { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
      in
      {
        # Toolchain devshell for CI (and local dev) on Nix-based runners, where a
        # downloaded rustup toolchain would not run (NixOS linker/FHS mismatch).
        # The crate is pure-Rust (rustls TLS, miniz flate2), so no system libs.
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rustfmt
            clippy
          ];
        };
      }
    );
}
