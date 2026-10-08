{
  description = "Tusk database client for Nix and NixOS";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      tusk = pkgs.callPackage ./nix/package.nix { };
    in
    {
      packages.${system} = {
        inherit tusk;
        default = tusk;
      };

      apps.${system}.default = {
        type = "app";
        program = "${tusk}/bin/tusk";
        meta.description = tusk.meta.description;
      };

      # Nixpkgs already has an unrelated Evernote client named "tusk".
      overlays.default = final: _prev: {
        tusk-database = final.callPackage ./nix/package.nix { };
      };

      checks.${system} = {
        package = tusk;
        runtime = pkgs.callPackage ./nix/check.nix { inherit tusk; };
        formatting = pkgs.runCommand "tusk-nix-formatting" { nativeBuildInputs = [ pkgs.nixfmt ]; } ''
          nixfmt --check ${./flake.nix} ${./nix/package.nix} ${./nix/check.nix}
          touch "$out"
        '';
      };

      formatter.${system} = pkgs.nixfmt-tree;
    };
}
