# openshoki

メニューバー／タスクバーに**常駐**して音声を録音する、Rust 製のデスクトップアプリです。
ウィンドウを主役にせず、常駐したまま素早く録音を開始・停止できることを狙っています。

現在は **macOS を主対象**に開発しています（Windows／Linux は一部機能が後続）。

## 主な機能

- **トレイ常駐**: 起動するとウィンドウを出さずにメニューバー／タスクバーへ常駐し、アイコンの
  メニューから操作します（macOS では Dock・アプリスイッチャーに出ない常駐アプリ）。
- **ワンクリックで録音の開始／停止**: メニューの「録音を開始」「録音を停止」で切り替えます。
- **録音中インジケーター**: 録音中はメニューバーのアイコンが赤く点滅し、ツールチップで状態が
  分かります。
- **マイク音声とシステム音声を別ファイルで保存**: マイク（発話）を `mic.mp3`、スピーカー等の
  システム音声（再生音）を `system.mp3` として、混ぜずに別々の MP3 で保存します
  （将来の文字起こしで発話と再生音を分けて扱うため）。
  - マイク録音は全 OS 共通（`cpal`）。
  - システム音声の録音は現状 **macOS のみ**（ScreenCaptureKit、macOS 13 以降）。
- **録音セッションをディレクトリ単位でまとめる**: 保存先の配下に、録音ごとの `<日時>`
  （例 `20260628-143025`）サブディレクトリを作り、その中に `mic.mp3` / `system.mp3` を置きます。
- **保存先の設定画面**: メニューの「設定を開く」から、録音ファイルの保存先を選べます。設定は
  OS 標準の設定ディレクトリに TOML で永続化されます。

## 動作要件

- **OS**:
  - macOS 13（Ventura）以降を主対象（システム音声録音に必要）。
  - Windows／Linux はマイク録音のみ動作対象で、システム音声録音は後続対応です。
- **権限（macOS）**: マイク録音にマイクの許可、システム音声録音に画面収録の許可が必要です。
  画面収録の許可が無い場合もアプリは落ちず、マイク録音は継続します。

## ビルドと実行

ソースからビルドして実行します（配布用の `.app` バイナリは今後提供予定）。

### 前提

- **Rust ツールチェーン**（edition 2024 を使うため Rust 1.85 以降）。
- **C コンパイラ**: `mp3lame-encoder` が libmp3lame をビルドするために必要です。
- **macOS**: 安定版の Xcode コマンドラインツール。ScreenCaptureKit の Swift ブリッジの
  ビルド・リンクに使います（ベータ版 Xcode では Swift 後方互換ライブラリを解決できず
  リンクに失敗することがあります）。

### 実行

```sh
cargo run
```

起動するとウィンドウは開かず、メニューバー／タスクバーのアイコンに常駐します。アイコンの
メニューから録音や設定を操作してください。

### リリースビルド

```sh
cargo build --release
```

## プロジェクト構成

```
openshoki/
├── Cargo.toml            クレート定義・依存
├── build.rs              Slint UI のコンパイルと macOS 向けリンク設定
├── ui/
│   └── app-window.slint  設定画面の UI 定義（Slint）
└── src/
    ├── main.rs           エントリ。トレイ初期化と Slint イベントループ起動
    ├── tray.rs           トレイアイコン／メニューの構築とイベントのディスパッチ
    ├── recorder.rs       録音セッション（マイク＋システム音声）の開始・停止と MP3 書き出し
    ├── system_audio.rs   macOS のシステム音声キャプチャ（ScreenCaptureKit）
    └── config.rs         設定（保存先）の読み込み・保存（TOML）
```

主な依存: GUI に [Slint](https://slint.dev/)、トレイ常駐に `tray-icon`、マイク取得に `cpal`、
MP3 エンコードに `mp3lame-encoder`、設定の永続化に `directories` / `serde` / `toml`。macOS では
`screencapturekit` と `objc2` 系を使います。

## 現状と今後

- **現状**: macOS 先行。マイク録音は全 OS、システム音声録音は macOS のみ。
- **今後**:
  - Windows（WASAPI loopback）／Linux（monitor source）のシステム音声録音（[#23](https://github.com/pkshimizu/openshoki/issues/23) / [#24](https://github.com/pkshimizu/openshoki/issues/24)）
  - macOS でマイク使用を検知して録音を自動開始・停止する機能（オプトイン）
  - 配布用 macOS `.app` バンドルの生成（[#20](https://github.com/pkshimizu/openshoki/issues/20)）
  - 開発時のホットリロード（`cargo dev`、[#17](https://github.com/pkshimizu/openshoki/issues/17)）
  - 録音した音声の文字起こし

## 開発

- 設計方針・用語・ルールは `docs/` にまとめています（`CONTEXT.md`／`PLAN.md`／`ISSUE.md`／
  `PR.md`／`COMMIT.md`、実装ルールは `docs/rules/`）。
- コミット前の検証コマンド:

  ```sh
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo build
  cargo test
  ```

- CI（GitHub Actions）で上記の build／fmt／clippy／test と `cargo audit`（依存の脆弱性検査）を
  実行しています。
