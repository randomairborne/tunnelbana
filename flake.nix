{

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-darwin"
        "aarch64-darwin"
        "i686-freebsd"
        "x86_64-freebsd"
        "aarch64-freebsd"
        "aarch64-linux"
        "armv6l-linux"
        "armv7a-linux"
        "armv7l-linux"
        "i686-linux"
        "riscv32-linux"
        "riscv64-linux"
        "x86_64-linux"
        "aarch64-windows"
        "x86_64-windows"
        "i686-windows"
      ];
      forAllSystems =
        func:
        nixpkgs.lib.genAttrs systems (
          system:
          func (
            import nixpkgs {
              inherit system;
            }
          )
        );
    in
    {
      devShell = forAllSystems (
        pkgs:
        pkgs.mkShellNoCC {
          buildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            rust-analyzer
            clippy
          ];
        }
      );
      packages = forAllSystems (pkgs: {
        default = { };
      });
    };
}
