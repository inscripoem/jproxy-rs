# Admin UI build pipeline

`admin_dist/` is rebuilt from the MIT-licensed upstream
[`LuckyPuppy514/jproxy-admin`](https://github.com/LuckyPuppy514/jproxy-admin)
Vue source with a small branding patch applied. The upstream license is kept
in `admin_dist/LICENSE.txt`; `admin_dist/NOTICE` records the derivation.

## Pinned source

- Repository: `https://github.com/LuckyPuppy514/jproxy-admin`
- Commit: `47ed76e943152d831c134144881d99d6b8a9b2bf`
  (2025-06-24, "新增登录关闭时自动登录匿名用户")
- This is the latest public commit. The Java v3.4.7 release (2025-07-18)
  ships a dist built after it; no newer public source commit exists.

## Pin verification

Building `47ed76e` with the upstream `pnpm-lock.yaml` reproduces the shipped
`admin_dist` content. Vite content hashes and Vue scoped-style IDs differ
because the release build used a slightly different dependency resolution,
but after normalizing both hash classes:

- `index.<hash>.js`, `default-layout.<hash>.js`, `index.<hash>.css`,
  `vue.<hash>.js` and `arco.<hash>.js` are byte-identical to the shipped
  v3.4.7 assets.
- 54 of the 61 shipped assets match exactly. The remaining 7 are the
  `crypto` chunk, four route-level `index` chunks (minifier output only;
  string literals and algorithm coverage match), and two logo PNGs
  (imagemin output).
- The source contains the exact branding strings present in the shipped
  dist: `JProxy Admin` in `index.html` and both login locales, `JProxy` in
  the navbar, and `© 2023 LuckyPuppy514` in the footer.

The generated `index.html` asset filenames do not match the shipped dist
byte-for-byte (`index.259ad068.js` vs the rebuilt `index.<hash>.js`), which
is expected from the hash differences above.

## Toolchain

`devenv.nix` provides Node 18 and pnpm 8 from the `nixpkgs-ui` input
(nixos-24.05). pnpm 8 migrates the upstream lockfile (version 5.4) while
preserving its resolved versions; pnpm 9+ re-resolves the tree and bumps
Vue from 3.2 to 3.5. `pnpm install --shamefully-hoist` is required because
`vue-tsc` needs the transitive `@types/node` that upstream never declared
directly.

## Rebuild

```sh
scripts/build-admin-ui.sh
```

The script clones or resets `.work/admin-ui-src` at the pinned commit,
applies `admin-ui/patches/*.patch`, installs, builds, and replaces
`admin_dist/index.html` and `admin_dist/assets/`. `admin_dist/LICENSE.txt`
and `admin_dist/NOTICE` are preserved.

## Branding changes

`admin-ui/patches/0001-branding.patch`:

- `<title>` and login titles become `jproxy-rs Admin` / `jproxy-rs 管理后台`.
- Navbar wordmark becomes `jproxy-rs`; layout, logo and version link are
  unchanged.
- Footer links to the jproxy-rs repository and keeps the upstream
  `JProxy © 2023 LuckyPuppy514` attribution link.
- The login page adds the non-affiliation disclaimer.

The layout, components, interaction and upstream logo are otherwise
untouched.
