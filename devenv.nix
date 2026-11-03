{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

let
  # The admin UI snapshot predates the current toolchain. nixos-24.05 keeps
  # Node 18 and pnpm 8 available; pnpm 8 migrates the upstream 5.4 lockfile
  # while preserving its resolved versions, and unlike pnpm 10 it still runs
  # dependency lifecycle scripts.
  uiPkgs = inputs.nixpkgs-ui.legacyPackages.${pkgs.system};
in
{
  languages.javascript = {
    enable = true;
    package = uiPkgs.nodejs_18;
    pnpm = {
      enable = true;
      package = uiPkgs.pnpm_8;
    };
  };

  packages = [
    pkgs.git
    pkgs.gzip
  ];
}
