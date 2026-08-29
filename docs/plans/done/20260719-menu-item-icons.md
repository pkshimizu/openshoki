# トレイメニューの各項目に関連アイコンを表示する

- 作成日: 2026-07-19
- ステータス: ドラフト

## 概要

トレイのコンテキストメニューの各項目（録音の開始/停止・設定を開く/閉じる・終了）に、
内容を表すアイコンを表示して視認性を上げる。アイコンは PNG 素材を用意し、録音項目は
状態（開始↔停止）に応じてアイコンも切り替える。

## 背景・前提（コンテキスト）

- openshoki はメニューバー／タスクバー常駐アプリ。トレイ常駐とメニューは `tray-icon`
  クレートが担い、メニュー項目は内部的に `muda`（`tray_icon::menu` が `muda::*` を re-export）
  で構築する（`docs/CONTEXT.md`）。
- 現状のメニュー項目は `tray_icon::menu::MenuItem`（`src/tray.rs`）で、アイコンを持てない。
  項目は `record_item`（Start/Stop Recording）・`toggle_item`（Open/Close Settings）・
  `quit_item`（Quit）の 3 つ。ラベルは既に英語化済み（`RECORD_LABEL_*` / `SETTINGS_LABEL_*`）。
- トレイのステータスアイコンはコード内で RGBA を描画して生成している（`tray.rs` の `dot_icon`）。
  `assets/` ディレクトリは現状存在しない。
- 「メニュー」= トレイのコンテキストメニューを指す。設定ウィンドウ（Slint）内の UI は対象外。

### 調査で分かったこと

- **項目アイコンには `muda::IconMenuItem` を使う**。`tray_icon::menu::{IconMenuItem, Icon}` として
  参照でき、`IconMenuItem::new(text, enabled, Some(Icon), None)` で生成、`set_text` / `set_icon` /
  `id()` は従来の `MenuItem` と同様に使える。`Menu::append` は `IsMenuItem` を受けるので
  `IconMenuItem` をそのまま追加できる。`muda::Icon` は `Clone` 可能。
- **muda の `Icon::from_path` は Windows 専用**（`#[cfg(windows)]`）。macOS では `Icon::from_rgba`
  しか使えないため、**PNG を自前で RGBA にデコードして `from_rgba` に渡す**必要がある。
- **デコードは `png` クレートで行う**。`png` 0.18.1 は既に依存ツリーに存在する（muda の macOS 依存）。
  直接依存として `Cargo.toml` に追加しても追加ビルドコストはほぼ無い。`png` はクロス
  プラットフォームなので全 OS のビルドで使える。
- **PNG はビルド時に `include_bytes!` で埋め込む**。`.app` バンドルの Resources へ素材を配置する
  仕組み（パッケージング）は未実装のため、実行時のファイル読み込みはパス解決が不安定になる。
  埋め込みなら `cargo run` と将来の `.app` の双方で確実に同じ素材を使え、パッケージングに依存しない。

## 要件

- トレイメニューの 3 項目すべてにアイコンを表示する:
  - 録音（開始）: 録音を表すアイコン（例: 赤い録音ドット）
  - 録音（停止）: 停止を表すアイコン（例: 停止の四角）
  - 設定: 設定を表すアイコン（例: 歯車）— 開く/閉じるで切り替えず固定
  - 終了: 終了を表すアイコン（例: 電源／×）
- 録音項目のアイコンは状態で切り替える。待機中は「録音（開始）」、録音中は「停止」を表示し、
  既存のラベル切替（Start/Stop Recording）と一致させる。
- 素材は実装側で用意する。シンプルな単色 PNG を生成して `assets/` にコミットし、そのまま使う
  （後で好みの素材に差し替え可能）。
- スコープ外:
  - 設定ウィンドウ（Slint）内のボタン・チェックボックスへのアイコン付与。
  - macOS の `NativeIcon`（システム画像）方式（今回は PNG 素材方式を採用）。
  - `.app` バンドルの Resources へ素材を配置する方式（埋め込みで代替するため不要）。

## 確定した論点

- **アイコン方式は「バンドル PNG 素材」**（ユーザー確認で確定）。カスタム描画や NativeIcon では
  なく PNG ファイルを素材の正とする。ただし macOS では自前デコードが必要なため、素材は PNG で
  持ちつつ `include_bytes!` + `png` デコード + `Icon::from_rgba` で読み込む。
- **録音項目のアイコンは状態で切り替える**（ユーザー確認で確定）。テキストとアイコンの示す動作を
  揃える。設定項目（開く/閉じる）はアイコンを切り替えず歯車固定にする（開閉はラベルで表す）。
- **素材は実装側で生成**（ユーザー確認で確定）。8bit RGBA の PNG を生成して `assets/` に置く。
  最終的な見た目は後から差し替えられるようにする（`未確定事項` 参照）。
- **素材仕様**: 32×32 px・8bit・カラータイプ RGBA の PNG。ライト/ダーク双方のメニュー背景で
  読めるよう、純黒・純白を避けた中間トーンかアクセント色（録音は赤）にする。デコードを単純化する
  ため、パレット/グレースケールではなく RGBA 固定とする。
- **追加依存は `png` のみ**。既存ツリーにあり、クロスプラットフォームで使える。

## 実装方針

`src/tray.rs` のメニュー項目を `MenuItem` から `IconMenuItem` に置き換え、生成時にアイコンを
渡す。アイコンは `assets/` に置いた PNG を `include_bytes!` で埋め込み、`png` クレートで RGBA に
デコードして `muda::Icon::from_rgba` を作る小さなヘルパー（例: `load_icon(bytes) -> Icon`）を
`tray.rs` に用意する。録音項目のアイコン切替は、既存のラベル切替と同じ箇所（`src/main.rs` の
`start_recording` / `toggle_recording`）で `set_icon` も呼んで行う。アイコン生成関数は
`tray.rs` に置き、`main.rs` からは `tray::record_start_icon()` / `tray::record_stop_icon()` の
ように取得して `record_item.set_icon(Some(..))` する（`Icon` は `Clone` なので都度生成でも
デコード 1 枚と軽い）。

`Tray` 構造体のフィールド型（`record_item` など）は `IconMenuItem` に変わるが、`set_text` /
`id()` の呼び出し側インターフェースは同じなので、`main.rs` の変更は型と `set_icon` の追加に
とどまる。

素材の生成は、実装時にシンプルな単色 PNG（録音ドット・停止四角・歯車・電源/×）を作って
`assets/menu/` 等に配置する。生成手段は問わない（スクリプトや簡易描画）が、成果物として
リポジトリに PNG をコミットし、コードは埋め込みで参照する。

## 実装ステップ

1. `assets/`（例: `assets/menu/`）にメニュー用 PNG を用意する: `record.png`（録音ドット）・
   `stop.png`（停止）・`settings.png`（歯車）・`quit.png`（電源/×）。いずれも 32×32・8bit・
   RGBA。リポジトリにコミットする。
2. `Cargo.toml` の `[dependencies]` に `png`（既存ツリーと同じ 0.18 系）を追加する。
3. `src/tray.rs` に、埋め込み PNG バイト列を `png` でデコードして `tray_icon::menu::Icon` を
   返すヘルパーを実装する（デコード失敗時の扱いは `error-handling.md` に従い、アイコン無しで
   続行するかログを残す）。各素材を `include_bytes!` で取り込む。
4. `src/tray.rs` の `Tray` を `MenuItem` から `IconMenuItem` に変更する。
   - `record_item`: 初期は「録音（開始）」アイコン。
   - `toggle_item`: 「設定」アイコン（固定）。
   - `quit_item`: 「終了」アイコン。
   - 各 `IconMenuItem::new(label, true, Some(icon), None)` で生成し、`Menu::append` する。
   - `Tray` のフィールド型・関連コメントを更新する。
5. `src/main.rs` の録音状態切替で、テキストに加えてアイコンも切り替える。
   - `start_recording`: `record_item.set_text(RECORD_LABEL_STOP)` に合わせて停止アイコンへ。
   - `toggle_recording` の停止側: `RECORD_LABEL_START` に合わせて録音アイコンへ。
   - `build_menu_event_handler` 内で `record_item` を引き続き使えることを確認する
     （型が `IconMenuItem` に変わるだけ）。
6. 検証（macOS 実機）:
   - `cargo run` でトレイメニューを開き、各項目に意図したアイコンが表示されることを目視確認。
   - 録音を開始→停止し、録音項目のアイコンが「録音↔停止」で切り替わり、ラベルと一致することを確認。
   - ライト/ダークのメニュー背景でアイコンが視認できることを確認。
   - `cargo build` / `cargo clippy --all-targets -- -D warnings` / `cargo test` が通ることを確認。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - `src/tray.rs`（メニュー項目を `IconMenuItem` 化、アイコン読み込みヘルパー追加）
  - `src/main.rs`（`Tray` 項目の型変更に伴う参照、録音アイコンの切替追加）
  - `Cargo.toml`（`png` 依存追加）
  - `assets/`（新規 PNG 素材、`.gitignore` で除外されていないこと）
- リスクと対策:
  - **PNG デコード/フォーマット不一致**: 素材を 8bit RGBA に固定し、デコードは RGBA 前提で単純化。
    生成時に `file`/デコードで RGBA を確認する。想定外フォーマットはログを残しアイコン無しで続行。
  - **ダークモードでの視認性**: 純黒/純白を避けた中間トーン・アクセント色にする。実機の
    ライト/ダーク双方で確認。
  - **クロスプラットフォーム**: `IconMenuItem` と `png` は全 OS 対応。Windows/Linux でも
    ビルド・表示できる想定だが、主対象は macOS。表示崩れがあれば後続で調整。
  - **`assets/` が `.gitignore` 対象**: 素材がコミットされるよう `.gitignore` を確認する。

## 未確定事項

- 最終的なアイコンのデザイン・配色は暫定（単色のシンプル素材）。より作り込んだ素材に差し替える
  場合は `assets/` の PNG を置き換えるだけで済むようにする（コードは埋め込みパス参照のみ）。
- Retina 表示での最適サイズ（32px で十分か、@2x を別途持つか）は実機確認後に必要なら調整する。
