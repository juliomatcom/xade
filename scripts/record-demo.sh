#!/bin/sh
# Re-records the website, README and docs media from the real app with scripted models
# (apps/desktop/tests/demo). Needs ffmpeg, ImageMagick, git and a GPU or Mesa.
set -eu
cd "$(dirname "$0")/.."

out="$(pwd)/target/demo-media"
rm -rf "$out"
XADE_DEMO_OUT="$out" cargo test -p xade --release --test demo -- --ignored

web=site/public
docs=docs/images
mkdir -p "$docs"
video() { ffmpeg -y -loglevel error -i "$out/master.mp4" -an "$@"; }
still() { magick "$out/$1.png" -resize "$3" -quality 86 "$2"; }

# Hero video, in two codecs.
video -vf "scale=1920:-2:flags=lanczos" -c:v libx264 -preset veryslow -crf 26 \
  -pix_fmt yuv420p -movflags +faststart "$web/multi-session.mp4"
video -vf "scale=1920:-2:flags=lanczos" -c:v libvpx-vp9 -crf 38 -b:v 0 -row-mt 1 \
  -pix_fmt yuv420p "$web/multi-session.webm"

# GitHub plays no repository video inline, so the README gets an animated WebP.
video -vf "fps=12,scale=1280:-2:flags=lanczos" -c:v libwebp_anim -quality 72 \
  -compression_level 6 -loop 0 "$docs/multi-session.webp"

still team "$web/app.webp" 2960x
still home "$web/app-home.webp" 2960x
# The New Multi-Session dialog alone, cut from the middle of its frame.
magick "$out/modal.png" -gravity center -crop 1620x940+0+0 +repage -quality 86 \
  "$web/multi-session-new.webp"
cp "$web/multi-session-new.webp" "$docs/multi-session-new.webp"
still member "$docs/multi-session-member.webp" 1480x
still team-done "$docs/multi-session-team.webp" 1480x

# Social card: the team screenshot on the site's dark background.
magick -size 1200x630 radial-gradient:'#072a24-#020617' \
  \( "$out/team.png" -resize 1100x \
     \( +clone -alpha transparent -fill white \
        -draw 'roundrectangle 0,0 %[fx:w-1],%[fx:h-1] 12,12' \) \
     -compose DstIn -composite \) \
  -gravity center -compose over -composite -strip "$web/og.png"

ls -l "$web"/multi-session.* "$web"/app*.webp "$web/og.png" "$docs"
