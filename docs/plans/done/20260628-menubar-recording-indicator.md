# 録音中であることをメニューバーで確認できるようにする

- 作成日: 2026-06-28
- ステータス: ドラフト

## 概要

メニューバー／タスクバーに常駐したまま、いま録音中かどうかをひと目で確認できるようにする。
直前のコミット（`84da60d`）でアイコンの色替え（グレー→赤）とホバー時ツールチップは入って
いるが、ツールチップはホバーしないと見えず、暫定アイコンの色替えだけでは気づきにくい。
そこで **録音経過時間のテキスト表示** と **録音マークの明確化（点滅）** を足し、常駐 UI 上で
能動的に「録音中」と分かる状態を作る。

## 背景・前提（コンテキスト）

- 本アプリは常駐型（CONTEXT.md「常駐」）。ウィンドウを主役にせず、メニューバー／タスクバーの
  アイコンで状態と操作を完結させる設計。録音中表示もこの常駐 UI 上で行うのが筋。
- トレイは `tray-icon` クレート（0.24.1）。Slint からは見えず、`main.rs` の Slint タイマー
  （`MENU_POLL_INTERVAL = 100ms`）でメニューイベントをポーリングして橋渡ししている。
  この既存タイマーを表示更新にも再利用する（新たなスレッド／タイマーを増やさない）。
- 録音状態は専用フラグを持たず「`recorder: Option<Recorder>` が `Some` か」で表す方針
  （`main.rs:116` 付近のコメント。「ありえない状態」を作らない）。本プランもこれに従う。
- 現状の録音中表示は `tray::set_recording_state(icon, recording)`（`src/tray.rs:71`）が
  アイコン色（`dot_icon`）とツールチップ（`TOOLTIP_IDLE` / `TOOLTIP_RECORDING`）を切り替える。
- `Recorder`（`src/recorder.rs`）は録音開始時刻を保持していない。経過時間表示には開始時刻が要る。
- クロスプラットフォーム方針だが macOS 先行（CONTEXT.md）。`tray-icon` の `set_title` は
  macOS のメニューバーにテキストを出せる。Windows／Linux では効き方が異なるため、テキスト表示は
  「効けば出す／失敗はログのみ」の扱いにし、アイコンの色・点滅は全 OS 共通の主表示とする。

## 要件

- 録音中、メニューバーのアイコン横に **録音経過時間** を `mm:ss`（1 時間以上は `h:mm:ss`）で
  常時テキスト表示する。停止したらテキストを消す。
- 録音中、トレイアイコンを **点滅** させて、待機中（静的なグレー）と明確に区別する。
- 待機中は現状どおり（静的アイコン・テキストなし・ツールチップ「openshoki」）に戻る。
- 失敗（テキスト設定不可・アイコン更新失敗など）でアプリ（常駐）を落とさない。既存方針どおり
  ログに残して継続する。
- スコープ外:
  - 録音中ウィンドウ（Slint 側）への状態表示。今回はメニューバー常駐 UI に限る。
  - macOS テンプレート画像化など見た目の最終調整（`dot_icon` の暫定コメントどおり後続）。
  - システム音声録音（issue #5）との連動。

## 確定した論点

- **表示方法（ユーザー確認済み）**: 「経過時間のテキスト表示」＋「録音マークの明確化／点滅」を
  採用。ドロップダウン内の状態行・現状維持案は不採用。
- **経過時間の真実の出どころ**: `Recorder` が録音開始時刻（`std::time::Instant`）を保持し、
  `elapsed()` を公開する。録音の生存期間を所有しているのは `Recorder` なので、ここを正とする。
- **更新の駆動**: 既存の 100ms ポーリングタイマー（`main.rs` の `timer.start(...)`）の中で、
  録音中のみ表示を更新する。新しいタイマー／スレッドは増やさない。
- **更新の間引き**: 100ms ごとに毎回 `set_title` / `set_icon` を呼ぶと無駄なので、
  「表示中の秒数が変わったとき」「点滅フレームが切り替わったとき」だけ更新する。
- **点滅の見せ方**: アイコンを消す（透明）と不安なので、塗りつぶし赤ドットと減光赤ドットの
  2 フレームを交互に出す。点滅周期は約 600ms（100ms タイマー 6 tick ごとにトグル）。

## 実装方針

責務を分ける:

1. **`Recorder`（recorder.rs）= 時間の真実**。開始時刻を持ち `elapsed()` を返すだけ。表示は知らない。
2. **`tray.rs` = 描画**。状態（待機／録音 + 経過時間 + 点滅フレーム）を受け取りアイコン・タイトル・
   ツールチップへ反映する純粋な描画関数群。
3. **`main.rs` のタイマー closure = 駆動と状態管理**。録音中かどうか（`recorder.is_some()`）と
   点滅フレーム・最後に描いた秒数を持ち、変化時のみ `tray.rs` の描画関数を呼ぶ。

これにより「録音状態は `Option<Recorder>` で表す」既存方針を崩さず、表示更新だけを既存タイマーに
相乗りさせられる。

## 実装ステップ

1. **`Recorder` に開始時刻と経過時間を追加**（`src/recorder.rs`）
   - フィールド `started_at: std::time::Instant` を追加し、`start()` 内（ストリーム再生開始の
     直後）で `Instant::now()` を入れる。
   - `pub fn elapsed(&self) -> std::time::Duration` を追加。
   - 確認: `cargo build` が通る。`elapsed()` が単調増加する（後続の表示で目視）。

2. **経過時間の整形関数を用意**（`src/tray.rs` もしくは小さなヘルパ）
   - `fn format_elapsed(d: Duration) -> String` を追加。`mm:ss`、1 時間以上は `h:mm:ss`。
   - 確認: 単体テスト（`#[cfg(test)]`）で 0s→`00:00`、65s→`01:05`、3661s→`1:01:01` を検証。

3. **`tray.rs` の描画 API を整理**（`src/tray.rs`）
   - 既存 `set_recording_state(icon, recording)` を、状態ごとの関数に置き換える:
     - `pub fn set_idle(icon)`: 静的アイコン・タイトル消去（`set_title(None::<&str>)`）・
       ツールチップを `TOOLTIP_IDLE` に。
     - `pub fn render_recording(icon, elapsed: Duration, blink_on: bool)`: 点滅フレームの
       アイコンへ更新・`set_title(Some(format_elapsed(elapsed)))`・ツールチップ
       `TOOLTIP_RECORDING`。
   - `dot_icon` を点滅 2 フレームに対応させる（録音中: 塗りつぶし赤／減光赤、待機: グレー）。
     `DotColor` を拡張するか、`recording_dot(blink_on: bool)` を足す。
   - すべて失敗は `eprintln!` でログのみ（既存方針）。
   - 確認: `cargo build` が通る。呼び出し側（main.rs）を直すまではコンパイルエラーで気づける。

4. **`main.rs` のタイマー closure に表示駆動を組み込む**（`src/main.rs`）
   - `build_menu_event_handler` の closure 内に状態を追加:
     `blink_ticks: u32`（点滅周期カウンタ）、`last_rendered_secs: Option<u64>`、
     `last_blink_on: bool`。
   - メニューイベント処理ループの後に「現在状態の描画」を追加:
     - `recorder.is_some()` のとき: `blink_ticks` を進めて `blink_on` を算出、
       `recorder.elapsed()` の秒と `blink_on` のどちらかが前回と変われば
       `tray::render_recording(...)` を呼ぶ。
     - 録音中→待機に変わった tick で `tray::set_idle(...)` を一度呼び、`last_rendered_secs` /
       `blink_ticks` をリセット。
   - `toggle_recording`（`main.rs:159`）からアイコン更新呼び出し（`set_recording_state`）を外し、
     録音セッションの開始／停止と `record_item` ラベル切替に専念させる。開始直後の見た目反映は
     次の tick（最大 100ms）で行う。
   - 点滅・秒数の周期定数（`BLINK_PERIOD_TICKS` 等）をファイル上部の定数群に追加。
   - 確認: `cargo run` で起動 → 録音開始でアイコンが点滅し、メニューバーに `00:01, 00:02…` と
     経過時間が出る → 停止でテキストが消え静的アイコンに戻る、を目視。

5. **後始末・整合確認**
   - 不要になった定数・関数（`set_recording_state`）の削除、コメント更新（CONTEXT の語彙
     「録音セッション」に沿わせる）。
   - 確認: `cargo build` / `cargo clippy`（CI 相当）が警告なく通る。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - `src/recorder.rs`: `Recorder` にフィールド追加・`elapsed()` 追加。
  - `src/tray.rs`: 描画 API の再編（`set_idle` / `render_recording`）、`dot_icon` 拡張、
    `format_elapsed`。
  - `src/main.rs`: タイマー closure の表示駆動追加、`toggle_recording` の責務縮小。
  - `ui/app-window.slint`: 変更なし（今回はメニューバー常駐 UI のみ）。
- リスクと対策:
  - **`set_title` が Windows／Linux で効かない**: テキスト表示が出ない可能性。アイコンの色・
    点滅を主表示にしておけば最低限の確認は全 OS で成立する。失敗はログのみで継続。macOS で目視確認。
  - **100ms ごとの更新負荷**: 「変化時のみ更新」で `set_icon`/`set_title` 呼び出しを点滅トグル
    （約 600ms）と秒の変化時だけに絞る。
  - **点滅の主張が強すぎる／ちらつく**: 周期と減光フレームのコントラストで調整。透明フレームは
    使わず減光に留めて、消えたように見えない。
  - **`Instant` の単調時計**: 経過時間に `Instant` を使うため、システム時計の変更（NTP 補正等）の
    影響を受けない。ファイル名のタイムスタンプは従来どおり `chrono::Local`（用途が別）。

## 未確定事項

- 経過時間テキストに録音マーク文字（例: 先頭の `●`）を付けるか。現状はアイコン側で点滅を担い、
  テキストは時間のみとする想定。実装中に macOS 実機で見て、視認性が弱ければ `●` 付与を検討する
  （後から足せる小変更）。
