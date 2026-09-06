{
  description = "Private tailnet URLs for local development servers";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      version = self.shortRev or self.dirtyShortRev or "dev";
      packageFor =
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        pkgs.callPackage ./nix/package.nix { inherit version; };
    in
    {
      packages = forAllSystems (system: {
        default = packageFor system;
        poros = packageFor system;
      });

      apps = forAllSystems (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/poros";
          meta.description = "Expose a localhost development server over Tailscale";
        };
      });

      checks = forAllSystems (system: {
        default = self.packages.${system}.default;
      });

      formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt-tree);

      overlays.default = final: _previous: {
        poros = final.callPackage ./nix/package.nix { inherit version; };
      };

      nixosModules.default =
        { pkgs, ... }:
        {
          environment.systemPackages = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
        };

      darwinModules.default = self.nixosModules.default;

      homeManagerModules.default =
        { pkgs, ... }:
        {
          home.packages = [ self.packages.${pkgs.stdenv.hostPlatform.system}.default ];
        };
    };
}
