# 再生バーのクリック・スライドによる再生位置の移動（シークバー化）

- 作成日: 2026-07-22
- ステータス: 確定

## 概要

Recordings ウィンドウの再生進捗バー（現在は表示専用）を操作可能なシークバーにする。
バーのクリックでその位置へ再生位置を移動し、つまみ（ノブ）のドラッグ（スライド）でも
移動できるようにする。文字起こしセグメントのクリック（#54）に続き、「聞きたい箇所へ
すぐ飛ぶ」導線をバー側にも足す。

## 背景・前提（コンテキスト）

- 進捗バーは `ui/recordings-window.slint` の Playback セクションにある表示専用の
  `Rectangle`（高さ 5px。トラック＋`root.progress` に応じたアクセント色の塗り）。
- シークの基盤は #54 で `player.rs` に実装済み: `AudioPlayer::seek(Duration)` は
  再生/一時停止状態を変えずに位置を移動し、キューが空なら積み直し、`try_seek` 非対応時は
  デコーダを開き直して読み飛ばすフォールバックを持つ。**本プランはこれをそのまま流用する**。
- 再生位置・進捗・時刻表示（`progress` / `time-text`）は、ウィンドウ表示中のみ回る
  100ms の再生 tick（`src/main.rs` の `poll_and_update` 相当）が毎回上書きしている。
  ドラッグ中のプレビュー表示は tick の上書きと競合するため、抑止の仕組みが要る。
- 全体長は `AudioPlayer::duration()`（`Option<Duration>`）。`Decoder::try_from(File)` で
  byte_len が付くため MP3 では通常取得できるが、`None` の可能性は型上残る（その場合
  比率→秒の換算ができない）。
- 再生可否は `playable` プロパティ（両音源は mix.mp3 生成済み、単一音源は常に可）で
  ボタンを無効化する既存パターンがある。

## 要件

- バーをクリックすると、クリック位置に対応する秒へ再生位置が移動する。
- つまみ（ノブ）をドラッグすると、ドラッグ中はバーの塗り・つまみ・時刻表示が指に追従し
  （プレビュー）、**指を離した時点で** その位置へシークする（ドラッグ中は音は現状の位置の
  まま流れ続ける）。
- つまみは現在の再生位置に常時表示し、掴める場所を視覚的に示す。
- 再生できないセッション（mix 未生成）・全体長が不明な場合はバー操作を無効にする
  （表示専用の現状挙動に縮退）。
- 一時停止中・停止中（キュー空）でもシークできる（`seek()` が既にこのケースを扱う）。
- スコープ外: ドラッグ中のリアルタイムシーク、キーボード操作（矢印キー等）、
  ホバー時のツールチップ時刻表示、波形表示。

## 確定した論点

- **ドラッグ中の挙動はプレビュー＋離した時にシーク**（ユーザー確認済み）。
  MP3 のシークを連続発行しないため音の途切れ・負荷が出ず、一般的な音楽プレイヤーと同じ挙動。
- **つまみ（ノブ）を追加する**（ユーザー確認済み）。現在位置に円形のつまみを常時表示する。
- **std-widgets の `Slider` は使わず、既存バーを `TouchArea` で拡張する**（調査で確定）。
  既存の Playback セクションのデザイン（角丸トラック＋アクセント塗り、`Palette` 追従）を
  保てるうえ、`Slider` だと外部からの `progress` 更新とユーザー操作の値同期が煩雑になる。
- **tick との競合は「スクラブ中フラグ」で抑止する**（調査で確定）。Slint 側に
  `scrubbing`（in-out）を持たせ、Rust の再生 tick は `get_scrubbing()` が true の間
  `progress` / `time-text` の上書きをスキップする。
- **比率→秒の換算は Rust 側で行う**。Slint はバー上の比率（0.0〜1.0）だけを渡し、
  Rust が `duration * ratio` を計算して `seek()` する。時刻フォーマットも既存の
  `format_playback_time` を再利用するため、ドラッグ中のプレビュー時刻も Rust 側の
  コールバックで更新する。
- **操作可否は `seekable` プロパティで渡す**。選択時に Rust が
  `playable && duration.is_some()` を計算してセットする（Slint 側で複合条件を組まない。
  `has-transcript` の重複条件を一本化した #65 レビューの教訓に合わせる）。

## 実装方針

Slint 側はバーを「トラック＋塗り＋つまみ＋透明な `TouchArea`」のシークバーに組み替え、
操作イベントを比率でコールバックする。Rust 側は比率を秒へ換算して既存の `seek()` を呼ぶ。

- Slint（`ui/recordings-window.slint`）:
  - バー本体はデザイン据え置き（高さ 5px・角丸・`Palette` 追従）。つまみ（直径 ~13px の
    アクセント色の円）を塗りの右端に重ねる。
  - 当たり判定はバーの上下に広げた `TouchArea`（実質 ~20px）で取り、細いバーでも
    掴みやすくする。
  - `TouchArea` の押下・移動で `scrubbing = true` とし、`mouse-x / width` を 0.0〜1.0 に
    クランプした比率でプレビュー（塗り・つまみ位置は `scrubbing ? preview : progress`）。
    移動のたびに `scrub-preview(ratio)` を呼び、時刻表示を Rust に更新させる。
  - 離した時（クリック確定）に `seek-to-ratio(ratio)` を呼び、`scrubbing = false` に戻す。
  - `seekable` が false なら `TouchArea` を無効化し、つまみも出さない（表示専用に縮退）。
- Rust（`src/main.rs`）:
  - `on_seek_to_ratio`: `player.duration()` × 比率を `Duration` へ換算（不正値は 0 へ丸める
    ヘルパを作りテストする）して `player.seek()`。直後に `time-text` / `progress` を即時
    更新して体感の遅延をなくす（次 tick を待たない）。
  - `on_scrub_preview`: `format_playback_time(duration × ratio, duration)` で時刻表示のみ
    更新する（シークはしない）。
  - 再生 tick: `rec.get_scrubbing()` が true の間は `progress` / `time-text` の上書きを
    スキップする（現在セグメントのハイライト更新は続けてよい）。
  - セッション選択時に `seekable`（`playable && duration.is_some()`）をセットする。

## 実装ステップ

1. **Slint: シークバー部品化と操作イベント**
   進捗バーをつまみ付きシークバーに組み替え、`scrubbing` / `seekable` プロパティと
   `scrub-preview(float)` / `seek-to-ratio(float)` コールバックを追加する。
   確認: `cargo build` が通り、`examples/transcript_view.rs`（必要ならプロパティを追加）で
   つまみ・塗りがドラッグに追従し、`seekable: false` で表示専用に縮退することを
   screencapture で目視確認（`docs/rules/slint.md` の検証手順）。
2. **Rust: 比率→秒の換算とシーク接続**
   換算ヘルパ（clamp 付き、不正比率・`duration` 無しでもパニックしない）を追加して
   単体テストを書き、`on_seek_to_ratio` / `on_scrub_preview` を接続、tick に
   `scrubbing` ガードを入れ、選択時に `seekable` をセットする。
   確認: `cargo test` が通る。実アプリ（`cargo run`）で、クリック・ドラッグで再生位置が
   移動し、ドラッグ中に音が飛ばず時刻表示だけ追従し、離した瞬間にシークすること。
   一時停止中・再生終了後のシーク、mix 未生成セッションで操作できないことも確認する。
3. **ドキュメント同期と検証**
   README の「録音の一覧と再生」にシークバー操作を追記し、`docs/CONTEXT.md` の再生の節に
   シークバー（離した時にシーク方式）を 1 文追加する。
   確認: `cargo build` / `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
   `cargo test` がすべて通る。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - `ui/recordings-window.slint`（進捗バー→シークバー、プロパティ・コールバック追加）
  - `src/main.rs`（コールバック接続、tick のスクラブ中ガード、`seekable` セット、換算ヘルパ）
  - `README.md` / `docs/CONTEXT.md`（機能説明の同期）
  - `player.rs` は**変更不要**（既存 `seek()` を流用）
- リスクと対策:
  - **tick の上書き競合**: ドラッグ中に 100ms tick が `progress` を書き戻すとつまみが
    震える。→ `scrubbing` ガードで抑止（実装方針どおり）。
  - **`try_seek` 非対応のフォールバック経路**: デコーダを開き直すと再生位置表示の基準が
    0 に戻りうる（#54 で既知・ログあり）。→ 本プランでは既知の縮退として許容し、挙動を
    変えない。
  - **終端ぎりぎりへのシーク**: 比率 1.0（終端）へのシークは即終了になりうる。→ 比率を
    0.0〜1.0 にクランプした上で、終端シークは「再生が終わる」自然な挙動として許容する。
  - **レイアウト崩れ**: `alignment` 指定下の stretch 無効など Slint レイアウトの落とし穴
    （`docs/rules/slint.md`）。→ ステップ 1 で example ＋ screencapture の目視確認を必須にする。

## 未確定事項

- なし
