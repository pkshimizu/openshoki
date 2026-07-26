#!/usr/bin/env bash
# MAS 可否の技術検証（#77）用に、`examples/mas_probe.rs` を .app に包んで走らせる。
#
#   ./scripts/mas-probe.sh --sandbox      # App Sandbox 有効（MAS と同じ制約）
#   ./scripts/mas-probe.sh --no-sandbox   # 同じ .app を sandbox entitlement 無しで（比較用）
#   ./scripts/mas-probe.sh --sandbox -- --verbose --skip-screen   # -- 以降はプローブへ渡す
#
# サンドボックスは**署名の entitlements** で効くため、素の実行ファイルを直接起動しても再現できない。
# ここでは ad-hoc 署名（`codesign -s -`）した .app を組んで、その中の実行ファイルを起動する
# （MAS 提出用の Apple Distribution 署名は不要。フェーズ 2 で本体へ付ける想定の entitlements は
# ad-hoc でも付与できる。付ける内容は下の plist が正）。
#
# --open は LaunchServices 経由で起動する。TCC（画面収録・マイク）とフォルダ選択パネルは
# 「どのアプリの要求か」を responsible process で見るため、シェルから実行ファイルを直接叩くと
# **ターミナル側**の権限として扱われ、.app の許可を試せない。open は標準出力を捨てるので、
# プローブに --report でファイルへも書かせて回収する。
#
# 生成物は target/ 配下（gitignore 済み）に置く。検証専用でリリースには関与しない。
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

sandbox=true
launch_with_open=false
probe_args=()
while [ $# -gt 0 ]; do
  case "$1" in
    --sandbox) sandbox=true; shift ;;
    --no-sandbox) sandbox=false; shift ;;
    --open) launch_with_open=true; shift ;;
    --) shift; probe_args=("$@"); break ;;
    *)
      echo "Unknown option: $1" >&2
      echo "usage: $0 [--sandbox|--no-sandbox] [--open] [-- <probe args>]" >&2
      exit 1
      ;;
  esac
done

# バンドル ID は検証専用のものにする（本体 #20 の CFBundleIdentifier と混ざると、TCC の許可が
# 検証ビルドに引きずられて結果を誤読する）。
bundle_id="net.noncore.shoki.masprobe"
app_name="shoki-mas-probe"

# CARGO_TARGET_DIR が設定されていると cargo の出力先が target/ ではなくなる。固定で参照すると、
# 古い実行ファイルが残っていた場合にそれを署名・起動してしまう。
target_dir="${CARGO_TARGET_DIR:-target}"
app_dir="$target_dir/mas-probe/$app_name.app"

cargo build --example mas_probe

rm -rf "$app_dir"
mkdir -p "$app_dir/Contents/MacOS"
cp "$target_dir/debug/examples/mas_probe" "$app_dir/Contents/MacOS/$app_name"

# マイクは usage description が無いと、プロンプトが出る前に落ちる。画面収録も TCC の対象だが
# usage description のキーは無く、.app として署名されていることが条件になる。
cat > "$app_dir/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleExecutable</key><string>$app_name</string>
  <key>CFBundleIdentifier</key><string>$bundle_id</string>
  <key>CFBundleName</key><string>$app_name</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.0.0</string>
  <key>CFBundleVersion</key><string>0</string>
  <key>LSMinimumSystemVersion</key><string>14.4</string>
  <key>NSMicrophoneUsageDescription</key>
  <string>shoki MAS probe checks whether audio APIs work inside the App Sandbox.</string>
</dict>
</plist>
PLIST

# mktemp が作った実体とは別のパスへ書かないよう、ディレクトリを掘ってその中に置く
# （macOS の mktemp -t は引数を接頭辞として扱うため、末尾に拡張子を足すと別ファイルになる）。
tmp_dir="$(mktemp -d -t shoki-mas-probe)"
trap 'rm -rf "$tmp_dir"' EXIT
entitlements="$tmp_dir/entitlements.plist"
if [ "$sandbox" = true ]; then
  # MAS で必要になる最小構成（プランのフェーズ 2 で本体へ付ける想定と同じ組み合わせ）。
  cat > "$entitlements" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.app-sandbox</key><true/>
  <key>com.apple.security.device.audio-input</key><true/>
  <!-- network.client はこのプローブでは使わない（通信経路が無い）。本体はモデルの
       ダウンロードで要るため、フェーズ 2 の想定セットをそのまま検証する目的で付けている。 -->
  <key>com.apple.security.network.client</key><true/>
  <key>com.apple.security.files.user-selected.read-write</key><true/>
  <key>com.apple.security.files.bookmarks.app-scope</key><true/>
</dict>
</plist>
PLIST
else
  # 比較用: 同じ .app を sandbox 無しで署名する（違いが sandbox 由来だと言い切れるようにする）。
  cat > "$entitlements" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict/>
</plist>
PLIST
fi

codesign --force --sign - --entitlements "$entitlements" --timestamp=none "$app_dir" >/dev/null

echo "Built $app_dir (sandbox=$sandbox)"
codesign -d --entitlements - --xml "$app_dir" 2>/dev/null | plutil -p - | sed 's/^/  /' || true
echo

# macOS の bash 3.2 では、空配列の `"${arr[@]}"` が `set -u` で unbound variable になる。
# 引数無しでも動くよう、要素があるときだけ展開する（以下の `${probe_args[@]+...}` も同じ理由）。
if [ "$launch_with_open" = false ]; then
  # exec だと EXIT trap が走らず一時ディレクトリが残るため、通常実行して終了コードを引き継ぐ。
  status=0
  "$app_dir/Contents/MacOS/$app_name" ${probe_args[@]+"${probe_args[@]}"} || status=$?
  exit "$status"
fi

# --open のときのレポート回収（理由はファイル冒頭のコメント参照）。
report_dir="$HOME/Library/Containers/$bundle_id/Data"
if [ "$sandbox" = false ]; then
  # サンドボックス無しではコンテナが作られないので、同じ場所を自前で用意する。中身は
  # 「どのユーザーが何を動かしているか」の目録なので所有者のみアクセス可にする。
  mkdir -m 700 -p "$report_dir"
fi
report_file="$report_dir/mas-probe-report.txt"
rm -f "$report_file"
# 表示し終えたら消す（実行パスの一覧を置きっぱなしにしない）。
trap 'rm -rf "$tmp_dir"; rm -f "$report_file"' EXIT

echo "Launching via LaunchServices (output goes to $report_file)…"
# --report はユーザーの probe_args より**前**に置く。プローブは同じフラグの最初の出現を採るため、
# 後置すると `-- --report /path` を渡されたときにこちらの指定が負けて回収できなくなる。
# open 自体の失敗も握りつぶさず、下の診断へ進ませる（set -e で即死させない）。
open -W "$app_dir" --args --report "$report_file" ${probe_args[@]+"${probe_args[@]}"} \
  || echo "open failed; see the diagnosis below." >&2

if [ -f "$report_file" ]; then
  cat "$report_file"
else
  echo "The app did not write $report_file (it may have been denied or crashed)." >&2
  echo "Check Console.app for sandbox/TCC denials." >&2
  exit 1
fi
