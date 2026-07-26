#!/usr/bin/env bash
# コミット済みのアイコン生成物が、マスターから再生成したものと一致するかを検査する。
# 作業ツリーは変更しない（再生成は一時ディレクトリへ出す）。
#
#   使い方: ./scripts/check-icons.sh
#
# 検査するのは決定的に再現できる生成物だけ:
#   assets/icon/openshoki.icon/Assets/mark-ink.svg / mark-ink-on-dark.svg … sed の出力なので完全一致
#   assets/icon/tray.png … ラスタライズを挟むので画素で比較（下記 TRAY_MAX_DIFF_PIXELS 参照）
#   assets/icon/generated/openshoki.icns … actool の出力だが決定的（実測）。完全一致。
#     これだけは actool（Xcode）が要る。無ければその旨を出してスキップする。
# `assets/icon/generated/Assets.car` は actool が入力を変えなくても毎回違うバイト列を出すため
# 検査しない（README の「アイコン資産の再生成」参照）。
#
# 必要なツール: rsvg-convert、magick（ImageMagick）。.icns まで見るなら Xcode（xcrun actool）。
set -euo pipefail

# tray.png の比較設定。バイト比較にすると rsvg / ImageMagick のバージョン差で偽陽性になるため、
# 画素で見る。TRAY_FUZZ 未満の色差は「同じ画素」と数え、それを超える画素が
# TRAY_MAX_DIFF_PIXELS を超えたら不一致とする（36x36 = 1296 画素）。
#
# 実測（同一マシン）: リサイズフィルタやラスタライズ解像度を変えても fuzz 10% では差 0 画素、
# 一方で形を変えると差が出る（起筆の丸みを 1px 潰す程度で 4〜5 画素、目に見える変更で 173 画素）。
# つまり環境差は fuzz 側で吸収できているので、許容画素数は小さく保って検出力を優先する。
TRAY_MAX_DIFF_PIXELS=1
TRAY_FUZZ=10%

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

regenerated="$(mktemp -d -t openshoki-icon-check-XXXXXX)"
trap 'rm -rf "$regenerated"' EXIT

# .icns まで検査できるのは、Icon Composer 形式の .icon を扱える actool があるときだけ
# （Xcode 26 以降）。無ければ mark.svg 由来の生成物だけを見る。
check_appicon=false
xcode_version="$(xcodebuild -version 2>/dev/null | head -n1)"
xcode_major="$(printf '%s' "$xcode_version" | sed -n 's/^Xcode \([0-9]*\).*/\1/p')"
if ! xcrun --find actool >/dev/null 2>&1; then
  appicon_skip_reason="actool is not available"
elif [ -z "$xcode_major" ]; then
  appicon_skip_reason="could not determine the Xcode version"
elif [ "$xcode_major" -lt 26 ]; then
  appicon_skip_reason="$xcode_version cannot compile .icon (Xcode 26 or later is required)"
else
  check_appicon=true
fi

echo "Regenerating the icon artifacts into a temporary directory…"
if [ "$check_appicon" = true ]; then
  ./scripts/generate-icons.sh --out-dir "$regenerated" >/dev/null
else
  ./scripts/generate-icons.sh --skip-appicon --out-dir "$regenerated" >/dev/null
fi

# 生成物が古い（＝再生成すれば直る）か、検査自体が失敗したか（＝再生成しても直らない）を
# 分けて数える。直し方の案内は前者のときだけ、最後に 1 回出す。
stale=false
broken=false

# 一画のレイヤー（mark.svg の色違い）。sed の出力なので環境に依らず完全一致するはず。
for layer in mark-ink.svg mark-ink-on-dark.svg; do
  committed="assets/icon/openshoki.icon/Assets/$layer"
  if ! cmp -s "$committed" "$regenerated/$committed"; then
    # パスデータは 1 行が数 KB になるので、差分そのものは出さず行数だけ示す。
    changed_lines="$(diff "$committed" "$regenerated/$committed" | grep -c '^[<>]' || true)"
    echo "$committed does not match assets/icon/mark.svg ($changed_lines lines differ)." >&2
    stale=true
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
  stale=true
else
  # compare は「差があると 1」「エラーだと 2」で終了し、指標は stderr に出す。1 は差があるだけ
  # なので自分で判定し、2（と想定外の値）はエラーとして生の出力ごと見せる。
  compare_status=0
  compare_output="$(magick compare -metric AE -fuzz "$TRAY_FUZZ" "$tray" "$regenerated/$tray" null: 2>&1)" \
    || compare_status=$?
  # 警告が混ざっても拾えるよう、指標は最終行から取る。
  diff_pixels="$(printf '%s\n' "$compare_output" | tail -n1)"
  diff_pixels="${diff_pixels%% *}"
  if [ "$compare_status" -gt 1 ] || ! [[ "$diff_pixels" =~ ^[0-9]+$ ]]; then
    echo "Could not compare $tray with the regenerated one (status $compare_status):" >&2
    echo "$compare_output" >&2
    broken=true
  elif [ "$diff_pixels" -gt "$TRAY_MAX_DIFF_PIXELS" ]; then
    echo "$tray differs from assets/icon/mark.svg ($diff_pixels pixels, allowed $TRAY_MAX_DIFF_PIXELS)." >&2
    stale=true
  fi
fi

# アプリアイコン（.icns）。icon.json / seal.svg を変えて再生成し忘れた場合もここで捕まる。
icns="assets/icon/generated/openshoki.icns"
if [ "$check_appicon" = true ]; then
  if ! cmp -s "$icns" "$regenerated/$icns"; then
    echo "$icns does not match the icon master (built here with $xcode_version)." >&2
    echo "  A different actool version can also produce this difference." >&2
    stale=true
  fi
else
  echo "Skipping $icns ($appicon_skip_reason)."
fi

if [ "$stale" = true ]; then
  echo "→ Run ./scripts/generate-icons.sh and commit the regenerated artifacts." >&2
fi
if [ "$stale" = true ] || [ "$broken" = true ]; then
  exit 1
fi
if [ "$check_appicon" = true ]; then
  echo "OK: the committed icon artifacts match the masters."
else
  echo "OK: the committed artifacts derived from assets/icon/mark.svg match (icns not checked)."
fi
