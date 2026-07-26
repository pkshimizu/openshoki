#!/usr/bin/env bash
# MAS 可否の技術検証（#77）用に、`examples/mas_probe.rs` を .app に包んで走らせる。
#
#   ./scripts/mas-probe.sh --sandbox      # App Sandbox 有効（MAS と同じ制約）
#   ./scripts/mas-probe.sh --no-sandbox   # 同じ .app を sandbox entitlement 無しで（比較用）
#   ./scripts/mas-probe.sh --sandbox -- --verbose --skip-screen   # -- 以降はプローブへ渡す
#
# サンドボックスは**署名の entitlements** で効くため、素の実行ファイルを直接起動しても再現できない。
# ここでは ad-hoc 署名（`codesign -s -`）した .app を組んで、その中の実行ファイルを起動する
# （MAS 提出用の Apple Distribution 署名は不要。sandbox / audio-input / network.client /
# user-selected read-write / bookmarks.app-scope は ad-hoc でも付与できる）。
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
bundle_id="net.noncore.openshoki.masprobe"
app_name="openshoki-mas-probe"
app_dir="target/mas-probe/$app_name.app"

cargo build --example mas_probe

rm -rf "$app_dir"
mkdir -p "$app_dir/Contents/MacOS"
cp target/debug/examples/mas_probe "$app_dir/Contents/MacOS/$app_name"

# マイク・画面収録は TCC の対象。usage description が無いと、プロンプトが出る前に落ちる。
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
  <string>openshoki MAS probe checks whether audio APIs work inside the App Sandbox.</string>
</dict>
</plist>
PLIST

entitlements="$(mktemp -t openshoki-mas-probe-XXXXXX).plist"
trap 'rm -f "$entitlements"' EXIT
if [ "$sandbox" = true ]; then
  # MAS で必要になる最小構成（プランのフェーズ 2 で本体へ付ける想定と同じ組み合わせ）。
  cat > "$entitlements" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.app-sandbox</key><true/>
  <key>com.apple.security.device.audio-input</key><true/>
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
  exec "$app_dir/Contents/MacOS/$app_name" ${probe_args[@]+"${probe_args[@]}"}
fi

# --open: LaunchServices 経由で起動する。TCC（画面収録・マイク）とフォルダ選択パネルは
# 「どのアプリが要求したか」を responsible process で見るため、シェルから実行ファイルを直に
# 叩くと**ターミナル側**の権限として扱われ、.app の許可を試せない。
# `open` は標準出力を捨てるので、プローブに --report でファイルへも書かせて回収する。
report_dir="$HOME/Library/Containers/$bundle_id/Data"
if [ "$sandbox" = false ]; then
  # サンドボックス無しではコンテナが作られないので、同じ場所を自前で用意する。
  mkdir -p "$report_dir"
fi
report_file="$report_dir/mas-probe-report.txt"
rm -f "$report_file"

echo "Launching via LaunchServices (output goes to $report_file)…"
open -W "$app_dir" --args ${probe_args[@]+"${probe_args[@]}"} --report "$report_file"

if [ -f "$report_file" ]; then
  cat "$report_file"
else
  echo "The app did not write $report_file (it may have been denied or crashed)." >&2
  echo "Check Console.app for sandbox/TCC denials." >&2
  exit 1
fi
