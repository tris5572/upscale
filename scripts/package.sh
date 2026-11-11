#!/bin/bash
set -euo pipefail

if [[ "$(uname -s)/$(uname -m)" != Darwin/arm64 ]]; then
    echo "Packaging requires macOS arm64" >&2
    exit 1
fi
if (( $# > 1 )); then
    echo "Usage: $0 [official-upstream.zip]" >&2
    exit 2
fi
command -v jq >/dev/null || { echo "Packaging requires jq" >&2; exit 1; }
archive=${1:-}
if [[ -n "$archive" && "$archive" != /* ]]; then
    archive="$PWD/$archive"
fi
cd -P "$(dirname "$0")/.."

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
manifest=resources/manifest.json
source_url=$(jq -er '.upstream_archive.source' "$manifest")
source_sha=$(jq -er '.upstream_archive.sha256' "$manifest")
app_version=$(jq -er '.app_version' "$manifest")
if [[ -z "$archive" ]]; then
    archive="$work/upstream.zip"
    curl -fLsS --retry 2 "$source_url" -o "$archive"
fi
printf '%s  %s\n' "$source_sha" "$archive" | shasum -a 256 -c - >&2

mkdir -p dist
stage="$work/upscale-macos-arm64"
mkdir -p "$stage"/{bin,libexec,lib,models,licenses,diagnostics}
unzip -q "$archive" 'realesrgan-ncnn-vulkan' 'models/realesrgan-x4plus*' -d "$work/upstream"
lipo -thin arm64 "$work/upstream/realesrgan-ncnn-vulkan" -output "$stage/libexec/realesrgan-ncnn-vulkan"
chmod 755 "$stage/libexec/realesrgan-ncnn-vulkan"
cp "$work"/upstream/models/* "$stage/models/"
cp resources/diagnostics/*.png "$stage/diagnostics/"
cp licenses/*.txt "$stage/licenses/"
cp README.md LICENSE SECURITY.md VALIDATION.md "$stage/"

jq -r '.files[] | "\(.sha256)  \(.path)"' "$manifest" > "$work/runtime.sha256"
(cd "$stage" && shasum -a 256 -c "$work/runtime.sha256") >&2

target_dir="$PWD/target/package"
rust_sysroot="$(rustc --print sysroot)"
separator=$'\x1f'
build_flags=${CARGO_ENCODED_RUSTFLAGS:-}
if [[ -z "${CARGO_ENCODED_RUSTFLAGS+x}" && -n "${RUSTFLAGS:-}" ]]; then
    read -r -a rust_flags <<< "$RUSTFLAGS"
    printf -v build_flags '%s\x1f' "${rust_flags[@]}"
    build_flags=${build_flags%"$separator"}
fi
for remap in "$HOME=/home/build" "${CARGO_HOME:-$HOME/.cargo}=/cargo" "$rust_sysroot=/rust" "$PWD=/upscale"; do
    build_flags="${build_flags:+$build_flags$separator}--remap-path-prefix=$remap"
done
export CARGO_ENCODED_RUSTFLAGS="$build_flags"
MACOSX_DEPLOYMENT_TARGET=11.0 cargo build --release --locked --target aarch64-apple-darwin --target-dir "$target_dir"
install -m 755 "$target_dir/aarch64-apple-darwin/release/upscale" "$stage/bin/upscale"
[[ "$("$stage/bin/upscale" --version)" = "upscale $app_version" ]]

cargo metadata --locked --filter-platform aarch64-apple-darwin --format-version 1 > "$work/cargo.json"
jq -r '
    . as $metadata | .resolve.nodes[].id as $id
    | $metadata.packages[] | select(.id == $id and .source != null)
    | [.name, .version, .license, .manifest_path] | @tsv
' "$work/cargo.json" > "$work/crates.tsv"
mkdir "$stage/licenses/rust"
while IFS=$'\t' read -r name version license cargo_manifest; do
    dest="$stage/licenses/rust/$name-$version"
    mkdir "$dest"
    for notice in "$(dirname "$cargo_manifest")"/{LICENSE*,COPYING*,NOTICE*,COPYRIGHT*,UNLICENSE*}; do
        if [[ -f "$notice" ]]; then cp "$notice" "$dest/"; fi
    done
    printf '%s %s\t%s\thttps://crates.io/crates/%s/%s\n' "$name" "$version" "$license" "$name" "$version"
done < "$work/crates.tsv" > "$stage/licenses/Rust-dependencies.txt"

rust_docs="$rust_sysroot/share/doc/rust"
mkdir "$stage/licenses/rust-stdlib"
cp "$rust_docs"/COPYRIGHT-library.html "$stage/licenses/rust-stdlib/"
cp -R "$rust_docs"/licenses "$stage/licenses/rust-stdlib/"
rustc --version > "$stage/licenses/rust-stdlib/version.txt"

(cd "$stage" && find bin licenses README.md LICENSE SECURITY.md VALIDATION.md -type f -exec shasum -a 256 {} + | LC_ALL=C sort -k2) > "$work/distribution.sha256"
jq --rawfile checksums "$work/distribution.sha256" '
    .distribution_files = ($checksums | split("\n") | map(select(length > 0) | {
        path: .[66:],
        sha256: .[:64]
    }))
' "$manifest" > "$stage/manifest.json"

archive_name="upscale-$app_version-macos-arm64.tar.gz"
destination="$PWD/dist/$archive_name"
archive_tmp=$(mktemp "$PWD/dist/.upscale-package.XXXXXXXX")
trap 'rm -f "$archive_tmp"; rm -rf "$work"' EXIT
digest_tmp=$(mktemp "$PWD/dist/.upscale-checksum.XXXXXXXX")
trap 'rm -f "$archive_tmp" "$digest_tmp"; rm -rf "$work"' EXIT
COPYFILE_DISABLE=1 tar --uid 0 --gid 0 --uname root --gname wheel --no-xattrs \
    -czf "$archive_tmp" -C "$work" upscale-macos-arm64
digest=$(shasum -a 256 "$archive_tmp")
printf '%s  %s\n' "${digest%% *}" "$archive_name" > "$digest_tmp"
chmod 644 "$archive_tmp" "$digest_tmp"
mv -f "$archive_tmp" "$destination"
mv -f "$digest_tmp" "$destination.sha256"
printf '%s\n' "$destination"
