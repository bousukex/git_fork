#!/bin/sh
# Makes the hicolor PNGs of Tine's icon from the source image
# (tine-source.webp): the rounded square is cut out, its outside made
# transparent, and the result scaled down. Run it again only when the
# source changes; the PNGs are kept in the repository.
# Needs dwebp (apt: webp) and ImageMagick's convert.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
dwebp -quiet "$here/tine-source.webp" -o "$work/source.png"
# The rounded square of the source (measured on its rim): 1111x1100 at
# 68,63, corners of about 270px.
convert "$work/source.png" -crop 1111x1100+68+63 +repage "$work/square.png"
convert -size 1111x1100 xc:black -fill white -draw "roundrectangle 2,2 1108,1097 270,270" "$work/mask.png"
convert "$work/square.png" "$work/mask.png" -alpha off -compose CopyOpacity -composite "$work/cut.png"
convert "$work/cut.png" -background none -gravity center -extent 1150x1150 "$work/icon.png"
for n in 16 24 32 48 64 128 256 512; do
    mkdir -p "$here/hicolor/${n}x${n}/apps"
    convert "$work/icon.png" -filter Lanczos -resize "${n}x${n}" -strip "PNG32:$here/hicolor/${n}x${n}/apps/tine.png"
done
