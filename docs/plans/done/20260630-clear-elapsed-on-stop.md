# 録音停止時にメニューバーの経過時間表示を消す

- 作成日: 2026-06-30
- ステータス: ドラフト

## 概要

録音を停止しても、メニューバーに経過時間（例: `01:23`）が残り続けるバグを直す。停止したら
待機表示へ戻し、経過時間テキストを消す。原因は `tray-icon` の macOS 実装で `set_title(None)`
が表示を消さないことなので、空文字での消去に変える。

## 背景・前提（コンテキスト）

- openshoki は **常駐**型アプリで、録音中はメニューバーのトレイに経過時間と点滅を表示する
  （issue #12 で実装。`src/tray.rs` の `render_recording` / `set_idle`、`src/main.rs` のタイマー）。
- 録音停止の流れは正しく組まれている: `toggle_recording` で `recorder` が `None` になり、次の
  タイマー tick の `else if was_recording` 分岐で `tray::set_idle(&tray_icon)` を呼んで待機表示へ
  戻す（`src/main.rs:178-185`）。アイコン色（グレー復帰）とツールチップ復帰は効いている。
- `set_idle` は経過時間テキストの消去に `icon.set_title(None::<&str>)` を使っている
  （`src/tray.rs:77`）。

## 要件

- 録音を停止したら、メニューバーの経過時間テキストが消える（待機表示に戻る）。
- 録音中の経過時間表示・点滅・アイコン色の挙動は変えない。
- スコープ外:
  - 経過時間の書式や表示位置の変更。
  - 録音中表示（`render_recording`）まわりの仕様変更。
  - Windows / Linux 向けの表示挙動（macOS のメニューバー表示が対象。他 OS は `set_title` の
    効き方が元々異なり、アイコン色・点滅を主表示にしている）。

## 確定した論点

調査で判明した根本原因:

- `tray-icon` 0.24 の macOS 実装 `set_title_inner`（`platform_impl/macos/mod.rs`）は、`title` が
  `None` のとき `if let Some(title) = title { … button.setTitle(…) }` のブロックを丸ごとスキップ
  する。つまり **`set_title(None)` は NSStatusItem ボタンの既存タイトルを消さない no-op**
  （内部の `attrs.title` だけ `None` になり、画面表示は前の経過時間のまま残る）。
- 一方 `set_title(Some(""))` なら `Some` 分岐に入り `button.setTitle("")` が呼ばれ、表示が空に
  なって消える。
- よって `set_idle` の `set_title(None)` を `set_title(Some(""))`（空文字）に変えれば消える。
  停止検知やアイコン/ツールチップ復帰のロジック自体は正しく、変更不要。

ユーザーへの確認は不要（「停止したら経過時間を消す」で要件は一意。表示の好みや優先順位の
判断余地なし）。

## 実装方針

- `src/tray.rs` の `set_idle` で、経過時間テキストの消去を `icon.set_title(None::<&str>)` から
  `icon.set_title(Some(""))` に変更する。コメントも「macOS では None では消えないため空文字で
  消す」と理由を明記する。
- 併せて、同じ落とし穴を繰り返さないための知見を `docs/rules/`（Slint/トレイの注意点）に
  残すことを検討する（`set_title(None)` は macOS で表示を消さない＝空文字を使う）。
- 停止検知・タイマー駆動・`render_recording` には手を入れない。

## 実装ステップ

1. **`set_idle` の修正**: `src/tray.rs:77` の `icon.set_title(None::<&str>)` を
   `icon.set_title(Some(""))` に変更し、理由をコメントに書く。`cargo build` / `cargo fmt --check`
   / `cargo clippy --all-targets -- -D warnings` / `cargo test` が通ることを確認する。
2. **動作確認**: 録音を開始 → メニューバーに経過時間が出る → 停止 → 経過時間が消えて待機表示
   （グレーアイコン・既定ツールチップ）に戻ることを目視で確認する（`docs/rules/slint.md` の
   検証手順に準じ、必要なら確認用バイナリ＋スクリーンショット）。
3. **知見のルール化（任意）**: 「macOS の `tray-icon` で `set_title(None)` は表示を消さない。消すには
   `Some("")` を使う」を `docs/rules/` に追記する。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - 変更: `src/tray.rs`（`set_idle` の 1 行）。必要なら `docs/rules/`（知見追記）。
  - `src/main.rs`・`render_recording`・タイマー駆動は変更なし。
- リスクと対策:
  - **他 OS への影響**: `set_title(Some(""))` は Windows 実装では no-op（`set_title` 自体が空実装）、
    GTK では空文字ラベルになるだけで、いずれも害はない。macOS の表示消去が目的。
  - **見た目の確認が必要**: ロジック上は消えるが、メニューバー幅の再計算（`update_dimensions`）が
    走るかも含め、目視で確認する（ステップ 2）。

## 未確定事項

- 知見を `docs/rules/slint.md` に追記するか、トレイ専用のルールファイルを設けるか（実装時に
  既存ルールの粒度を見て判断）。
