{
  description = "eek! lightweight LLM proxy";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs = {
    self,
    nixpkgs,
  }: let
    supportedSystems = [
      "x86_64-linux"
      "aarch64-linux"
    ];
    forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
  in {
    packages = forAllSystems (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
        eek = pkgs.rustPlatform.buildRustPackage {
          pname = "eek";
          version = "0.1.0";

          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
            ];
          };

          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "eek! lightweight LLM proxy";
            license = pkgs.lib.licenses.mit;
            mainProgram = "eek";
          };
        };
        image = pkgs.dockerTools.buildLayeredImage {
          name = "eek";
          tag = "latest";
          config.Cmd = [(pkgs.lib.getExe eek)];
          config.User = "65534:65534";
        };
      in {
        inherit eek image;
        default = eek;
      }
    );

    devShells = forAllSystems (
      system: let
        pkgs = nixpkgs.legacyPackages.${system};
      in {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
            cargo-audit
          ];

          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      }
    );

    overlays.default = final: _prev: {
      eek = self.packages.${final.system}.eek;
    };
  };
}
