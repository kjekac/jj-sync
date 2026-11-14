{
  description = "Synchronize mutable Jujutsu work between development hosts";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";

  outputs = { self, nixpkgs }: let
    systems = [ "x86_64-linux" "aarch64-linux" "aarch64-darwin" "x86_64-darwin" ];
    eachSystem = nixpkgs.lib.genAttrs systems;
  in {
    packages = eachSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
      package = pkgs.callPackage ./jj-sync.nix { };
    in { default = package; jj-sync = package; } // pkgs.lib.optionalAttrs pkgs.stdenv.hostPlatform.isDarwin {
      native-cycle-check = pkgs.writeShellScriptBin "native-cycle-check" ''
        exec ${pkgs.python3}/bin/python3 ${./tests/native-cycle.py} ${package}/bin/jj-sync "$@"
      '';
    });
    checks = eachSystem (system: { inherit (self.packages.${system}) jj-sync; });
    devShells = eachSystem (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in { default = pkgs.mkShell {
      packages = [ pkgs.cargo pkgs.rustc pkgs.rustfmt pkgs.clippy pkgs.jujutsu pkgs.gitMinimal pkgs.openssh pkgs.python3 pkgs.nushell ];
    }; });
  };
}
