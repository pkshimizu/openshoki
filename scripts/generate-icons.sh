#!/usr/bin/env bash
# アプリアイコンとメニューバー用アイコンを、1 つのマスター資産から再生成する。
#
#   マスター: assets/icon/openshoki.icon（Icon Composer 形式。icon.json + Assets/*.svg）
#             assets/icon/mark-mono.svg（メニューバー用の一画だけを黒で描いた SVG）
#   生成物:   assets/icon/generated/Assets.car   … macOS 26（Tahoe）のレイヤードアイコン
#             assets/icon/generated/openshoki.icns … 旧 macOS 用のフォールバック
#             assets/icon/tray.png               … メニューバー常駐アイコン（36x36 8bit RGBA）
#
# 生成物はコミットする（ビルド時生成にしない）。`src/tray.rs` は tray.png を include_bytes! で
# 埋め込むため、資産を変えたらこのスクリプトを実行して差分をコミットすること。
#
# 必要なツール: Xcode（xcrun actool）、rsvg-convert、magick（ImageMagick）
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

icon_master="assets/icon/openshoki.icon"
mono_svg="assets/icon/mark-mono.svg"
generated_dir="assets/icon/generated"
tray_png="assets/icon/tray.png"

# トレイアイコンの一辺（px）。tray-icon の macOS 実装は表示高さを 18pt に固定してアスペクト比
# だけ保つため、Retina の 2x にあたる 36px で作る（32px だと拡大されてぼやける）。
tray_size=36
# トリム後に足す余白（px。1024px 換算）。グリフを高さいっぱいに収めつつ、端が切れないようにする。
tray_border=8

for tool in rsvg-convert magick; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Missing required tool: $tool" >&2
    exit 1
  fi
done
if ! xcrun --find actool >/dev/null 2>&1; then
  echo "Missing required tool: actool (install Xcode)" >&2
  exit 1
fi

echo "Generating $generated_dir from $icon_master"
rm -rf "$generated_dir"
mkdir -p "$generated_dir"
# actool は .icon から Tahoe 用の Assets.car と旧 macOS 用の .icns を同時に書き出す。
# .icns を ictool で直接書き出すと Tahoe 規定の余白が入らず大きすぎる見た目になるため、
# 必ず actool 経由で作る。
xcrun actool "$icon_master" --compile "$generated_dir" \
  --output-format human-readable-text --notices --warnings --errors \
  --output-partial-info-plist /dev/null \
  --app-icon openshoki --include-all-app-icons \
  --enable-on-demand-resources NO --development-region en \
  --target-device mac --minimum-deployment-target 13.0 --platform macosx

echo "Generating $tray_png from $mono_svg"
tmp_png="$(mktemp -t openshoki-tray-XXXXXX).png"
trap 'rm -f "$tmp_png"' EXIT
rsvg-convert -w 1024 -h 1024 "$mono_svg" -o "$tmp_png"
# 余白をトリムしてから 36px 高へ縮め、正方形の中央に置く。色は落として（template 画像として
# 使うため macOS はアルファだけを見る）8bit RGBA で書き出す（`load_tray_icon` が要求する形式）。
magick "$tmp_png" -trim +repage \
  -bordercolor none -border "$tray_border" \
  -resize "x$tray_size" \
  -background none -gravity center -extent "${tray_size}x${tray_size}" \
  -define png:color-type=6 -strip "$tray_png"

echo "Done:"
ls -1 "$generated_dir" | sed 's/^/  /'
echo "  $tray_png ($(magick identify -format '%wx%h %[bit-depth]bit %[channels]' "$tray_png"))"
