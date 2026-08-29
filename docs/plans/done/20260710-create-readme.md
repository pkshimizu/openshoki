# README.md を作成する

- 作成日: 2026-07-10
- ステータス: ドラフト

## 概要

リポジトリ直下に日本語の `README.md` を新規作成する。openshoki が何をするアプリかを
はじめて見た人が把握でき、ソースからビルドして起動できるようにするのが狙い。現状は
配布物（`.app` リリース）がまだ無いため、ソースからのビルド・実行を中心に案内する。

## 背景・前提（コンテキスト）

- `docs/CONTEXT.md` によれば、openshoki は「メニューバー／タスクバーに**常駐**して音声を
  録音する Rust 製デスクトップアプリ」。GUI は Slint、トレイ常駐は tray-icon、マイク録音は
  cpal + mp3lame-encoder（MP3）、macOS のシステム音声は screencapturekit。
- **macOS 先行**。Windows（WASAPI loopback）/ Linux（monitor source）のシステム音声は
  後続（issue #23 / #24）。マイク録音は全 OS 共通（cpal）。
- README はまだ存在しない（本プランで新規作成）。
- 調査で判明した現状（README に事実として書けること）:
  - 依存（`Cargo.toml`）: `slint` / `tray-icon` / `cpal` / `mp3lame-encoder` / `directories` /
    `rfd` / `serde` / `toml` / `chrono`、macOS 限定で `objc2` / `objc2-app-kit` /
    `screencapturekit`（`macos_13_0` feature）。edition 2024。
  - ソース構成: `Cargo.toml` / `build.rs` / `ui/app-window.slint` /
    `src/{main,tray,recorder,system_audio,config}.rs`。
  - システム音声キャプチャは macOS で **ScreenCaptureKit（macOS 13+）** を使う。
    `mp3lame-encoder` は libmp3lame をビルドする（C コンパイラが要る）。

## 要件

- リポジトリ直下に `README.md` を **日本語**で作成する（コード識別子・ライブラリ名・
  コマンドは原語のまま）。
- **標準版**の範囲で書く。含める節（下記「実装方針」の構成）。
- スコープ外:
  - **ライセンス節は入れない**（LICENSE ファイル未定のため。将来 LICENSE を決めたら追記）。
  - 英語版 README は作らない。
  - スクリーンショット・バッジ・CI ステータス画像は今回入れない。
  - `.app` リリースの配布手順は成果物が無いため書かない（issue #20 完了後に追記）。
  - `cargo dev`（cargo-watch ホットリロード）は未整備（issue #17）なので**動く前提で
    書かない**。触れるとしても「今後」扱いに留める。

## 確定した論点

- **言語は日本語**（ユーザー選択）。理由: docs / CONTEXT / issue / PR がすべて日本語で一貫。
- **ライセンスは今回記載しない**（ユーザー選択）。LICENSE ファイルが無く未定のため、
  誤った表記を避ける。
- **範囲は標準版**（ユーザー選択）。
- **起動方法は `cargo run` を案内**（調査で確定）。`.cargo/config.toml` が存在せず
  `cargo dev` エイリアスは未整備（issue #17 が open）。CONTEXT.md には `cargo dev` の
  記述があるが現状は動かないため、README では現状動くコマンドのみ書く。
- **ビルド前提に C コンパイラ / Xcode を明記**（調査で確定）。`mp3lame-encoder` が
  libmp3lame を、macOS の screencapturekit が Swift ブリッジをビルドするため、Xcode
  コマンドラインツール（安定版）が要る。CI では Swift 後方互換ライブラリを含む安定版
  Xcode を使う運用（`.github/workflows/ci.yml`）と整合させ、README でも安定版 Xcode を
  前提として一言触れる。
- **動作要件は macOS 13+ を明記**（調査で確定）。screencapturekit の `macos_13_0` feature と
  CONTEXT の `LSMinimumSystemVersion=13.0` に基づく。マイク・画面収録の権限が要ることも書く。

## 実装方針

`README.md` を次の構成（標準版）で作成する。各節は CONTEXT.md の語彙（**常駐**・
**録音セッション**）に従う。

1. **タイトルと 1〜2 行の説明** — 「メニューバー／タスクバーに常駐して録音するデスクトップ
   アプリ」。
2. **主な機能** — 常駐トレイからのワンクリック録音開始/停止、マイクとシステム音声を
   別ファイル（`mic.mp3` / `system.mp3`）で MP3 保存、録音セッションを `<日時>` ディレクトリに
   まとめる、保存先の設定画面、（macOS）マイク使用検知での自動開始（オプトイン）。
   実装済みの範囲を正確に書き、未実装は「今後」に回す。
3. **動作要件** — 対応 OS（現状 macOS 13+ を主対象、Windows/Linux はマイク録音のみ／
   システム音声は後続）、必要な権限（マイク・画面収録）。
4. **ビルドと実行** — 前提（Rust ツールチェーン、安定版 Xcode コマンドラインツール／
   C コンパイラ）、`cargo run` での起動、`cargo build --release` でのビルド。トレイに
   常駐しウィンドウは出ない旨。
5. **プロジェクト構成** — 主要ディレクトリ/ファイルの役割（`src/*.rs`・`ui/`・`build.rs`）を
   簡潔な一覧で。
6. **現状と今後（ステータス）** — macOS 先行、Windows/Linux のシステム音声は後続
   （issue #23 / #24）、`.app` リリース（#20）・`cargo dev`（#17）・文字起こし等は今後。
7. **開発** — 参照する `docs/`（CONTEXT / PLAN / ISSUE / PR / COMMIT）と za フローの存在を
   一言。CI（build/fmt/clippy/test・cargo audit）に触れる程度。

## 実装ステップ

1. 上記構成で `README.md` を作成する。記載する機能・コマンド・構成が現在のコード
   （`Cargo.toml` / `src/` / `ui/`）と一致していることを確認する（未整備の `cargo dev` や
   未提供のリリースを「動く」と書かない）。
2. Markdown のリンク・見出し・コードブロックが壊れていないか、コマンド例が実際に有効か
   （`cargo run` / `cargo build --release`）を確認する。
3. 必要なら軽く体裁を整える（目次は標準版では任意）。

## 影響範囲・リスク

- 影響を受けるファイル: 新規 `README.md` のみ。コード・ビルドには影響しない。
- リスクと対策:
  - **CONTEXT.md との齟齬**（`cargo dev`・`assets/` など「想定」記述と現状の差）。
    → README は現状動く事実のみ記載し、未整備は「今後」に明記。CONTEXT 側の記述は
    本プランの対象外（必要なら別途整理）。
  - **記載が実装より先走る**。→ 機能一覧は実装済みに限定し、後続は issue 番号付きで「今後」に。

## 未確定事項

- ライセンス（LICENSE ファイルと README のライセンス節）は未定。決定後に追記する。
- 英語版 README の要否。将来 public での訴求を強めるなら別途検討。
- `.app` 配布手順・スクリーンショットは、リリース（#20）が整い次第 README に追記する。
