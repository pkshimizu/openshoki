# GitHub Actions でリリース用の実行バイナリを生成する

- 作成日: 2026-07-10
- ステータス: ドラフト

## 概要

`v*` タグの push を契機に、GitHub Actions で macOS（Apple Silicon）向けの配布物を
ビルドし、GitHub Releases に添付する。openshoki はメニューバー常駐の録音アプリなので、
配布物は生バイナリではなく `.app` バンドルとし、マイク／画面収録の権限プロンプトに
必要な `Info.plist` を同梱する。手作業のビルド・配布をなくし、タグ 1 つでリリースできる
状態にするのが狙い。

## 背景・前提（コンテキスト）

- 本アプリは **メニューバー常駐型**（CONTEXT.md「常駐」）。`src/main.rs` の
  `hide_dock_icon()` が実行時に `NSApplicationActivationPolicy::Accessory` を設定し
  Dock に出さない。同関数のコメントどおり、配布パッケージでは `Info.plist` の
  `LSUIElement` 指定が確実で、それを本プランのパッケージングで扱う。
- **システム音声キャプチャは macOS のみ**（`screencapturekit`、`macos_13_0` feature）。
  そのため最低対応 OS は **macOS 13.0**。マイク録音は `cpal` でクロス対応だが、
  Windows/Linux は未検証のため今回のリリース対象外。
- **Swift ランタイム依存**: `screencapturekit` が Swift ブリッジを使うため、生成バイナリは
  `@rpath` 経由で `/usr/lib/swift`（dyld 共有キャッシュ）を参照する。`build.rs` が
  `-Wl,-rpath,/usr/lib/swift` を付与済みなので、`cargo build --release` で出るバイナリは
  そのまま配布先の macOS でも解決できる（macOS 13+ は Swift ランタイムを OS 同梱）。
  → バンドル側で追加対応は不要。ただし後述の署名を行う場合は rpath/依存を壊さないこと。
- 既存 CI（`.github/workflows/ci.yml`）は macos-latest で fmt/clippy/build/test を回す。
  リリースは別ワークフロー `release.yml` として追加し、CI とは責務を分ける。
- **アプリアイコン（.icns）が未整備**。トレイアイコンは `src/tray.rs` の `dot_icon()` が
  実行時に RGBA 生成しており、ファイル素材は無い（コメントも「暫定アイコン」）。`.app` の
  アイコンは別途必要になるため、暫定素材を用意する。
- `Cargo.toml` の `version = "0.1.0"`。タグのバージョンと突き合わせて不整合を防ぐ。

## 要件

- `v*`（例: `v0.1.0`）タグの push でリリースワークフローが起動する。
- macOS / Apple Silicon（`aarch64-apple-darwin`）向けの `.app` バンドルを生成する。
- `.app` に `Info.plist`（`LSUIElement`・使用許諾文言・最低 OS バージョン等）を同梱する。
- 生成した `.app` を配布用に固めて（zip）GitHub Releases に添付する。
- スコープ外:
  - Windows / Linux 向けビルド。
  - Apple Developer 証明書による正式な **コード署名・公証（notarization）**
    （証明書が無いため。ad-hoc 署名のみ行う。詳細は「未確定事項」）。
  - 自動更新（Sparkle 等）、Homebrew Cask 等の配信チャネル。
  - Universal binary（Intel 対応）。

## 確定した論点

ユーザー確認で決めた事項:

- **対象 OS**: macOS のみ（システム音声実装が macOS 先行で、Windows/Linux は未検証のため）。
- **成果物形式**: `.app` バンドルを固めて配布（メニューバーアプリとして権限プロンプトに
  `Info.plist` が要るため、生バイナリではなくバンドルにする）。
- **トリガー**: `v*` タグ push（定番。タグ 1 つでリリースが完結する）。
- **アーキテクチャ**: Apple Silicon（`aarch64`）のみ（現行 Mac 向け。GitHub の
  `macos-latest` ランナーが arm64 のためネイティブビルドで済み、クロス設定が不要）。

調査で解消した事項:

- **バンドル手法は手動組み立てを採用**する。`cargo-bundle` は `Info.plist` に
  `LSUIElement` や `NSMicrophoneUsageDescription` などの任意キーを差し込む口が乏しく、
  今回必要な権限文言・常駐指定を確実に入れられない。ワークフロー内で `.app` の
  ディレクトリ構造を組み、リポジトリに置いた `Info.plist` テンプレートを使う方が
  透明で壊れにくい（サードパーティ製ツールへの依存も避けられる）。
- **配布物の固め方は `ditto` で zip 化**する。`.app` はシンボリックリンク・拡張属性を
  含むため、`ditto -c -k --keepParent` で `.app` を丸ごと zip にする（`.dmg` も候補だが
  未署名では Gatekeeper 挙動は zip と変わらず、CI では zip が簡潔）。
- **ランナーは `macos-latest` = arm64**。ネイティブに `aarch64-apple-darwin` が出るため
  クロスコンパイル設定は不要。

## 実装方針

1. リポジトリに **パッケージング用の定義**を置く（`packaging/`）:
   - `Info.plist` テンプレート（`LSUIElement=true`、`CFBundleIdentifier`、
     `NSMicrophoneUsageDescription`、`LSMinimumSystemVersion=13.0`、実行ファイル名など）。
     バージョン（`CFBundleShortVersionString`/`CFBundleVersion`）はビルド時にタグから
     `PlistBuddy` で流し込む。
   - アプリアイコン `AppIcon.icns`（暫定素材。ソース PNG から CI で `sips`+`iconutil` で
     生成するか、生成済み `.icns` を置く。見た目の作り込みは後続）。
2. `.github/workflows/release.yml` を追加し、以下を行う:
   - `on: push: tags: ['v*']`。
   - `runs-on: macos-latest`、`dtolnay/rust-toolchain@stable`、`Swatinem/rust-cache@v2`。
   - `cargo build --release` でリリースバイナリを生成。
   - タグのバージョンと `Cargo.toml` の `version` の一致を確認（不一致なら fail）。
   - `.app` を組み立て（`Contents/MacOS/openshoki`、`Contents/Resources/AppIcon.icns`、
     `Contents/Info.plist`）、バージョンを注入。
   - **ad-hoc 署名**（`codesign --force --deep --sign - openshoki.app`）で安定した
     コード識別子を付与し、TCC（マイク・画面収録許可）が許可を記憶しやすくする。
   - `ditto -c -k --keepParent` で `openshoki-<version>-macos-arm64.zip` を作成。
   - `softprops/action-gh-release` で Release を作成し zip を添付。
3. 動作確認（受け入れ）: テストタグ（例 `v0.1.0-test` など）で一度回し、生成 zip を
   別 Mac（または quarantine 付与状態）で展開し、右クリック→開くで常駐・録音が動くことを
   確認する。

## 実装ステップ

1. `packaging/Info.plist` を作成する。
   - 検証: `plutil -lint packaging/Info.plist` が OK。`LSUIElement`・
     `NSMicrophoneUsageDescription`・`LSMinimumSystemVersion`(=13.0)・
     `CFBundleExecutable`(=openshoki)・`CFBundleIdentifier` が含まれる。
2. 暫定アプリアイコンを用意する（`packaging/AppIcon.icns`、またはソース PNG＋CI 生成）。
   - 検証: `.app` の Finder 表示にアイコンが出る（暫定でよい）。
3. `.github/workflows/release.yml` を追加する（トリガー・ビルド・`.app` 組み立て・
   バージョン注入・ad-hoc 署名・zip 化・Release 添付）。
   - 検証: ワークフロー構文が通り（`actionlint` かローカル目視）、必要な permissions
     （`contents: write`）が設定されている。
4. タグ⇔`Cargo.toml` バージョン整合チェックを組み込む。
   - 検証: バージョン不一致タグでジョブが fail し、一致で通る。
5. テストタグで実行し、生成物を確認する。
   - 検証: Release に `openshoki-<version>-macos-arm64.zip` が添付され、展開した `.app` が
     Dock に出ず常駐し、マイク許可プロンプトが出て録音（`mic.mp3`）が保存される。
     ScreenCaptureKit の画面収録許可を与えるとシステム音声（`system.mp3`）も保存される。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - 追加: `.github/workflows/release.yml`、`packaging/Info.plist`、`packaging/AppIcon.icns`
    （＋必要ならソース PNG）。
  - 参照のみ: `build.rs`（rpath 付与を前提にする）、`Cargo.toml`（バージョン整合）、
    `src/main.rs` の `hide_dock_icon()`（`LSUIElement` と役割が重複するが害はない）。
  - 既存の `ci.yml` は変更しない。
- リスクと対策:
  - **未署名/ad-hoc 署名のため Gatekeeper 警告**が出る。対策: Release ノートに
    「右クリック→開く」または `xattr -dr com.apple.quarantine openshoki.app` を案内する。
    正式な署名・公証は証明書調達後に別 issue で対応。
  - **TCC 権限がバイナリ更新でリセットされうる**（未公証のため）。ad-hoc 署名で緩和するが、
    バージョン更新後に許可の再付与が必要な場合がある旨を案内する。
  - **Swift ランタイム解決**: 配布先が macOS 13 未満だと ScreenCaptureKit/Swift ランタイムで
    起動失敗しうる。`LSMinimumSystemVersion=13.0` で下限を明示して防ぐ。
  - **アイコン未整備**: 暫定素材で進め、作り込みは後続（既存の「暫定アイコン」方針に合わせる）。
  - **バージョン二重管理**: タグと `Cargo.toml` の不一致。整合チェックで CI 側で弾く。

## 未確定事項

- 正式なコード署名・公証をいつ入れるか（Apple Developer Program 加入・証明書と
  App-specific password / notarytool の準備が前提）。本プランでは ad-hoc 署名に留める。
- アプリアイコンの正式デザイン（暫定素材で先行、後続で差し替え）。
- `CFBundleIdentifier` の確定値（例: `net.noncore.openshoki`）。TCC はこの identity に
  紐づくため、確定後は安易に変えない方がよい。
- `.dmg` 形式での配布に切り替えるか（現状は zip。配布体験を上げたくなったら検討）。
