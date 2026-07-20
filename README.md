# openshoki

メニューバー／タスクバーに**常駐**して音声を録音する、Rust 製のデスクトップアプリです。
ウィンドウを主役にせず、常駐したまま素早く録音を開始・停止できることを狙っています。

現在は **macOS を主対象**に開発しています（Windows／Linux は一部機能が後続）。

## 主な機能

- **トレイ常駐**: 起動するとウィンドウを出さずにメニューバー／タスクバーへ常駐し、アイコンの
  メニューから操作します（macOS では Dock・アプリスイッチャーに出ない常駐アプリ）。
- **多重起動しない**: 既に起動している状態で再度起動しても、二重に常駐せず終了します（自動録音の
  二重発火や保存先の競合を防ぐため）。ロックは起動中だけ有効で、終了・クラッシュ後は再び起動できます。
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
- **録音停止時の自動文字起こし（オプトイン）**: ローカルの whisper.cpp で各音源をオンデバイス
  文字起こしし、セグメントの開始/終了時刻付き JSON（`mic.json` / `system.json`）をセッション
  ディレクトリへ保存します（音声を外部送信しません）。設定画面のトグルで有効化するだけで使え、
  whisper モデル（ggml-small、約 465MB）は初回の文字起こし時に Hugging Face から自動ダウンロード
  してデータディレクトリへ保存・再利用します（SHA-256 検証つき）。通信はこのダウンロード
  （受信）のみで、音声や文字起こし結果を送信することはありません。
- **録音の一覧と再生**: メニューの「Recordings…」から、録音済みセッションを新しい順に一覧し、
  選んで再生（Play / Pause / Stop、経過/全体時間の表示）できます。マイクとシステム音声の両方が
  あるセッションは、ミックスして同時に再生します。

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
- **CMake**: `whisper-rs` が whisper.cpp をビルドするために必要です（`brew install cmake`）。
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
│   ├── app-window.slint       設定画面の UI 定義（Slint）
│   └── recordings-window.slint 録音一覧・再生ウィンドウの UI 定義（Slint）
├── assets/
│   └── menu/             トレイメニュー項目のアイコン（PNG, 32x32 RGBA。ビルド時に埋め込む）
└── src/
    ├── main.rs           エントリ。トレイ初期化と Slint イベントループ起動
    ├── tray.rs           トレイアイコン／メニューの構築とイベントのディスパッチ
    ├── recorder.rs       録音セッション（マイク＋システム音声）の開始・停止と MP3 書き出し
    ├── player.rs         録音の再生（rodio でファイルをストリーミング再生）
    ├── mixdown.rs        録音停止後の mic＋system ミックス音声（mix.mp3）生成（バックグラウンド）
    ├── recordings.rs     録音セッションの探索（保存先を走査し新しい順に一覧）
    ├── system_audio.rs   macOS のシステム音声キャプチャ（ScreenCaptureKit）
    ├── transcribe.rs     録音停止後の自動文字起こし（whisper.cpp、バックグラウンド）
    ├── whisper_model.rs  内蔵 whisper モデルの管理（初回ダウンロード・SHA-256 検証）
    ├── single_instance.rs 多重起動を防ぐ排他ロック（起動時に取得）
    └── config.rs         設定（保存先など）の読み込み・保存（TOML）
```

主な依存: GUI に [Slint](https://slint.dev/)、トレイ常駐に `tray-icon`、マイク取得に `cpal`、
MP3 エンコードに `mp3lame-encoder`、再生に `rodio`、設定の永続化に `directories` / `serde` / `toml`、
多重起動防止に `fs2`、
文字起こしに `whisper-rs`（whisper.cpp）/ `symphonia`（MP3 デコード）/ `rubato`（リサンプル）。
macOS では `screencapturekit` と `objc2` 系を使います。

## 現状と今後

- **現状**: macOS 先行。マイク録音は全 OS、システム音声録音は macOS のみ。
- **今後**:
  - Windows（WASAPI loopback）／Linux（monitor source）のシステム音声録音（[#23](https://github.com/pkshimizu/openshoki/issues/23) / [#24](https://github.com/pkshimizu/openshoki/issues/24)）
  - 配布用 macOS `.app` バンドルの生成（[#20](https://github.com/pkshimizu/openshoki/issues/20)）
  - 録音の文字起こし表示と発話秒数へのスキップ（[#54](https://github.com/pkshimizu/openshoki/issues/54)）

## 開発

- **ホットリロード（自動再ビルド・再起動）**: `cargo dev` でソース（`src` / `ui` /
  `build.rs` / `Cargo.toml`）の変更を監視し、保存するたびに自動で再ビルドして起動し直します。
  事前に `cargo install cargo-watch` が必要です。

  ```sh
  cargo install cargo-watch   # 初回のみ
  cargo dev
  ```

- コミット前の検証コマンド:

  ```sh
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo build
  cargo test
  ```

- CI（GitHub Actions）で上記の build／fmt／clippy／test と `cargo audit`（依存の脆弱性検査）を
  実行しています。
