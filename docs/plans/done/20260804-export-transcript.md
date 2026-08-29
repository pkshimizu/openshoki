# 文字起こし結果をエクスポートできるようにする

- 作成日: 2026-08-04
- ステータス: ドラフト

## 概要

Recordings ウィンドウで選んだセッションの文字起こしを、テキスト（`.txt`）・CSV（`.csv`）・
字幕（`.vtt` / `.srt`）の 4 形式で任意の場所へ書き出せるようにする。今はアプリ内で読むことしか
できず、議事録に貼る・動画へ字幕として当てる・表計算で集計するといった二次利用ができない。

## 背景・前提（コンテキスト）

- 文字起こしはセッションディレクトリの `mic.json` / `system.json` に保存される。`src/transcribe.rs`
  が書く形は `segments[] = { start, end, text }` に `source` / `model` / `language` /
  `duration_secs` が付いたもので、doc に「`start` / `end`（秒）/ `text` の形は互換を保って変更する」と
  明記されている。
- 読み取りは `src/transcript.rs` が担い、`mic.json` / `system.json` を**話者ラベル付きで開始秒の
  昇順にマージ**して `Vec<TranscriptSegment>` を返す。話者は JSON 内の値ではなくファイル名で決める。
  欠落・破損は空として扱い落とさない。
- **`TranscriptSegment` は終了時刻を持っていない**。`RawSegment` も `end` を読まず、doc に
  「`end` は現状使わないため保持しない（ハイライトは次のセグメント開始まで継続する仕様）」と
  書かれている。字幕は終了時刻が必須なので、**読み取り側に `end` を足すのが本プランの前提作業**に
  なる。
- Recordings ウィンドウ（`ui/recordings-window.slint`）の詳細ペインには `Transcribe` と `Delete` の
  縦並びボタンがあり、`detail-transcript-status`（`TranscriptStatus` enum）と
  `detail-transcript-text` で状態を出す。文字起こし中は両ボタンを無効化する流儀。
- 表示行は `TranscriptRow { speaker, is-mic, time, text }` を Rust から詰める。画面の時刻表記は
  `tray::format_elapsed`（`mm:ss`、1 時間以上は `h:mm:ss`）を再生時間表示と共用している。
- 機微ファイルの書き出しには `src/private_file.rs`（`write` / `create`）がある。録音・文字起こし
  JSON・議事録 Markdown はすべて 0600。`OpenOptions::mode` は新規作成時しか効かないため、開いた
  **後**にモードを設定し直す作りになっている。
- ファイル選択には `rfd` を使っている（設定画面の保存先選択、トリガーアプリ選択）。いずれも Slint の
  コールバック内＝メインスレッドから呼んでいる。
- 議事録要約は `summary.md`（`src/summarize.rs`、0600）として生成済みだが、Recordings への表示は
  #81 が未実装。
- 製品名は #111 で `shoki` へ改名済み（設定は `net.noncore.shoki`）。リポジトリ名の変更と private 化は
  #112 で未了。
- 関連ルール: `docs/rules/security.md`（機微ファイルは所有者限定／外部クレートのエラーはフルパスを
  含みうるのでログへ流さない）、`docs/rules/messages.md`（UI・ログは英語）、`docs/rules/slint.md`
  （Rust ⇄ Slint の状態は enum／状態→文言は Rust の網羅 match／従属コントロールの無効化は単一の
  ゲート／操作は `tests/` のテストバックエンド・見た目は `examples/` ＋ screencapture）、
  `docs/rules/error-handling.md`（失敗でアプリを落とさない）。

## 要件

- Recordings の詳細ペインに**形式を選ぶ ComboBox と `Export…` ボタン**を置き、押すと保存ダイアログ
  （`rfd`）で場所とファイル名を決めて書き出す。
- 形式は 4 つ:
  - **Text（`.txt`）**: 1 行 1 セグメントで `[mm:ss] Mic: 本文`（時刻表記は画面と同じ
    `format_elapsed`）
  - **CSV（`.csv`）**: ヘッダ行つき、列は `start` / `end` / `speaker` / `text`
  - **WebVTT（`.vtt`）**: `WEBVTT` ヘッダ ＋ `hh:mm:ss.mmm --> hh:mm:ss.mmm`、話者は `<v Mic>`
  - **SubRip（`.srt`）**: 1 始まりの連番 ＋ `hh:mm:ss,mmm --> hh:mm:ss,mmm`、話者は行頭に `Mic: `
- 対象は**マージ済みトランスクリプト**（画面に見えているものと同じ）。
- 文字起こしが無い／失敗しているセッションでは Export を無効化する。
- 書き出したファイルは所有者限定（0600）で作る（発話内容を含むため）。
- 書き出しの結果（成功・失敗）を詳細ペインに表示する。
- スコープ外:
  - 音源別（mic のみ / system のみ）の書き出し
  - 議事録要約（`summary.md`）の書き出し（#81 の表示が入ってから改めて判断する）
  - 複数セッションの一括書き出し
  - クリップボードへのコピー
  - PDF / Word などの整形文書

## 確定した論点

**ユーザーへの確認で決まったこと**

- **形式 ComboBox ＋ `Export…` ボタン**: 形式が確定した状態で保存ダイアログを開けるので、拡張子・
  フィルタ・既定ファイル名を正しく設定できる。「ボタン 1 つで、選ばれた拡張子から形式を判定する」案は、
  拡張子なしで入力されたときの既定の扱いが増えるので採らない。
- **テキストは時刻＋話者＋本文**: 画面表示と対応が取れ、後から検索・引用しやすい。
- **字幕に話者ラベルを入れる**: WebVTT は標準の話者タグ `<v Mic>`、SubRip は行頭に `Mic: `。自分の
  発話と相手の発話を見分けられる。
- **対象はマージ済みのみ**: 画面に見えているものと一致し、実装とテストが最小で済む。

**調査で決めたこと**

- **`end` を読み取り側に足す（前提作業）**: JSON には `end` があるのに `TranscriptSegment` が
  捨てている。字幕には必須なので `end_secs` を持たせる。`end` が欠落・不正（負・非有限・`start` 未満）
  なら**次のセグメントの開始秒**へ、最後のセグメントは `start + 既定長`へ丸める。JSON は信頼境界外なので、
  既存の `start_duration` と同じ流儀でパニックさせない。
- **時刻表記は形式ごとに変える**: テキストは画面と同じ `format_elapsed`（画面と突き合わせやすい）、
  字幕は規格どおりのタイムコード、CSV は**秒（小数）**。CSV を `hh:mm:ss` 文字列にすると表計算で
  計算に使えないため。
- **CSV は UTF-8 BOM 付きにする**: 日本語環境の Excel は BOM なし UTF-8 を Shift_JIS と誤認して
  文字化けする。CSV の主用途が表計算なので BOM を付ける。テキスト・字幕には付けない（エディタや
  プレイヤーは素の UTF-8 を期待する）。
- **CSV インジェクションを防ぐ**: `=` `+` `-` `@` で始まるセルは Excel / Numbers が数式として
  解釈する。発話が「=」で始まると意図しない計算・外部参照が走りうる。該当セルは先頭にシングル
  クォートを足して無効化する（**引用符で囲むだけでは防げない**）。`docs/rules/security.md` の趣旨に沿う。
- **書き出しは既存の `private_file` を使う**: 発話内容を含むので 0600。ユーザーが選んだ場所に既存の
  緩いファイルがあっても、開いた後にモードを設定し直す `private_file` の作りがそのまま効く。
- **失敗をログだけにしない**: ユーザーが保存ダイアログで場所を決めた操作なので、黙って失敗すると
  「保存したつもり」になる。詳細ペインに結果行を出す。ログにはフルパスを含めない
  （`docs/rules/security.md`）。

## 実装方針

- **書式変換は純粋関数として `src/transcript_export.rs` に切り出す**。`&[TranscriptSegment]` から
  `String`（CSV だけ BOM のため `Vec<u8>`）を作る関数を形式ごとに置き、ファイル I/O・ダイアログ・UI から
  独立させて単体テストで固定する。
  - `pub enum ExportFormat { Text, Csv, WebVtt, SubRip }` に拡張子と表示名を集約する
    （`extension()` / `display_name()`）。Slint へは ComboBox の index ではなく enum 由来の文言リストを
    渡し、選択は index → enum で解決する（既存の whisper モデル ComboBox と同じ流儀。
    `docs/rules/slint.md`）。
  - 終了時刻の丸めも同モジュールの純粋関数にする。
- **UI は詳細ペインのボタン列に足す**。`Transcribe` / `Delete` と同じ縦並びに ComboBox と `Export…` を
  置く。無効化条件は「文字起こし中」と「セグメントが 0 件」で、`docs/rules/slint.md` の「従属コントロール
  群の無効化は単一のゲートに一本化する」に従って 1 つのゲートにまとめる（0 件判定は Rust 側で
  `detail-transcript-status` から導出して渡す）。
- **保存ダイアログは既存の rfd 利用と同じくコールバック内（メインスレッド）で開く**。既定ファイル名は
  `<セッション日時>-transcript.<拡張子>`、フィルタは選択中の形式 1 つだけにする。
- **結果表示は enum ＋ Rust の網羅 match**（`TranscriptStatus` と同じ流儀）。セッションを切り替えたら
  クリアする。

## 実装ステップ

### 1. 終了時刻を読めるようにする（`src/transcript.rs`）

`RawSegment` に `end` を足し、`TranscriptSegment` に `end_secs` を持たせる。欠落・不正時の丸めは
純粋関数にして「次セグメントの開始秒 → 既定長」の順にフォールバックする。既存の表示・シークの挙動は
変えない（ハイライトは今のまま「次のセグメント開始まで」）。

単体テスト: `end` 欠落 / `end < start` / 非有限 / 最後のセグメント / mic と system が重なる並び。

**完了条件**: `cargo test` が通り、既存のトランスクリプト表示とシークが変わらない。

### 2. 書式変換を作る（`src/transcript_export.rs` 新規）

`ExportFormat` と 4 形式の変換関数、拡張子・表示名を実装する。

単体テスト: 空 / 1 セグメント / 複数話者 / 本文に改行・カンマ・ダブルクォートを含む（CSV のクオート）
/ 本文が `=` で始まる（CSV インジェクション対策）/ 1 時間超の時刻 / VTT は `.`・SRT は `,` の
小数点区切り / SRT の連番が 1 始まり / CSV 先頭の BOM。

**完了条件**: `cargo test` が通り、書き出した `.vtt` / `.srt` を実際のプレイヤー（QuickTime / VLC）で
当てて字幕が表示される（手動確認）。

### 3. UI と配線（`ui/recordings-window.slint` / `src/main.rs`）

ComboBox・`Export…` ボタン・結果表示行を追加する。`export-transcript(int, int)`（セッション index と
形式 index）のコールバックで `load_transcript` → 変換 → `rfd` の保存ダイアログ → `private_file::write`
を行う。無効化条件と、セッション切り替え時の結果クリアも入れる。

**完了条件**: `cargo run` で 4 形式を書き出せる。文字起こしが無いセッションでは押せない。書き出した
ファイルのモードが 0600 になっている。

### 4. テストと見た目確認

- `tests/ui_export.rs`（テストバックエンド）: 形式の選択が正しい enum へ解決される、無効化条件では
  コールバックが発火しない、押下で 1 回・正しい引数で発火する。ダイアログは開かず、コールバックの契約に
  絞る（`docs/rules/slint.md` のテストバックエンド制約）。
- `examples/transcript_view.rs` に倣って、ボタン列が増えた詳細ペインのレイアウトを screencapture で
  確認する（ボタンが 4 つ縦に並ぶので詰まりやすい）。

**完了条件**: `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` が通り、
screencapture で崩れがない。

### 5. ドキュメント同期

README の機能説明に追記し、`docs/CONTEXT.md` に `src/transcript_export.rs` と 4 形式の方針
（時刻表記の使い分け・CSV の BOM・インジェクション対策・0600）を足す。

## 影響範囲・リスク

**影響を受けるモジュール**: `src/transcript.rs`（`end_secs` の追加）、`src/transcript_export.rs`
（新規）、`src/main.rs`（配線・ダイアログ・結果表示）、`ui/recordings-window.slint`（ComboBox・
ボタン・結果行）、`tests/ui_export.rs`（新規）、`README.md`、`docs/CONTEXT.md`。

**リスクと対策**:

- **`TranscriptSegment` の変更が波及する**: 表示・シーク・要約プロンプト（`src/summarize.rs` は
  `Speaker::label` の文字列を前提に書かれている）で使われている。**フィールド追加だけに留め**、既存の
  挙動と要約側の入力整形には手を入れない。
- **字幕の時間が重なる**: mic と system は別音源なので同時刻の発話がありうる。WebVTT は重なるキューを
  許容する（同時表示）が、SubRip はプレイヤーによって表示が不安定になることがある。仕様として受け入れ、
  README に「マージ済みのため話者の発話が重なることがある」と書く。分けたい場合は音源別書き出し
  （スコープ外）で対応する。
- **CSV の Excel 誤認**: BOM で回避する。BOM を嫌うツールもあるため CSV 以外には付けない。
- **CSV インジェクション**: 先頭シングルクォートで無効化し、テストで固定する。
- **保存先の権限**: `private_file` が 0600 にするため、共有フォルダへ書いても他ユーザーからは読めない。
  共有したいときは書き出し後に本人が権限を変える必要がある（README に一言添える）。
- **巨大なトランスクリプト**: 読み取り側に 32MB の上限があるため変換もその範囲に収まり、文字列を
  一括で組み立てても問題ない規模。
- **ダイアログ中のイベントループ**: rfd の保存ダイアログは既存 2 箇所と同じ使い方なので新たな懸念は
  増えない（100ms tick が止まる点も既存と同じ）。

## 未確定事項

- ComboBox とボタンの文言（`Text` / `CSV` / `WebVTT` / `SubRip` と `Export…`）。screencapture を見て
  決める。
- 既定ファイル名の形（`20260628-143025-transcript.srt` とするか、セッション日時だけにするか）。
- 終了時刻を補う最後のセグメントの既定長（2 秒案）。実際の字幕表示を見て調整する。
- 結果表示を独立した行にするか、`Transcribe` の状態行（`detail-transcript-text`）へ寄せるか。
- テキスト形式の時刻を画面と同じ `format_elapsed`（`mm:ss` / `h:mm:ss`）にしたが、grep やソートの
  都合でゼロ埋めした `hh:mm:ss` の方が扱いやすい可能性がある。実際に書き出して判断する。
