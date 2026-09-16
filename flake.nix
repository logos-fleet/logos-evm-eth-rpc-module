{
  description = "Logos eth_rpc module — proxyable, fail-closed Ethereum JSON-RPC client (per-chain config, socks5h/Tor-ready).";

  inputs = {
    # A rev on the logos-fleet fork, not logos-co: `"platform": true` in
    # metadata.json -- ADR 0009's declaration that this module owns access a
    # webview cannot give it -- is a key only THIS line of the builder knows.
    # logos-co's validator predates it and does not ignore it: it THROWS, telling
    # the author to rename it to `platforms` (plural), which is the near-miss
    # guard for an overlay list doing its job on a key that did not exist when it
    # was written.
    #
    # THIS PIN IS WHAT MAKES THE `config` PASSTHRU BELOW SAFE, and the order is
    # load-bearing (logos-workspace#207). A nix attribute that EXISTS BUT THROWS
    # cannot be caught -- logos-basecamp's catalog reads
    # `(module.config or { }).platform or false`, and `or` does not fall back
    # past a throw, it takes the whole consumer flake down. Publishing `config`
    # while this url still said logos-co would therefore have broken basecamp's
    # evaluation for everyone, and looked like an unrelated break.
    logos-module-builder.url = "github:logos-fleet/logos-module-builder/738f1a6ef5a6f755f8433297ac0d2ef54bba8d2f";

    # Declared OPTIONAL in metadata.json: typed, and tolerated when absent. Each contributes
    # its published `packages.<system>.lidl` -- verified_proxy_module's is a single
    # 19,928-byte contract.
    #
    # A REQUIRED declaration would not pull libverifproxy in either: measured, the closure holds
    # 0 nimbus paths and `.#install` stages only this module either way. `optional` is about
    # absence being tolerated, not about the closure.
    #
    # The follows is for LOCK SIZE, not compatibility: without it each dependency drags its own
    # module-builder subtree and this lock goes 756 -> 2250 nodes. The contract is unaffected
    # either way -- measured byte-identical with and without.
    modules_state = {
      url = "github:logos-co/logos-modules-state-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
    verified_proxy_module = {
      url = "github:logos-co/logos-verified-proxy-module";
      inputs.logos-module-builder.follows = "logos-module-builder";
    };
  };

  outputs = inputs@{ self, logos-module-builder, ... }:
    let
      nixpkgs = logos-module-builder.inputs.nixpkgs;
      systems = [ "aarch64-darwin" "x86_64-darwin" "aarch64-linux" "x86_64-linux" ];

      # x86_64-windows is a cross PSEUDO-SYSTEM the builder already understands
      # (logos-module-builder lib/common.nix routes it to
      # logos-nix.lib.mkWindowsPkgs, and picks the build platform separately).
      # It is a target, never a host we evaluate nixpkgs natively for, so it
      # only ever belongs in `packages`.
      targets = systems ++ [ "x86_64-windows" ];

      # ONE module, answered for every target at once. mkLogosModule already
      # keys its own outputs by system, so calling it per target built five
      # copies of the same evaluation and threw four away.
      module = logos-module-builder.lib.mkLogosModule {
        src = ./.;
        configFile = ./metadata.json;
        flakeInputs = inputs;
      };

      # The mobile pseudo-systems logos-nix keys its cross package sets by. Kept
      # out of `targets` above for the reason the builder keeps them out of its
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
      packages = nixpkgs.lib.genAttrs (targets ++ mobileTargets)
        (target: module.packages.${target});

      # An Android cross derivation's `system` is its BUILD platform, so
      # `packages.aarch64-android` is pinned to the builder's canonical one
      # (x86_64-linux) and a Mac cannot realise it. The same artifact, reached
      # from whichever machine is doing the building:
      #   nix build .#legacyPackages.aarch64-darwin.mobile.aarch64-android.bare
      legacyPackages = module.legacyPackages or { };

      # THE MODULE'S OWN ANSWER ABOUT ITSELF, forwarded so a consumer flake can
      # read it without building anything -- the shape token_list_module and
      # uniswap_module already publish, and the half of ADR 0009 that was
      # missing here (logos-workspace#207).
      #
      # `"platform": true` in metadata.json says this module owns access a
      # webview cannot give it, and the DECLARATION alone does nothing: the
      # thing that acts on it lives in another repo. logos-basecamp's mobile
      # catalog marks a Platform entry with
      # `(module.config or { }).platform or false` (flake.nix, mkBareSpec), and
      # each shell's Platform FLOOR is derived from that index and its own
      # Bundled closure (nix/platform-floor.nix). With no `config` here the
      # `or false` fallback won, silently: eth_rpc read as an ordinary module,
      # and a Downloaded module that depends on it -- the wallet UI's `web`
      # variant is exactly that shape -- was offered for install on a shell
      # that never bundled it and would have died at its first `eth_call`.
      #
      # `configFor` is the per-target resolution of the same document; this
      # module has no `platforms` overlay, so the two agree everywhere.
      inherit (module) config configFor;
    };
}
