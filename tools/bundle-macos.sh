#!/usr/bin/env bash
#
# Build displaymux and assemble a macOS .app bundle around it.
#
#   tools/bundle-macos.sh              build only -> target/release/displaymux.app
#   tools/bundle-macos.sh --install    also copy it to /Applications
#   tools/bundle-macos.sh --dmg        also package -> target/release/dist/*.dmg
#
# Architecture:
#   (no flag)                    build for the machine doing the build
#   --target=aarch64-apple-darwin  build that target
#   --universal                  build both targets and lipo them into one bundle
#
# The bundle carries all three binaries in Contents/MacOS. `dmux` looks for
# `dmux-ddc-helper` next to its own executable, so keeping them together inside
# the bundle is what lets the GUI spawn the DDC helper without any configuration.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

app_name="displaymux"
bundle_id="io.github.tsukiyo20.displaymux"
version="$(sed -n 's/^version *= *"\(.*\)"/\1/p' Cargo.toml | head -1)"
: "${version:?could not read version from Cargo.toml}"

out="$root/target/release"
app="$out/$app_name.app"
contents="$app/Contents"

install=false
make_dmg=false
universal=false
target=""
for arg in "$@"; do
    case "$arg" in
        --install) install=true ;;
        --dmg) make_dmg=true ;;
        --universal) universal=true ;;
        --target=*) target="${arg#--target=}" ;;
    esac
done

bins=(dmux dmux-ddc-helper dmux-gui)

# Build one architecture into the cargo directory for it.
build() {
    local triple="$1"
    if [[ -n "$triple" ]]; then
        cargo build --release --features gui --target "$triple" --bin dmux \
            --bin dmux-ddc-helper --bin dmux-gui
    else
        cargo build --release --features gui --bin dmux --bin dmux-ddc-helper --bin dmux-gui
    fi
}

echo "building displaymux $version"
if $universal; then
    # Both halves are built and then fused. A universal bundle is the one
    # artifact that runs on every Mac, which is what the updater publishes.
    arch="universal"
    bin_dir="$out/universal"
    for triple in aarch64-apple-darwin x86_64-apple-darwin; do
        echo "  $triple"
        build "$triple"
    done
    rm -rf "$bin_dir"
    mkdir -p "$bin_dir"
    for bin in "${bins[@]}"; do
        lipo -create -output "$bin_dir/$bin" \
            "target/aarch64-apple-darwin/release/$bin" \
            "target/x86_64-apple-darwin/release/$bin"
    done
elif [[ -n "$target" ]]; then
    case "$target" in
        aarch64-*) arch="arm64" ;;
        x86_64-*) arch="x86_64" ;;
        *) arch="$target" ;;
    esac
    bin_dir="target/$target/release"
    echo "  $target"
    build "$target"
else
    [[ "$(uname -m)" == "arm64" ]] && arch="arm64" || arch="x86_64"
    bin_dir="$out"
    build ""
fi

echo "assembling $app"
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"

for bin in "${bins[@]}"; do
    cp "$bin_dir/$bin" "$contents/MacOS/$bin"
done
cp assets/icon.icns "$contents/Resources/icon.icns"

cat > "$contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleDevelopmentRegion</key>
	<string>en</string>
	<key>CFBundleDisplayName</key>
	<string>$app_name</string>
	<key>CFBundleExecutable</key>
	<string>dmux-gui</string>
	<key>CFBundleIconFile</key>
	<string>icon</string>
	<key>CFBundleIdentifier</key>
	<string>$bundle_id</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>$app_name</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>$version</string>
	<key>CFBundleVersion</key>
	<string>$version</string>
	<key>LSApplicationCategoryType</key>
	<string>public.app-category.utilities</string>
	<key>LSMinimumSystemVersion</key>
	<string>13.0</string>
	<key>NSHighResolutionCapable</key>
	<true/>
</dict>
</plist>
PLIST

# Ad-hoc signature. Apple Silicon refuses to run unsigned code, and signing the
# inner binaries before the bundle keeps the seal valid.
echo "signing (ad-hoc)"
for exe in "$contents/MacOS"/*; do
    codesign --force --sign - "$exe" >/dev/null 2>&1
done
codesign --force --sign - "$app" >/dev/null 2>&1 || \
    echo "warning: ad-hoc signing failed; the bundle should still run locally"

echo "built $app"

# The updater installs from a disk image: it mounts the .dmg read-only, copies
# the .app out, and swaps it in. A zip would need a different code path, so the
# release artifact is a .dmg.
if $make_dmg; then
    # The updater picks its asset by architecture, and matches on *all* of its
    # patterns, so the name is the only thing that says which build this is.
    dist="$out/dist"
    dmg="$dist/$app_name-$version-macos-$arch.dmg"
    mkdir -p "$dist"
    rm -f "$dmg"

    staging="$(mktemp -d)"
    trap 'rm -rf "$staging"' EXIT
    cp -R "$app" "$staging/"
    # The usual drag-to-install layout.
    ln -s /Applications "$staging/Applications"

    echo "packaging $dmg"
    hdiutil create -quiet -volname "$app_name" -srcfolder "$staging" \
        -ov -format UDZO "$dmg"
    echo "built $dmg"
fi

if $install; then
    dest="/Applications/$app_name.app"
    echo "installing to $dest"
    if [[ -w /Applications ]]; then
        rm -rf "$dest"
        cp -R "$app" "$dest"
    else
        rm -rf "$dest"
        sudo cp -R "$app" "$dest"
    fi

    # A stale copy registered with LaunchServices keeps the old icon around.
    /System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister \
        -f "$dest" >/dev/null 2>&1 || true
    echo "installed $dest"
    echo
    echo "The command line tool lives at:"
    echo "  $dest/Contents/MacOS/dmux"
fi
