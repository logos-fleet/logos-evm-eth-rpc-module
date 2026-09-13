{
  description = "Logos eth_rpc module — proxyable, fail-closed Ethereum JSON-RPC client (per-chain config, socks5h/Tor-ready).";

  inputs = {
    logos-module-builder.url = "github:logos-co/logos-module-builder";
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;

      # ONE module, answered for every target at once. mkLogosModule already
      # keys its own outputs by system, so calling it per system built four
      # copies of the same evaluation and threw three away.
      module = logos-module-builder.lib.mkLogosModule {
        src = ./.;
        configFile = ./metadata.json;
        flakeInputs = inputs;
      };

      # The mobile pseudo-systems logos-nix keys its cross package sets by. Kept
      # out of `systems` above for the reason the builder keeps them out of its
      # own: a phone gets the Bare image and none of the other outputs. `?
      # ${t}` rather than a bare index, so a logos-module-builder pin without
      # the mobile cross sets leaves this flake simply WITHOUT mobile keys
      # instead of failing to evaluate.
      #
      # THIS IS WHAT MAKES eth_rpc BUNDLABLE (slice 30, criterion 2). A phone's
      # Bundled set is resolved out of a catalog whose every entry is a module's
      # own `mobile.<target>.bare`, so a module with no mobile output cannot be
      # in that set however well it builds on a desktop -- and the wallet UI's
      # `web` variant has nothing to fetch a balance from until it is.
      mobileTargets = builtins.filter (t: module.packages ? ${t})
        [ "aarch64-ios" "aarch64-ios-simulator" "aarch64-android" ];
    in
    {
      packages = forAllSystems (system: module.packages.${system})
              // nixpkgs.lib.genAttrs mobileTargets (t: module.packages.${t});

      # An Android cross derivation's `system` is its BUILD platform, so
      # `packages.aarch64-android` is pinned to the builder's canonical one
      # (x86_64-linux) and a Mac cannot realise it. The same artifact, reached
      # from whichever machine is doing the building:
      #   nix build .#legacyPackages.aarch64-darwin.mobile.aarch64-android.bare
      legacyPackages = module.legacyPackages or { };
    };
}
