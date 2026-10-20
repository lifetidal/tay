#!/usr/bin/env bash
# Build the current working tree, including uncommitted source changes.
set -euo pipefail
usage() {
    cat <<'HELP'
Usage: tools/package-arch.sh [--offline] [--syncdeps] [--skip-tests]
  --offline     Use cached Cargo dependencies only.
  --syncdeps    Allow makepkg to install missing build dependencies (sudo).
  --skip-tests  Skip the workspace test suite.
  --force       Overwrite an existing package with the same name.
Output: dist/arch/*.pkg.tar.zst. Does not install or start Tay.
Refuses to build when dist/arch already holds the same
version-release artifact unless --force is given (bump pkgrel
in packaging/arch/PKGBUILD to make a new release instead).
HELP
}
makepkg_args=()
force=false
for arg in "$@"; do
    case "$arg" in
        --force) force=true ;;
        --offline) export CARGO_NET_OFFLINE=true ;;
        --syncdeps) makepkg_args+=(--syncdeps) ;;
        --skip-tests) makepkg_args+=(--nocheck) ;;
        -h|--help) usage; exit 0 ;;
        --force) : ;;
        *) printf 'Unknown option: %s\n' "$arg" >&2; usage >&2; exit 2 ;;
    esac
done
if (( EUID == 0 )); then
    echo 'Run as a normal user: makepkg refuses to run as root.' >&2
    exit 1
fi
for command in makepkg tar sha256sum sed; do
    command -v "$command" >/dev/null || { echo "Missing command: $command" >&2; exit 1; }
done
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
output_dir="$repo_root/dist/arch"
mkdir -p "$output_dir"
version=$(sed -n 's/^version = "\([^"]*\)"/\1/p' "$repo_root/Cargo.toml")
pkgrel=$(sed -n 's/^pkgrel=//p' "$repo_root/packaging/arch/PKGBUILD")
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || { echo 'Unsupported workspace version' >&2; exit 1; }
# 默认拒绝覆盖同名产物:同版本号的包应 bump pkgrel 而非静默替换
# (曾因此覆盖掉已安装版本对应的 dist 制品,见 hardening-notes §13)。
if [[ "$force" != true ]] && compgen -G "$output_dir/tay-$version-$pkgrel-*.pkg.tar.zst" > /dev/null; then
    echo "产物已存在: tay-$version-$pkgrel(对应 dist/arch 的同名包)。" >&2
    echo "请把 packaging/arch/PKGBUILD 的 pkgrel 从 $pkgrel 加 1 再打包;" >&2
    echo "确认要覆盖旧制品再加 --force。" >&2
    exit 1
fi
if [[ "$force" == true ]]; then
    makepkg_args+=(--force)
fi
build_dir=$(mktemp -d "${TMPDIR:-/tmp}/tay-package.XXXXXXXX")
trap 'rm -rf -- "$build_dir"' EXIT
# Explicit inputs avoid packaging .git, target, user configuration or build output.
tar -czf "$build_dir/tay-source.tar.gz" --transform='s,^,tay/,' \
    -C "$repo_root" Cargo.toml Cargo.lock LICENSE README.md crates config packaging
cp "$repo_root/packaging/arch/PKGBUILD" "$build_dir/PKGBUILD"
digest=$(sha256sum "$build_dir/tay-source.tar.gz")
digest=${digest%% *}
sed -i -e "s/^pkgver=.*/pkgver=$version/" \
    -e "s/^sha256sums=.*/sha256sums=('$digest')/" "$build_dir/PKGBUILD"
export PKGDEST="$output_dir"
export PKGEXT='.pkg.tar.zst'
export CARGO_TARGET_DIR="$repo_root/target/arch-package"
cd "$build_dir"
makepkg "${makepkg_args[@]}"
echo 'Built package(s):'
makepkg --packagelist
