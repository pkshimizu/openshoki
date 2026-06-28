# 設定ダイアログと録音ファイル保存先の設定

- 作成日: 2026-06-28
- ステータス: ドラフト

## 概要

メインウィンドウを「設定画面」として作り込み、**録音ファイルの保存先**フォルダを
ユーザーが設定できるようにする。設定値は OS 標準の設定ディレクトリに TOML で永続化し、
再起動後も保持する。録音機能自体は後続のため、ここでは「保存先という設定項目を持ち、
編集・保存・復元できる」土台を作ることが狙い。

## 背景・前提（コンテキスト）

- openshoki は **常駐**型のデスクトップ録音アプリ。GUI は Slint、トレイ常駐は tray-icon。
  起動時はウィンドウを出さずトレイに常駐し、トレイメニューの「ウィンドウを表示」で
  ウィンドウを開く（`docs/CONTEXT.md`、基盤プラン `docs/plans/done/20260627-app-foundation.md`）。
- 現状のメインウィンドウ（`ui/app-window.slint`）は「openshoki / メニューバーに常駐しています」
  というプレースホルダのみ。設定 UI の置き場所として空いている。
- 永続化の仕組みは未実装。設定ファイル・config ディレクトリの扱いは本プランで新規に入れる。
- **録音セッション** は録音開始から停止までの 1 回分で、1 つの音声ファイルに対応する。
  その保存先がここで設定する「録音ファイルの保存先」。
- 既存のトレイは `toggle_item`（表示/非表示）と `quit_item`（終了）の 2 項目
  （`src/tray.rs`）。表示状態とラベルの対応は `TOGGLE_LABEL_SHOW/HIDE` 定数で一本化済み。

## 要件

- メインウィンドウを設定画面にし、現在の「録音ファイルの保存先」を表示する。
- ネイティブのフォルダ選択ダイアログから保存先を選び、設定を更新できる。
- 設定は OS 標準の設定ディレクトリに TOML で永続化し、再起動後に復元する。
- 設定ファイルが無い初回起動時は、デフォルト保存先 `~/Documents/openshoki` を使う。
- スコープ外:
  - 録音機能そのもの（マイク取得・録音・ファイル書き出し）。保存先の「設定」のみ扱う。
  - 保存先フォルダの実体作成（フォルダが無くても設定値としては保持する。実際の作成・存在
    保証は録音機能の実装時に行う）。
  - 保存先以外の設定項目（フォーマット、ホットキー等）。

## 確定した論点

ユーザー確認で決定（いずれも推奨案を採用、デフォルト保存先のみ Documents 配下）:

1. **設定 UI の形態**: 既存のメインウィンドウ（`AppWindow`）を設定画面にする。Slint の
   複数ウィンドウ管理を避けられ実装が単純。トレイ「ウィンドウを表示」がそのまま設定画面を開く。
2. **保存先の選択方法**: `rfd` クレートのネイティブフォルダ選択ダイアログを使い、選んだ
   パスを画面に表示する。打ち間違いが無く UX が良い。クロスプラットフォーム対応。
3. **永続化**: `directories` クレートで得る OS 標準の設定ディレクトリ
   （macOS: `~/Library/Application Support/...`）に、`serde` + `toml` で TOML 保存。
   Rust エコシステムの定番で、手編集もしやすい。
4. **デフォルト保存先**: `~/Documents/openshoki`。`directories` の `UserDirs::document_dir()`
   から導出し、取得できない環境ではホーム配下にフォールバックする。

調査で解消した点:
- 現状ウィンドウはプレースホルダのみで、設定 UI を載せる余地がある（`ui/app-window.slint`）。
- 永続化・設定ディレクトリの仕組みは未実装で、新規導入が必要。
- クレート最新版: `rfd 0.17`、`directories 6.0`、`serde 1`（`derive`）、`toml 1.1`。

## 実装方針

- **設定の永続化を担う `src/config.rs` を新設**する。録音や UI から独立した「設定の
  読み書き」モジュールとして切り出し、責務を分ける。
  - `Config { recording_dir: PathBuf }` を `serde` で (de)serialize 可能にする。
  - `Config::load()`: 設定ファイルがあれば読み、無ければデフォルト（`~/Documents/openshoki`）
    を返す。壊れた TOML は致命にせず、ログを残してデフォルトにフォールバックする
    （`docs/rules/error-handling.md` の方針）。
  - `Config::save(&self)`: 設定ディレクトリを作成（`create_dir_all`）してから TOML を書く。
  - パス解決は `directories::ProjectDirs`（設定ファイル置き場）と `UserDirs`（Documents）を使う。
- **UI（`ui/app-window.slint`）を設定画面にする**。`AppWindow` に次を持たせる:
  - プロパティ `in property <string> recording-dir;`（現在の保存先表示用）
  - コールバック `callback choose-folder();`（「フォルダを選択」ボタン押下）
  - レイアウト: タイトル、現在の保存先ラベル、保存先パス表示、「フォルダを選択」ボタン。
    既存の最小ウィンドウのサイズ前提（`min/preferred 360x220`、`src/main.rs` の `WINDOW_*`）は
    内容に合わせて見直す。
- **`src/main.rs` で配線する**:
  - 起動時に `Config::load()` し、`ui.set_recording_dir(...)` で初期表示。
  - 設定値はウィンドウのコールバックから更新するため、`Rc<RefCell<Config>>` で共有して
    クロージャに渡す。
  - `ui.on_choose_folder(...)`: メインスレッド（Slint イベントループ）上で
    `rfd::FileDialog::pick_folder()` を呼び、選択されたら `Config.recording_dir` を更新 →
    `save()` → `ui.set_recording_dir(...)` で表示更新。`?` を使えないコールバックなので
    保存失敗は `eprintln!` でログする。
- 既存のトレイ表示/非表示・終了・常駐（`run_event_loop_until_quit`）の挙動は変更しない。

## 実装ステップ

1. **依存追加**: `cargo add rfd directories toml` と `cargo add serde --features derive`。
   `cargo build` が通ることを確認。
2. **`src/config.rs` を実装**: `Config` 型、`load`/`save`、パス解決（ProjectDirs/UserDirs、
   デフォルト `~/Documents/openshoki`）。`main.rs` に `mod config;` を追加。
   単体で `cargo build`・`cargo clippy` が通ることを確認。
3. **設定の読み書きを単体確認**: 一時的な動作確認（`examples/` の確認用バイナリ等）で、
   保存 → ファイル生成 → 読み戻しが一致し、ファイル欠如時にデフォルトへフォールバック
   することを確認（確認後、一時ファイルは削除）。
4. **`ui/app-window.slint` を設定画面に更新**: `recording-dir` プロパティ、`choose-folder`
   コールバック、保存先表示と「フォルダを選択」ボタンを追加。`cargo build`（Slint コンパイル）
   が通ることを確認。
5. **`src/main.rs` を配線**: 起動時 `Config::load()` → 初期表示、`on_choose_folder` で
   rfd ピッカー → 更新 → `save()` → 表示更新。`Rc<RefCell<Config>>` で共有。
6. **検証**: `cargo build` / `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings`。
   目視（`docs/rules/slint.md` の手順）で、トレイから設定画面を開く → 現在の保存先表示 →
   「フォルダを選択」で選び直す → 表示が更新され TOML が書かれる → アプリ再起動で選んだ
   保存先が復元される、ことを確認。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - 新規: `src/config.rs`
  - 変更: `Cargo.toml`（依存追加）、`src/main.rs`（`mod config`・load・コールバック配線・
    ウィンドウサイズ定数の見直し）、`ui/app-window.slint`（設定画面化）
  - `src/tray.rs` は原則変更なし（メインウィンドウを設定画面にするため新規メニュー項目は不要）。
- リスクと対策:
  - **rfd とイベントループの相性**: ネイティブダイアログはメインスレッドで動かす必要がある。
    Slint コールバックはメインスレッドで実行されるため同期 `pick_folder()` を使う方針だが、
    winit イベントループと競合・ブロックする懸念がある。問題が出たら `rfd::AsyncFileDialog`
    へ切り替え、Slint のイベントループにフューチャを橋渡しする。検証ステップ 6 で実機確認する。
  - **設定ディレクトリ/Documents が取得できない環境**: `directories` が None を返す場合は
    ホーム配下へフォールバックし、ログを残す。致命にしない。
  - **TOML 破損・権限エラー**: 読み込み失敗はデフォルトにフォールバック、保存失敗は
    `eprintln!` でログ（`docs/rules/error-handling.md`）。アプリは落とさない。
  - **ウィンドウサイズの二重定義**: サイズは `.slint` 側と `src/main.rs` の `WINDOW_*` に
    重複している。設定画面化で内容が増えるため、両者を矛盾なく更新する（相互参照コメントあり）。

## 未確定事項

- 保存先フォルダの実体作成タイミング（設定時に作るか、録音開始時に作るか）。本プランでは
  作らず、録音機能の実装時に決める。
- `directories::ProjectDirs` の qualifier/organization 文字列（例: `net.noncore.openshoki`）。
  設定ファイルのパスに影響するため、実装時に確定して `config.rs` のコメントに残す。
