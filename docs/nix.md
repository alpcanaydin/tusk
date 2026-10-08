# Tusk with Nix

The repository provides a flake for **x86_64 Linux**, including NixOS. Enable
`nix-command` and `flakes` in your Nix configuration if they are not already
enabled:

```ini
experimental-features = nix-command flakes
```

## Run or install

Try Tusk without installing it:

```sh
nix run github:alpcanaydin/tusk
```

Install it in your user profile, including the desktop launcher and icons:

```sh
nix profile add github:alpcanaydin/tusk#tusk
tusk
```

On older Nix versions, use `nix profile install` instead of `nix profile add`.
Update a profile installation with `nix profile upgrade tusk`.

The package uses the pinned, published Linux release, with its bundled
`postgres-language-server` and patched `sqls`. It patches the ELF interpreter
and library paths for Nix; no `nix-ld` or FHS environment is needed on NixOS.
PostgreSQL 18 client tools are included in the application's environment, so
backup and restore work without separately installing `pg_dump`, `pg_restore`
or `psql`. The package starts no PostgreSQL server.

The wrapper respects explicit `TUSK_PGLS`, `TUSK_SQLS` and `TUSK_PG_BIN`
overrides. Connection profiles, settings and passwords use the same locations
as other Linux installations.

## NixOS and Home Manager

Add Tusk to your configuration's flake inputs:

```nix
inputs.tusk.url = "github:alpcanaydin/tusk";
inputs.tusk.inputs.nixpkgs.follows = "nixpkgs";
```

Pass your flake inputs to the NixOS module via `specialArgs = { inherit inputs; };`,
then add the package:

```nix
{ inputs, pkgs, ... }:
{
  environment.systemPackages = [
    inputs.tusk.packages.${pkgs.stdenv.hostPlatform.system}.tusk
  ];
}
```

For Home Manager, pass the inputs via `extraSpecialArgs = { inherit inputs; };`
and use `home.packages` instead:

```nix
{ inputs, pkgs, ... }:
{
  home.packages = [
    inputs.tusk.packages.${pkgs.stdenv.hostPlatform.system}.tusk
  ];
}
```

Alternatively, apply `inputs.tusk.overlays.default` to your Nixpkgs instance
and install `pkgs.tusk-database`. The overlay uses this name because Nixpkgs
already has an unrelated Evernote application named `tusk`.

## Desktop requirements

Use a Wayland session with a Vulkan-capable graphics driver, as with the other
Linux packages. On NixOS, enable `hardware.graphics.enable = true;`. Saved
passwords need a running Secret Service provider, such as GNOME Keyring or a
compatible KWallet service. The package uses the desktop's existing provider.

On other Linux distributions, Nix applications may need
[nixGL](https://github.com/nix-community/nixGL) to access the host's graphics
drivers:

```sh
nix run --impure github:nix-community/nixGL -- nix run github:alpcanaydin/tusk
```

## Working on the Nix package

From a checkout:

```sh
nix build                  # package the pinned release into ./result
nix run                    # run that release
nix fmt                    # format the Nix files
nix flake check            # build, headless runtime checks, desktop entry, formatting
```

This flake packages a release; it does not compile the current Rust working
tree. Use the [Linux source build instructions](../README.md#linux-build) for
application development.

After a new release's Linux DEB has been published, update `version` and `src.hash`
in `nix/package.nix`. Get the new hash with:

```sh
nix store prefetch-file --json --hash-type sha256 \
  "https://github.com/alpcanaydin/tusk/releases/download/v<VERSION>/Tusk-<VERSION>-ubuntu-amd64.deb"
```

Run `nix flake check` after updating. Keep `flake.lock` committed; update its
Nixpkgs revision deliberately with `nix flake update nixpkgs`.
