#!/bin/sh
# Regenerate both icons from the approved artwork. Requires ImageMagick and macOS iconutil.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SOURCE="$ROOT/crates/menubar/AppIconSource.png"
DEST="$ROOT/crates/menubar"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Clip generation artifacts outside the tile without changing the arrow artwork.
magick "$SOURCE" \
    \( -size 1254x1254 xc:black -fill white -draw 'roundrectangle 134,134 1114,1114 254,254' \) \
    -alpha off -compose CopyOpacity -composite -resize 1024x1024 "$DEST/AppIcon.png"

# Extract the colored arrows, excluding the pale tile. Fill enclosed highlight holes.
# These bounds and chroma threshold belong to this approved source image.
# Inset the mask slightly so antialiasing does not join the tips at menu bar size.
magick "$SOURCE" -crop 760x880+250+190 +repage -alpha off -colorspace sRGB \
    -fx '(max(r,max(g,b))-min(r,min(g,b)))>0.24?1:0' \
    -morphology Close Disk:2 \
    -define connected-components:area-threshold=10000 \
    -define connected-components:mean-color=true -connected-components 8 \
    -morphology Erode Disk:8 \
    -trim +repage -filter Box -resize 30x30 -gravity center -background black \
    -extent 36x36 -depth 8 "$WORK/alpha.png"
magick "$WORK/alpha.png" -depth 8 "gray:$DEST/MenuBarIcon.alpha"
magick -size 36x36 xc:black "$WORK/alpha.png" \
    -alpha off -compose CopyOpacity -composite "$DEST/MenuBarIcon.png"

mkdir "$WORK/AppIcon.iconset"
for size in 16 32 128 256 512; do
    magick "$DEST/AppIcon.png" -resize "${size}x${size}" "$WORK/AppIcon.iconset/icon_${size}x${size}.png"
    double=$((size * 2))
    magick "$DEST/AppIcon.png" -resize "${double}x${double}" "$WORK/AppIcon.iconset/icon_${size}x${size}@2x.png"
done
iconutil -c icns "$WORK/AppIcon.iconset" -o "$DEST/AppIcon.icns"
