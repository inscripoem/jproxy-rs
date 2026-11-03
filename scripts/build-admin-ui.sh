#!/usr/bin/env bash
# Rebuild admin_dist/ from the pinned upstream jproxy-admin source and apply
# the local branding patch. See admin-ui/README.md for the source pin evidence.
#
# Usage:
#   scripts/build-admin-ui.sh
#
# Run from the repository root. The script always re-executes itself through
# `devenv shell`, so the Node/pnpm versions come from devenv.nix rather than
# the caller's global toolchain.
set -euo pipefail

UPSTREAM_REPO="https://github.com/LuckyPuppy514/jproxy-admin.git"
UPSTREAM_COMMIT="47ed76e943152d831c134144881d99d6b8a9b2bf"

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

# Always use the devenv toolchain, even when a global pnpm is on PATH.
if [[ "${ADMIN_UI_IN_DEVENV:-}" != "1" && -z "${DEVENV_ROOT:-}" ]]; then
  if ! command -v devenv >/dev/null 2>&1; then
    echo "error: run this script inside 'devenv shell' (devenv not installed)" >&2
    exit 1
  fi
  ADMIN_UI_IN_DEVENV=1 exec devenv shell -- bash "$0" "$@"
fi

if ! command -v pnpm >/dev/null 2>&1; then
  echo "error: pnpm is missing inside the devenv shell" >&2
  exit 1
fi
if [[ "$(pnpm --version)" != 8.* ]]; then
  echo "error: pnpm 8 is required (found $(pnpm --version)); run inside devenv" >&2
  exit 1
fi

SRC_DIR="${ADMIN_UI_SRC:-$ROOT_DIR/.work/admin-ui-src}"
PATCH_DIR="$ROOT_DIR/admin-ui/patches"

if [[ ! -d "$SRC_DIR/.git" ]]; then
  rm -rf "$SRC_DIR"
  mkdir -p "$(dirname "$SRC_DIR")"
  git clone --no-checkout "$UPSTREAM_REPO" "$SRC_DIR"
fi

git -C "$SRC_DIR" remote set-url origin "$UPSTREAM_REPO"
git -C "$SRC_DIR" fetch --depth=1 origin "$UPSTREAM_COMMIT"
git -C "$SRC_DIR" checkout --force --detach FETCH_HEAD
git -C "$SRC_DIR" reset --hard --quiet FETCH_HEAD
git -C "$SRC_DIR" clean -fd --quiet

for patch in "$PATCH_DIR"/*.patch; do
  [[ -e "$patch" ]] || continue
  git -C "$SRC_DIR" apply --check "$patch"
  git -C "$SRC_DIR" apply "$patch"
  echo "applied $(basename "$patch")"
done

# pnpm's strict node_modules layout hides transitive @types/node from vue-tsc.
# The upstream project never declared it directly; npm/yarn hoisting provided
# it. --shamefully-hoist restores that layout without changing versions.
(
  cd "$SRC_DIR"
  HUSKY=0 pnpm install --shamefully-hoist \
    --config.confirmModulesPurge=false --reporter=append-only
  pnpm build
)

test -f "$SRC_DIR/dist/index.html"
grep -q '<title>jproxy-rs Admin</title>' "$SRC_DIR/dist/index.html"

rm -rf "$ROOT_DIR/admin_dist/assets"
cp -R "$SRC_DIR/dist/assets" "$ROOT_DIR/admin_dist/assets"
cp "$SRC_DIR/dist/index.html" "$ROOT_DIR/admin_dist/index.html"

# vite-plugin-compression emits gzip headers that depend on the host zlib.
# Recompress with a fixed header (and the plugin's 1 KiB threshold) so a
# rebuild of unchanged sources produces identical .gz files.
find "$ROOT_DIR/admin_dist/assets" -type f \
  \( -name '*.js' -o -name '*.css' \) -size +1023c -print0 |
  while IFS= read -r -d '' file; do
    gzip -9 -n -c "$file" > "$file.gz"
  done

echo "admin_dist rebuilt from ${UPSTREAM_COMMIT:0:7}"
echo "  assets: $(find "$ROOT_DIR/admin_dist/assets" -type f | wc -l) files"
