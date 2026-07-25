#!/usr/bin/env bash
# コミット済みのアイコン生成物が、マスターから再生成したものと一致するかを検査する。
# 作業ツリーは変更しない（再生成は一時ディレクトリへ出す）。
#
#   使い方: ./scripts/check-icons.sh
#
# 検査するのは決定的に再現できる生成物だけ:
#   assets/icon/openshoki.icon/Assets/mark-ink.svg / mark-ink-on-dark.svg … sed の出力なので完全一致
#   assets/icon/tray.png … ラスタライズを挟むので画素で比較（下記 TRAY_MAX_DIFF_PIXELS 参照）
# `assets/icon/generated/Assets.car` は actool が入力を変えなくても毎回違うバイト列を出すため
# 検査しない（README の「アイコン資産の再生成」参照）。
#
# 必要なツール: rsvg-convert、magick（ImageMagick）。Xcode は不要。
set -euo pipefail

# tray.png で許容する差分画素数（36x36 = 1296 画素中）。ラスタライザ・エンコーダのバージョン差で
# 縁のアンチエイリアスが 1〜2 画素ぶれることがあるため、完全一致ではなく小さな上限で見る。
# 実測: 同じ入力なら 0、目に見える形の変更では 173（13%）だったので、この幅で取り違えは起きない。
TRAY_MAX_DIFF_PIXELS=4
# 画素が「違う」と数える閾値。上と同じくアンチエイリアスの微差を無視するため。
TRAY_FUZZ=10%

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

regenerated="$(mktemp -d -t openshoki-icon-check-XXXXXX)"
trap 'rm -rf "$regenerated"' EXIT

echo "Regenerating the deterministic icon artifacts into a temporary directory…"
./scripts/generate-icons.sh --skip-appicon --out-dir "$regenerated" >/dev/null

failed=false

report_stale() {
  echo "  → Run ./scripts/generate-icons.sh and commit the regenerated artifacts." >&2
  failed=true
}

# 一画のレイヤー（mark.svg の色違い）。sed の出力なので環境に依らず完全一致するはず。
for layer in mark-ink.svg mark-ink-on-dark.svg; do
  committed="assets/icon/openshoki.icon/Assets/$layer"
  if ! cmp -s "$committed" "$regenerated/$committed"; then
    # パスデータは 1 行が数 KB になるので、差分そのものは出さず行数だけ示す。
    changed_lines="$(diff "$committed" "$regenerated/$committed" | grep -c '^[<>]' || true)"
    echo "$committed does not match assets/icon/mark.svg ($changed_lines lines differ)." >&2
    report_stale
  fi
done

# メニューバー用グリフ。まず形式（`decode_rgba_png` が要求する 36x36 8bit RGBA）を確かめ、
# 次に画素を比べる（バイト比較にすると、rsvg / ImageMagick のバージョン差で偽陽性になる）。
tray="assets/icon/tray.png"
committed_format="$(magick identify -format '%wx%h %[bit-depth] %[channels]' "$tray")"
regenerated_format="$(magick identify -format '%wx%h %[bit-depth] %[channels]' "$regenerated/$tray")"
if [ "$committed_format" != "$regenerated_format" ]; then
  echo "$tray has a different format than the regenerated one" >&2
  echo "  committed:   $committed_format" >&2
  echo "  regenerated: $regenerated_format" >&2
  report_stale
else
  # compare は差があると非ゼロ終了するので、指標だけを取り出して自分で判定する。
  diff_pixels="$(magick compare -metric AE -fuzz "$TRAY_FUZZ" "$tray" "$regenerated/$tray" null: 2>&1 || true)"
  diff_pixels="${diff_pixels%% *}"
  if ! [[ "$diff_pixels" =~ ^[0-9]+$ ]]; then
    echo "Could not compare $tray with the regenerated one: $diff_pixels" >&2
    failed=true
  elif [ "$diff_pixels" -gt "$TRAY_MAX_DIFF_PIXELS" ]; then
    echo "$tray differs from assets/icon/mark.svg ($diff_pixels pixels, allowed $TRAY_MAX_DIFF_PIXELS)." >&2
    report_stale
  fi
fi

if [ "$failed" = true ]; then
  exit 1
fi
echo "OK: the committed icon artifacts match assets/icon/mark.svg."
