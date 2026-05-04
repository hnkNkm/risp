{
  description = "Rust development environment for rust-lisp";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rustToolchain = pkgs.rust-bin.stable.latest.default.override {
          extensions = [
            "rust-src"
            "rust-analyzer"
            "clippy"
            "rustfmt"
            "llvm-tools-preview"
          ];
        };
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustToolchain

            # Build tools
            pkg-config
            openssl

            # LLVM (for inkwell / llvm-sys)
            llvmPackages_18.llvm
            llvmPackages_18.libllvm
            libffi
            libxml2

            # Cargo extensions
            cargo-watch
            cargo-edit
            cargo-nextest
            cargo-audit
            cargo-expand

            # Debugger
            lldb
          ] ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.libiconv
          ];

          env = {
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
            RUST_BACKTRACE = "1";
            LLVM_SYS_181_PREFIX = "${pkgs.llvmPackages_18.llvm.dev}";
          };

          shellHook = ''
            echo "Rust dev shell ready"
            echo "  rustc:         $(rustc --version)"
            echo "  cargo:         $(cargo --version)"
            echo "  rust-analyzer: $(rust-analyzer --version)"
          '';
        };
      });
}
