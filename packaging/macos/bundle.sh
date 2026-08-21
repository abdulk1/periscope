#!/usr/bin/env bash
#
# Builds Periscope.app, and a .dmg to put it in.
#
# Signing and notarisation are opt-in, because they need credentials this
# repository does not have and must never contain:
#
#   PERISCOPE_CODESIGN_IDENTITY   a Developer ID Application identity, e.g.
#                                 "Developer ID Application: Someone (TEAMID)"
#   PERISCOPE_NOTARY_PROFILE      a `xcrun notarytool store-credentials` profile
#
# With neither set, the output is an unsigned bundle: it runs locally, and
# Gatekeeper will refuse it on anyone else's machine. `docs/LIMITATIONS.md`
# says so plainly rather than pretending otherwise.
#
# Usage: packaging/macos/bundle.sh [--skip-build]

set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
out="$root/target/bundle"
app="$out/Periscope.app"
dmg="$out/Periscope.dmg"

version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
if [[ -z "$version" ]]; then
	echo "could not read the version out of Cargo.toml" >&2
	exit 1
fi

if [[ "${1:-}" != "--skip-build" ]]; then
	echo "==> building scope $version"
	cargo build --release --bin scope --manifest-path "$root/Cargo.toml"
fi

binary="$root/target/release/scope"
if [[ ! -x "$binary" ]]; then
	echo "no release binary at $binary" >&2
	exit 1
fi

echo "==> assembling $(basename "$app")"
rm -rf "$app" "$dmg"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"

cp "$binary" "$app/Contents/MacOS/scope"
sed "s/__VERSION__/$version/g" "$root/packaging/macos/Info.plist" \
	>"$app/Contents/Info.plist"
printf 'APPL????' >"$app/Contents/PkgInfo"

echo "==> drawing the icon"
iconset="$out/AppIcon.iconset"
rm -rf "$iconset"
mkdir -p "$iconset"
python3 "$root/packaging/macos/icon.py" "$out/AppIcon.png" >/dev/null

# The sizes macOS actually asks for. Each @2x is the next size up, which is why
# they are generated from one 1024px master rather than drawn separately.
for size in 16 32 128 256 512; do
	sips -Z "$size" "$out/AppIcon.png" --out "$iconset/icon_${size}x${size}.png" >/dev/null
	sips -Z "$((size * 2))" "$out/AppIcon.png" \
		--out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done
iconutil --convert icns "$iconset" --output "$app/Contents/Resources/AppIcon.icns"
rm -rf "$iconset" "$out/AppIcon.png"

if [[ -n "${PERISCOPE_CODESIGN_IDENTITY:-}" ]]; then
	echo "==> signing"
	# The hardened runtime is what notarisation requires; --timestamp is what
	# keeps the signature valid after the certificate expires.
	codesign --force --deep --options runtime --timestamp \
		--sign "$PERISCOPE_CODESIGN_IDENTITY" "$app"
	codesign --verify --strict --verbose=2 "$app"
else
	echo "==> not signing (PERISCOPE_CODESIGN_IDENTITY is unset)"
fi

echo "==> building $(basename "$dmg")"
staging="$out/dmg"
rm -rf "$staging"
mkdir -p "$staging"
cp -R "$app" "$staging/"
# The drag-to-install gesture everyone already knows.
ln -s /Applications "$staging/Applications"
hdiutil create -volname "Periscope" -srcfolder "$staging" -ov -format UDZO "$dmg" >/dev/null
rm -rf "$staging"

# The disk image gets signed too, not only the app inside it. Stapling a
# notarisation ticket to an unsigned image leaves `spctl -t open` reporting
# "no usable signature" for the very file people download, even though the app
# within it is accepted — so the thing being judged on first contact is the
# thing that was never signed.
if [[ -n "${PERISCOPE_CODESIGN_IDENTITY:-}" ]]; then
	echo "==> signing the disk image"
	codesign --force --timestamp --sign "$PERISCOPE_CODESIGN_IDENTITY" "$dmg"
fi

if [[ -n "${PERISCOPE_NOTARY_PROFILE:-}" ]]; then
	echo "==> notarising (this waits for Apple)"
	xcrun notarytool submit "$dmg" --keychain-profile "$PERISCOPE_NOTARY_PROFILE" --wait
	# Stapling both means neither needs the network on first launch.
	xcrun stapler staple "$app"
	xcrun stapler staple "$dmg"
	xcrun stapler validate "$dmg"
else
	echo "==> not notarising (PERISCOPE_NOTARY_PROFILE is unset)"
fi

echo
echo "app: $app"
echo "dmg: $dmg"
if [[ -z "${PERISCOPE_CODESIGN_IDENTITY:-}" ]]; then
	echo
	echo "This build is unsigned. It runs here; on another machine Gatekeeper"
	echo "will refuse it until someone right-clicks and chooses Open."
fi
