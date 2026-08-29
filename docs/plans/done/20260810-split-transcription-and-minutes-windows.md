# 文字起こし・議事録要約の設定とモデル管理を、機能ごとの独立ウィンドウへ統合する

- 作成日: 2026-08-10
- ステータス: ドラフト

## 概要

いま「使うモデルの選択」が**設定画面の ComboBox**と**モデル管理ウィンドウの Use ボタン**の
2 か所にあり、同じ設定を 2 つの UI が持っている。これを解消するため、**文字起こし**と
**議事録要約**それぞれの独立ウィンドウを新設し、「その機能の設定」と「その機能で使うモデルの
管理（一覧・選択・取得・削除）」を 1 つのウィンドウにまとめる。設定画面は両ウィンドウへの
導線だけを持つ。種別横断のモデル管理ウィンドウ（#117 / #138）は廃止して 2 つに分割する。

## 関連する既存 issue と着手順

このプランは設定画面まわりの**結節点**にあり、前後に複数の issue がぶら下がる。着手順を
間違えると同じ画面を 2 回作り直すことになるので、関係を先に整理しておく。

### 先行させる: #127（デザインシステムと設定画面の再設計）

このプランは**単独では着手しない**。先に #127「refactor: デザインシステムを整備して設定画面を
再設計する」を対応し、そこで確立したデザインシステム（`ui/style.slint` の意味トークン、
セクションヘッダ・状態表示・ボタン階層などの共通部品、固定ヘッダ＋スクロール本文＋固定フッタの
構成）の上にこのプランを載せる。

理由は 2 つある。

- #127 は設定画面の Recording / Auto-Record / Transcription / Meeting Minutes の**階層と従属関係**を
  作り直す。ここを触った直後に本プランで Transcription / Meeting Minutes を別ウィンドウへ
  切り出すので、順番が逆だと同じ箇所を 2 回作り直すことになる。
- 本プランで新設する 2 ウィンドウは、#127 が整えた共通部品をそのまま使うのが望ましい。先に
  新ウィンドウを作ると、デザインシステムが無い状態の見た目を後から作り直すことになる。

**具体的な画面デザイン（レイアウト・情報の並び・トークンの当て方・ウィンドウ寸法）は、
このプランに着手する直前に別途作成する。** 本プランが決めているのは「どの設定とどのモデル管理を
どのウィンドウに置くか」という構造と、Rust ⇄ Slint の配線の方針までで、見た目は #127 の成果を
見てから決める。下の「実装方針」に書いた画面内の並び（トグル → 言語 → 一覧 → 注記 など）は
構造上の依存関係を示すための暫定案で、デザイン確定時に差し替えてよい。

### 後続にしたい: #83 / #85 / #84（エンジン選択とオンライン LLM）

この 3 つは**本プランが作る 2 ウィンドウの中身を増やす**もので、本プランを**先に**片づけて
おくのが望ましい。

- #83「LLM エンジン選択の設定基盤と API キーの Keychain 管理」は、`TranscribeEngine` /
  `SummaryEngine` の enum と `transcribe_engine` / `summarize_engine` 設定を足したうえで、
  **設定画面に API キー入力（マスク表示・保存・削除）と送信警告文の共通部を追加**する。
- #85（OpenAI Whisper API でオンライン文字起こし）と #84（オンライン LLM で議事録要約）は、
  それぞれ**設定画面でエンジンのオンライン選択肢を有効化し、送信警告を表示**する。

いずれも「文字起こしの設定」「要約の設定」なので、本プランの後なら**最初から
`TranscriptionWindow` / `MinutesWindow` に置ける**。逆順だと #83 が設定画面に作った API キー UI と
警告文を、本プランで丸ごと移す作業が発生する。

さらに、エンジン選択が入ると**オンラインエンジンを選んでいる間はローカルモデルが使われない**
という状態が生まれる。これは `config.toml` のパス上書き中と同じ性質（選んでも使われない・
落としても動かない）なので、`can_use_row` / `can_download_row` に**エンジンの軸が 1 つ増える**
ことになる。本プランではこの判定に手を入れないが、**エンジンの軸を後から足せる形**（判定は
Rust の純粋関数に閉じ、Slint 側では導出しない）を崩さないようにする。

### 順序は自由だが同じ場所を触る: #126 / #124 / #123

モデル選択の経路（`select_model`、`src/main.rs:2915`）に触れる。本プランの前後どちらでも
成立するが、コンフリクトしやすい。

- #126「選択が変わっていないときは設定を保存しない」— `select_model` の保存判定。
- #124「モデルを選び直したら前のダウンロードを打ち切る」— `select_model` からのダウンロード開始。
- #123「whisper モデルもパス上書き中は選択時にダウンロードしない」— `select_model` の
  ダウンロード開始判定の非対称（`src/main.rs:2142` に既知として明記されている箇所）。

なお本プランで ComboBox が消え、モデル選択の入口が**一覧の Use だけ**になるため、
「同じ値を選び直したときに保存が走る」経路（#126）は本プランの後では起きにくくなる
（`can_use_row` が選択中の行の Use を出さない）。#126 を本プランの後に回すなら、まだ直す価値が
あるか読み直すこと。

### 参照が古くなる: #121（文字起こし結果のエクスポート）

#121 は Recordings ウィンドウの詳細ペインの話なので本プランと衝突しないが、本文に
「**Slint へは enum 由来の文言リストを渡して index → enum で解決する（既存の whisper モデル
ComboBox と同じ流儀）**」という参照がある。本プランはその whisper モデル ComboBox を削除するので、
**本プランを先にやるなら #121 の参照先を書き換える**（言語の ComboBox は
`TranscriptionWindow` に残るので、そちらを参照先にできる）。

## 背景・前提（コンテキスト）

- `docs/CONTEXT.md` のとおり、本アプリはトレイ常駐型で、ウィンドウは**起動時に生成して隠して
  おき、閉じても hide するだけ**（`on_close_requested` が `CloseRequestResponse::HideWindow`）。
  表示は `show_window`（`src/main.rs:1758`）が初回だけ `set_position` / `set_size` でジオメトリを
  確定してから `show()` する（`docs/rules/slint.md` の「ループ稼働中の初回 show」対策）。
- Slint は `ui/app-window.slint` だけを `build.rs` がコンパイルし、他ウィンドウはそこから
  **再エクスポート**する（`ui/app-window.slint:7,9`）。この構造は今回も維持する。
- モデルの種別の正は `model_download::REGISTERED_CATALOGS`（`ModelKind::Speech` /
  `ModelKind::Summary`）。呼び出し側でカタログを並べ直さない、という不変条件がある
  （`src/model_download.rs:528`。並べ直すと種別を足した人が片方だけ更新して削除ガードが
  静かに外れる）。
- 行の状態は**取得の軸**（`ModelStatus`）と**使用の軸**（`RowUsage`）に分け、操作の可否
  （`can_use_row` / `can_download_row` / `can_delete_row`）は Rust の純粋関数が決める。
  Slint 側で enum から導出させない（`docs/rules/slint.md`）。
- ディスクの走査（`model_download::installed_models()`）は **`refresh_models_window` の中だけ**
  という不変条件が明文化されている（`src/main.rs:2246`、`docs/rules/performance.md`）。
  100ms tick は走査せず行の組み直しだけを行い、`downloaded_seen` ラッチ（`src/main.rs:1425`）が
  変化したときだけ 1 回走査し直す。
- モデル選択の入口は現在 3 つ（設定 ComboBox ×2、モデル管理の Use）で、いずれも
  `select_model`（`src/main.rs:2915`）に集約されている。Use から選んだときだけ
  `apply_model_selection_to_settings`（`src/main.rs:2950`）で設定画面の ComboBox を追従させる、
  という**片方向の非対称**がある。
- `config.toml` の `whisper_model_path` / `summary_model_path` は UI を持たない上級者向けの
  上書きで、上書き中は行の「使う」「取得する」の出し方が変わる（`can_use_row` /
  `can_download_row`）。今回この判定ロジックには手を入れない。
- 検証の流儀: **見た目は `examples/` の確認用バイナリで目視・スナップショット、配線
  （クリックが届くか）は `tests/` のポインタイベント**（`tests/ui_models.rs:10` 他）。
  UI テストはヘッドレス（`i_slint_backend_testing::init_no_event_loop()`）で、100ms tick に
  依存する挙動はテストしない。

## 要件

- 文字起こし設定ウィンドウ（`TranscriptionWindow`）を新設し、次をここに集約する。
  - 自動文字起こしの ON/OFF（`auto_transcribe`）
  - 認識言語の選択（`transcribe_language`）
  - **whisper モデルの一覧・選択（Use）・取得（Download）・削除（Delete）**
- 議事録要約設定ウィンドウ（`MinutesWindow`）を新設し、次をここに集約する。
  - 自動要約の ON/OFF（`auto_summarize`）
  - **要約 LLM の一覧・選択（Use）・取得（Download）・削除（Delete）**
- 設定画面（`AppWindow`）の Transcription セクションは、上記 2 ウィンドウへの**導線ボタンと
  現在の状態の要約行**だけにする。モデル選択の ComboBox・言語 ComboBox・自動実行トグル・
  モデル状態行は設定画面から取り除く。
- 種別横断のモデル管理ウィンドウ（`ui/models-window.slint` / `ModelsWindow`）は**廃止**する。
- `models/` 直下のカタログ外ファイル（種別を判定できない残骸）は、**両ウィンドウの末尾に
  同じものを出す**。
- 新設 2 ウィンドウの配線は `src/windows/` 配下の新モジュールへ切り出し、`main.rs` の肥大化を
  抑える。

- スコープ外:
  - **具体的な画面デザイン**（レイアウト・情報の並び・デザイントークンの当て方・ウィンドウ寸法）。
    着手直前に #127 の成果を見てから別途作成する。
  - デザインシステムそのものの整備（#127 が担う）。
  - トレイメニューから新ウィンドウを直接開く導線（設定画面経由のみ）。
  - `whisper_model_path` / `summary_model_path` の UI 化。
  - `can_use_row` / `can_download_row` / `can_delete_row` の判定ロジック変更。
  - Recordings ウィンドウ・録音／自動録音セクションの変更。
  - `main.rs` 全体の大規模なモジュール分割（既存ウィンドウの配線は動かさない）。

## 確定した論点

### モデル管理ウィンドウは廃止して 2 つに分割する（ユーザー確認済み）

要件が求めているのは「モデル選択の入口の二重化の解消」なので、種別横断の一覧を残すと
導線もコードも二重のままになる。`ModelKind` で行を絞れば分割は素直に実現できる。

### 設定画面には導線ボタンだけを残す（ユーザー確認済み）

自動実行トグル（`auto_transcribe` / `auto_summarize`）も各ウィンドウへ移す。設定画面には
「いまどうなっているか」を示す 1 行の要約テキスト（Rust が組み立てる）と、ウィンドウを開く
ボタンだけを置く。設定が 2 か所に散らない。

### カタログ外ファイルは両ウィンドウの末尾に同じものを出す（ユーザー確認済み）

種別が判定できない（`InstalledModel.kind` が `None`）以上、どちらか片方に置くのは恣意的で、
片方のウィンドウしか開かないユーザーからは掃除できなくなる。行の内容は完全に同じものを両方に
出し、どちらから消しても他方の一覧は次の走査で追従する。

### 新規 2 ウィンドウの配線だけ `src/windows/` へ切り出す（ユーザー確認済み）

`src/main.rs` は現在 4286 行あり、ウィンドウが 2 つ増えると悪化する。新設分と、そこへ移せる
モデル一覧まわり（行の合成・可否判定・走査・通知）を切り出す。既存の Recordings / 設定画面の
配線は動かさない（差分を大きくしないため）。

### 要約の「文字起こしへの従属」は無効化ではなく注意書きで表す

現状は要約のコントロールを `transcribe-deps` で囲み、`auto_transcribe` が OFF のとき淡色化・
無効化している。ウィンドウが分かれると別ウィンドウの状態でコントロールを殺すことになり、
「なぜ押せないか」がその場で分からない。また**手動の Summarize は `auto_transcribe` と独立に
動く**（`src/main.rs:271-270` のコメント、`TranscribeJob.summarize` は自動経路のぶらさげ）ため、
無効化は実装より強い制約になる。

方針: `MinutesWindow` の自動生成トグルは**常に操作可能**にし、`auto_transcribe` が OFF のときは
トグルの下に「自動生成には自動文字起こしが必要（Transcription 設定で ON にする）」旨の注意行を
出す。この注意行の出し分けは Rust が組み立てた文字列を渡す（状態→文言の対応表は Rust の網羅
match が正。`docs/rules/slint.md`）。

### モデル選択の入口は「一覧の Use」に一本化する

ComboBox が消えるので `apply_model_selection_to_settings`（`src/main.rs:2950`）は不要になり、
「Use で選んだら設定画面の ComboBox を追従させる」という非対称も消える。`select_model`
（`src/main.rs:2915`）はそのまま両ウィンドウの Use の唯一の入口として残す。設定画面の要約行は
tick が `is_visible()` のときだけ更新する（既存のモデル状態行の更新と同じ流儀）。

### ディスク走査は 2 ウィンドウで 1 つに共有する

走査を各ウィンドウが持つと、両方開いているときに 2 倍走査してしまう。走査結果
（`model_row_sources` 相当）と `downloaded_seen` ラッチは**共有の 1 つ**にし、
「どちらかのウィンドウを開いたとき・どちらかで操作した直後・取得完了を拾った 1 回」だけ走査する
という現行の不変条件をそのまま引き継ぐ。行の合成だけを種別でフィルタして各ウィンドウへ配る。

## 実装方針

### Slint 側

- `ui/model-list.slint` を新設し、`ModelRow` / `ModelStatus` と**一覧の描画（見出し・行・
  ボタン・削除確認モーダル）** を `ModelList` コンポーネントとして切り出す。現在
  `ui/models-window.slint` にある描画をほぼそのまま移し、両ウィンドウが import する
  （両ウィンドウに複製すると、行の見た目を直したとき片方だけ古くなる）。
- `ui/transcription-window.slint`（`TranscriptionWindow`）を新設: 自動文字起こしトグル →
  言語 ComboBox → `ModelList`（whisper）→ カタログ外ファイル → Hugging Face の注記。
- `ui/minutes-window.slint`（`MinutesWindow`）を新設: 自動要約トグル → 従属の注意行 →
  `ModelList`（LLM）→ カタログ外ファイル → 注記。
- `ui/app-window.slint`: Transcription セクションを「Transcription… / Meeting Minutes… の
  2 ボタン＋それぞれの状態要約行」に置き換え、`transcribe-languages` / `whisper-models` /
  `summary-models` / 各 index / 各 status / `summary-model-overridden` / 対応するコールバックを
  削除。`ModelsWindow` の再エクスポートを 2 ウィンドウに差し替える。
- `ui/models-window.slint` は削除。
- ウィンドウ高さ（`min-height` / `preferred-height`）は `examples/*_view.rs -- snapshot` で
  実測してから決める。設定画面は中身が大幅に減るので `WINDOW_HEIGHT = 900` を縮める。
  Slint 側と `src/main.rs` の定数は**必ず両方そろえて**直す。

### Rust 側

- `src/windows/mod.rs` を新設し、以下を置く。
  - `models.rs`: 現在 `main.rs` にあるモデル一覧の合成・可否判定・走査・通知
    （`model_rows` / `model_row_sources` / `can_use_row` / `can_download_row` /
    `can_delete_row` / `RowUsage` / `ModelsRefresh` / `NoticeUpdate` / `refresh_models_window` /
    `refresh_model_rows` / `apply_model_rows` / `models_total_text` / `model_delete_detail` /
    `select_model` など）を移す。`model_rows` に **`ModelKind` のフィルタ引数**を足し、
    カタログ外行（`kind == None`）は常に末尾へ付ける。
  - `transcription.rs`: `TranscriptionWindow` の生成・コールバック配線・tick の追従。
  - `minutes.rs`: `MinutesWindow` の同上。
- 走査結果とラッチを持つ共有ハンドル（現 `ModelListHandles` / `ModelsHandles` に相当）を
  1 つにまとめ、両ウィンドウの `Weak` と各自の行モデル（`Rc<VecModel<ModelRow>>`）を持たせる。
- `main.rs` に残すのは、共有ハンドルの生成と 2 ウィンドウの生成・`open-*-window`
  コールバックの登録、tick からの呼び出しだけにする。
- 設定画面の要約行テキストは `transcription_summary_text` / `minutes_summary_text` を新設して
  組み立てる（例: `On — Whisper small, English` / `Off`）。既存の `model_status_text` /
  `summary_model_status_text` は各ウィンドウの行の状態表示に引き続き使う。
- 削除するもの: `change-whisper-model` / `change-summary-model` / `change-transcribe-language` /
  `toggle-auto-transcribe` / `toggle-auto-summarize` の**設定画面側の**登録、
  `apply_model_selection_to_settings`、`MODEL_SELECT_FAILED_NOTICE` の設定画面追従に関する部分。
  トグル・言語の永続化ロジック自体は新ウィンドウのコールバックへ移す（保存失敗時の書き戻しを
  含めてそのまま。`docs/rules/slint.md`）。

### 検証（examples / tests）

- `examples/models_view.rs` を `examples/transcription_view.rs` / `examples/minutes_view.rs` へ
  分割し、既存のバリアント（`empty` / `unreadable` / `confirm` / `notice`）を引き継ぐ。
  `examples/settings_view.rs` は縮んだ設定画面に合わせて更新する。
- `tests/ui_models.rs` を `tests/ui_transcription.rs` / `tests/ui_minutes.rs` へ組み替える。
  一覧の操作契約（確認モーダル経由の削除・押した行の index・状態ごとのボタン出し分けの網羅
  match・`can-download` はプロパティに従う・見出し行はボタン無し）は `ModelList` が共通なので、
  片方のウィンドウで通し、もう片方は「そのウィンドウ固有の配線（トグル・言語）」を見る。
- `tests/ui_settings_rollback.rs` は対象の CheckBox / ComboBox が新ウィンドウへ移るので、
  そちらを見るように書き換える（設定画面に残る SpinBox の契約はそのまま）。

## 実装ステップ

0. **前提の確認と画面デザインの作成**（着手直前）: #127 が完了していることを確認し、そこで
   整備されたデザイントークン・共通部品を踏まえて、新設 2 ウィンドウと縮小後の設定画面の
   画面デザインを作る。
   *完了条件*: 2 ウィンドウそれぞれの情報の並び・使う共通部品・ウィンドウ寸法の目安が
   決まっていて、以降のステップが見た目の判断で止まらない。
1. **`ui/model-list.slint` の切り出し**: `ModelRow` / `ModelStatus` / 一覧描画 / 削除確認モーダルを
   `ModelList` コンポーネントへ移し、`ModelsWindow` がそれを使う形に一旦置き換える。
   *完了条件*: `cargo build` が通り、`cargo run --example models_view` の見た目が変わらない。
2. **`src/windows/models.rs` への切り出し**: モデル一覧の合成・可否判定・走査・通知・
   `select_model` を `main.rs` から移す（挙動は変えない）。`model_rows` に `ModelKind` フィルタ
   引数を足すが、この時点では全種別を渡す。
   *完了条件*: `cargo test` と `cargo clippy` が通り、`tests/ui_models.rs` がそのまま通る。
3. **`TranscriptionWindow` の新設**: `ui/transcription-window.slint` と
   `src/windows/transcription.rs`。自動文字起こしトグル・言語 ComboBox・whisper の `ModelList`・
   カタログ外行を配線し、設定画面に暫定の導線ボタンを足して開けるようにする。走査ハンドルは
   既存の共有ハンドルを使い回す。
   *完了条件*: 設定画面から開けて、Use / Download / Delete と言語・トグルが効く。
   `examples/transcription_view.rs -- snapshot` で潰れ・折り返しを確認済み。
4. **`MinutesWindow` の新設**: 同様に `ui/minutes-window.slint` と `src/windows/minutes.rs`。
   自動要約トグル・従属の注意行・LLM の `ModelList`・カタログ外行。
   *完了条件*: 上と同じ。`auto_transcribe` が OFF のとき注意行が出る。
5. **走査・ラッチの共有と tick の統合**: 両ウィンドウが開いていても走査が 1 回で済むことを確認
   （`ModelsRefresh::Poll` は走査しない、`Rescan` はラッチ変化時の 1 回だけ、という不変条件を
   doc コメントに書き直す）。確認モーダルが開いている間は tick が触らないことも維持する。
   *完了条件*: 両ウィンドウを開いた状態でダウンロードを走らせ、走査が増えないこと（ログか
   一時的なカウンタで確認）。
6. **`ModelsWindow` の廃止**: `ui/models-window.slint` と関連配線・`open-models-window` を削除。
   `ui/app-window.slint` の再エクスポートを差し替える。
   *完了条件*: `models-window` への参照がリポジトリに残っていない（`rg models-window`）。
7. **設定画面の縮小**: Transcription セクションを 2 ボタン＋状態要約行に置き換え、不要な
   プロパティ・コールバックを削除。`transcription_summary_text` / `minutes_summary_text` を
   実装し、tick の `is_visible()` 経路で差分更新する。`WINDOW_HEIGHT` と Slint 側の高さを
   スナップショットで実測して合わせる。
   *完了条件*: `examples/settings_view.rs -- snapshot` で余白・潰れが無く、`WINDOW_HEIGHT` と
   `ui/app-window.slint` の値が一致している。
8. **テストの組み替え**: `tests/ui_models.rs` → `tests/ui_transcription.rs` /
   `tests/ui_minutes.rs`、`tests/ui_settings_rollback.rs` の対象移動。
   *完了条件*: `cargo test` が通り、状態ごとのボタン出し分けの網羅 match が残っている。
9. **ドキュメント更新**: `README.md`（機能の説明 91 行目付近・ディレクトリ構成 170 行目付近）と
   `docs/CONTEXT.md`（#117 / #138 のモデル管理の段落、`ui/` の構成、要約の従属の記述）を
   新しい構造に書き直す。
   *完了条件*: `rg 'Manage Models'` が残っていない。CONTEXT の記述と実装が一致している。
10. **通しの目視確認**: `cargo run` で設定 → 各ウィンドウ → Use / Download / Delete →
    録音 → 自動文字起こし → 自動要約まで動かす。
    *完了条件*: モデル選択が一覧の Use だけで完結し、設定画面の要約行が追従する。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - 新規: `ui/model-list.slint`、`ui/transcription-window.slint`、`ui/minutes-window.slint`、
    `src/windows/mod.rs`、`src/windows/models.rs`、`src/windows/transcription.rs`、
    `src/windows/minutes.rs`、`examples/transcription_view.rs`、`examples/minutes_view.rs`、
    `tests/ui_transcription.rs`、`tests/ui_minutes.rs`
  - 変更: `ui/app-window.slint`、`src/main.rs`（大幅減）、`examples/settings_view.rs`、
    `tests/ui_settings_rollback.rs`、`README.md`、`docs/CONTEXT.md`
  - 削除: `ui/models-window.slint`、`examples/models_view.rs`、`tests/ui_models.rs`
  - 変更しない: `src/config.rs`（設定項目は不変）、`src/model_download.rs`、
    `src/whisper_model.rs`、`src/summary_model.rs`、`src/tray.rs`
- リスクと対策:
  - **走査が 2 倍になる**（両ウィンドウが独立に `installed_models()` を呼ぶ）。ステップ 5 で
    共有を確認し、「走査するのは 1 か所だけ」という不変条件を doc コメントに残す
    （`docs/rules/performance.md`）。
  - **カタログ外行の二重表示で削除が食い違う**（片方で消したのに他方が残る）。削除の直後は
    `ModelsRefresh::AfterOperation` で走査し直すので、開いている他方のウィンドウは次の tick で
    追従する。ここは実機で確認する。
  - **ジオメトリ確定の取りこぼし**（`docs/rules/slint.md`）。新ウィンドウ 2 つとも
    `show_window` を通し、初回フラグを個別に持たせる（`ModelsWindow` と同じ `RefCell<bool>`）。
  - **設定の巻き戻しが効かなくなる**。移設したトグル・ComboBox も `<=>` で束ね、保存失敗時に
    `set_*` で書き戻す（`docs/rules/slint.md`）。テストで固定する。
  - **要約の従属の挙動変更**（無効化 → 注意行）を CONTEXT に書き忘れると、次に読む人が
    「無効化が消えたのはバグ」と読む。ステップ 9 で必ず書く。
  - `ModelKind` を足したときに新ウィンドウを作り忘れる。`REGISTERED_CATALOGS` を並べ直さない
    不変条件は維持し、フィルタは種別を渡すだけの形にする。
  - **後続の #83 / #84 / #85 でエンジンの軸が増えたときに拡張できない形にしてしまう**。
    可否判定（`can_use_row` / `can_download_row` / `can_delete_row`）は Rust の純粋関数に
    閉じたままにし、Slint 側へは真偽値だけを渡す（現行どおり）。新ウィンドウの Slint に
    「オンデバイスなら…」のような条件分岐を持ち込まない。

## 未確定事項

- **画面デザイン全般**（2 ウィンドウの情報の並び、使う共通部品、設定画面の導線の見せ方）。
  ステップ 0 で #127 の成果を見てから決める。「実装方針」の並びは暫定案。
- #127 が設定画面の構成をどこまで変えるかによって、本プランの設定画面側の差分が変わる。
  #127 完了時点で「実装方針」の Slint 側の記述を読み直す。
- **#83（エンジン選択・API キー）との着手順**。上記のとおり本プランを先にするのを推奨するが、
  オンライン対応を急ぐなら #83 を先に入れて、本プランで API キー UI ごと移す判断もありうる。
  #127 の完了時点で決める。
- エンジン選択（#83 以降）が入ったとき、オンラインエンジン選択中のモデル一覧をどう見せるか
  （行ごと隠す／状態文言で「いまは使われない」と伝える）。本プランでは決めない。
- 設定画面の状態要約行の文言（`On — Whisper small, English` の形）と、モデル未取得時の扱い
  （状態も出すか、ウィンドウ側に任せるか）。ステップ 7 のスナップショットを見ながら決める。
- 新ウィンドウのサイズ定数（`TRANSCRIPTION_*` / `MINUTES_*`）と縮小後の `WINDOW_HEIGHT`。
  実測で決める。
- カタログ外ファイルのセクション見出し文言（両ウィンドウで同じにするか、「このアプリが管理して
  いないファイル」と明示するか）。
- 将来トレイメニューから各ウィンドウを直接開く導線を足すかどうか（今回はスコープ外）。
