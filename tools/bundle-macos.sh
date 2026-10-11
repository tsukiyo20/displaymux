#!/usr/bin/env bash
#
# Build displaymux and assemble a macOS .app bundle around it.
#
#   tools/bundle-macos.sh              build only -> target/release/displaymux.app
#   tools/bundle-macos.sh --install    also copy it to /Applications
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
[[ "${1:-}" == "--install" ]] && install=true

echo "building displaymux $version"
cargo build --release --features gui \
    --bin dmux --bin dmux-ddc-helper --bin dmux-gui

echo "assembling $app"
rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"

for bin in dmux dmux-ddc-helper dmux-gui; do
    cp "$out/$bin" "$contents/MacOS/$bin"
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
