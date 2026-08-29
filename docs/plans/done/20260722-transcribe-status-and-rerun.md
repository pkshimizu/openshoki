# 録音一覧に文字起こしの状態を表示し、再実行できるようにする

- 作成日: 2026-07-22
- ステータス: ドラフト

## 概要

Recordings ウィンドウで、各録音セッションの文字起こし状態（**文字起こし前 / 文字起こし中 /
完了 / 失敗**）を一覧と詳細ペインに表示し、詳細ペインの「Transcribe」ボタンで文字起こしを
（再）実行できるようにする。現状は「JSON があるか」の 2 値表示のみで、進行中・失敗が
見えず、失敗や設定変更（言語 #67 等）後のやり直し手段が無い。

## 背景・前提（コンテキスト）

- 文字起こしは `TranscribeWorker`（1 本の逐次ワーカー + `mpsc`）が録音停止時に実行する。
  **進行状態は外部へ公開されておらず**、失敗はログのみ。一覧の表示は `RecordingSession.
  has_transcript`（`mic.json` / `system.json` の有無、`list_sessions` 時点のスナップショット）。
- 一覧行には transcript 有無のドット（`row.has-transcript` で配色）が既にある
  （`ui/recordings-window.slint`）。詳細ペインには日時・音源サマリー・トランスクリプト・
  再生コントロールがある。
- Recordings ウィンドウ表示中は既存の 100ms tick（再生位置・現在セグメントの更新）が
  回っており、状態の定期反映に相乗りできる。
- ワーカーと UI の状態共有は `Arc<Mutex<...>>` を tick で読む流儀（#59 の DL 状況と同型）。
  Slint はメインスレッド専用のため、ワーカー側は共有状態の更新のみ行う。
- 手動実行時のモデル・言語は現在の設定（`whisper_model_path` / `transcribe_language`）の
  スナップショットを使う（既存 `submit_transcription` と同じ）。
- **#66（録音削除）が同じ詳細ペインにボタンを追加する予定**（未着手）。着手順を直列にして
  コンフリクトを避ける。

## 要件

- 一覧の各行に文字起こし状態を表示する（既存ドットを状態別の見た目へ拡張。例:
  完了=アクセント色 / 文字起こし中=進行中と分かる表示 / 失敗=警告色 / 前=非表示）。
- 詳細ペインに選択セッションの状態テキスト（`Not transcribed` / `Transcribing…` /
  `Transcribed` / `Transcription failed`）と「Transcribe」ボタンを表示する。
- 「Transcribe」は**常時実行可能**（文字起こし中のみ無効）。完了済みセッションでは既存
  JSON を上書きして作り直す（言語 #67 やモデル変更後の撮り直しが主用途）。
  `auto_transcribe` 設定が OFF でも手動実行できる。
- 録音停止時の自動文字起こしも同じ状態表示に反映される（ウィンドウを開いていれば
  「文字起こし中」→「完了」が見える）。
- 「失敗」は**メモリのみ**で保持する（アプリ再起動後は JSON の有無に基づき「前/完了」へ
  戻る。失敗理由はログで確認。再実行でリカバリできる）。
- 文字起こし完了時、そのセッションを選択中ならトランスクリプト表示を読み直す（再実行の
  結果がその場で見える）。
- スコープ外:
  - 失敗状態・失敗理由の永続化（マーカーファイル）。
  - 進行率（%）表示（whisper はセグメント逐次のため簡単な進捗が取りにくい。状態のみ）。
  - キューの可視化・キャンセル。
  - 音源ファイル単位の部分再実行。

## 確定した論点

ユーザー確認で決定:

1. **失敗はメモリのみ**（再起動で消える。ファイルを増やさずシンプルに、真実は JSON の有無）。
2. **再実行は常時可能**（完了済みも上書き。文字起こし中のみ無効）。
3. **表示は一覧行＋詳細ペイン**（一覧で俯瞰、詳細で操作）。

調査で確定:

4. 一覧行のドット・詳細ペイン・100ms tick・`Arc<Mutex>` 共有の既存基盤があり、
   状態表示は tick 相乗りで実現できる（新しい通知機構は不要）。
5. 「文字起こし中」には**キュー待ちを含める**（ワーカーは逐次のため、投入済み未処理も
   ユーザーから見れば進行中。状態を分けない）。

## 実装方針

- **状態の型（`src/transcribe.rs`）**:
  `enum TranscribeStatus { Transcribing, Done, Failed }` ＋
  共有マップ `Arc<Mutex<HashMap<PathBuf /* セッション dir */, TranscribeStatus>>>` を
  `TranscribeWorker` が所有し、読み取りハンドル（`status_of(dir)` / clone した Arc）を
  main へ渡す。
  - `submit` 時: 対象セッション dir を `Transcribing` に（キュー待ち含む）。
  - `run_job` 完了時: 全音源成功→`Done`、1 つでも失敗→`Failed`（理由はログ。既存の
    音源単位スキップのログをそのまま使う）。
  - マップに無いセッションの表示は `has_transcript ? Done : NotTranscribed` で解決する
    （表示用の 4 状態は UI 側で合成。`NotTranscribed` はマップに持たない）。
  - `TranscribeJob` に `session_dir: PathBuf` を追加（状態キー。`audio_paths` からの
    推測でなく明示）。
- **UI（`ui/recordings-window.slint`）**:
  - `SessionRow.has-transcript: bool` → `transcript-status: int`（0=なし/1=中/2=完了/3=失敗）へ
    置き換え、ドットの見た目を状態別にする（完了=アクセント、文字起こし中=控えめな色、
    失敗=警告色。色は `Style` に追加）。
  - 詳細ペインに状態テキスト＋「Transcribe」ボタン（`transcript-status == 1` で無効）。
    `callback transcribe-session(int)`。宣言グルーピング維持。
- **main.rs**:
  - `SessionRow` 組み立て時に状態を合成。Recordings ウィンドウ表示中の tick で共有マップを
    読み、**変化があった行だけ** Slint モデルを更新（`VecModel::set_row_data`）。選択中
    セッションの状態テキストも同時更新。`Done` へ変化した瞬間に、選択中ならトランスクリプトを
    読み直す。
  - `on_transcribe_session`: インデックス→セッション解決 → 存在する音源（mic/system）から
    `TranscribeJob` を組み立てて submit（モデル・言語は現在の config スナップショット。
    `auto_transcribe` は見ない）。
  - 既存の停止時 `submit_transcription` も `session_dir` を渡す形に追従（自動経路も同じ
    状態表示に乗る）。
- **文言**（英語・Title Case）: ボタン `Transcribe`、状態 `Not transcribed` /
  `Transcribing…` / `Transcribed` / `Transcription failed`。
- **ドキュメント同期**: README（Recordings の機能）・docs/CONTEXT.md（状態表示と再実行、
  失敗はメモリのみ、の一行）。

## 実装ステップ

1. **状態基盤**: `TranscribeStatus` と共有マップを `TranscribeWorker` に追加し、
   `submit` / `run_job` の前後で更新。`TranscribeJob.session_dir` を追加し、停止時の
   自動経路を追従。状態遷移の純粋部分（成功/一部失敗→Done/Failed の合成、表示 4 状態の
   解決）に単体テスト。
2. **一覧の状態表示**: `SessionRow.transcript-status` 化とドットの状態別描画。
   `open_recordings_window` での初期合成と、tick での差分更新。
3. **詳細ペインの状態＋再実行**: 状態テキスト・Transcribe ボタン・`on_transcribe_session`。
   実行中の無効化。完了時のトランスクリプト再読込。
4. **異常系**: 音源なしセッション（ボタン無効）、再実行中のセッション削除や選択変更で
   落ちないこと、失敗→再実行のリカバリ。
5. **ドキュメント同期＋仕上げ**: README・CONTEXT.md。`cargo build` / `cargo fmt --check` /
   `cargo clippy --all-targets -- -D warnings` / `cargo test`。実機で 自動/手動の両経路の
   状態遷移（前→中→完了、失敗→再実行→完了）を確認。

## 影響範囲・リスク

- **影響を受けるファイル/モジュール**:
  - 変更: `src/transcribe.rs`（状態共有・job フィールド）、`src/main.rs`（行の合成・tick
    更新・再実行コールバック）、`ui/recordings-window.slint`（SessionRow・状態表示・ボタン）、
    `ui/style.slint`（状態色）。README・docs/CONTEXT.md。
  - 変更なし: `src/recordings.rs`（`has_transcript` の走査はそのまま。表示合成は main 側）。
- **リスクと対策**:
  - **Mutex 競合**: tick（100ms）での読み取りはロック時間を短く（HashMap の clone は
    セッション数分の小データ。行数が多くても数百件想定で軽微）。ワーカー側の更新も
    ジョブ境界のみ。
  - **上書き再実行と読み取りの競合**: 再実行中に古い JSON が残ったまま（whisper 完了まで
    旧内容が見える）→ 意図どおり（原子的な `write_transcription` が完了時に置き換える）。
  - **状態とファイルの不整合**: `Done` なのに JSON が無い（手動削除等）ケースは、選択時の
    `load_transcript` が空を返し縮退表示（既存動作）。真実はファイル側にある前提を保つ。
  - **#66（削除）との順序**: 同じ詳細ペインを触るため直列化（本件と #66 の着手順は
    issue 化時点で調整。削除されたセッションの状態エントリはマップに残っても無害だが、
    #66 実装時に削除経路でエントリも消すのが望ましい—依存メモとして issue に書く）。
  - **ウィンドウ非表示中の状態変化**: tick 更新はウィンドウ表示中のみでよい（開いたときに
    `open_recordings_window` が最新を合成する）。

## 未確定事項

- 「文字起こし中」のドット表現（点滅 or 静的な色違い）は実装時に見た目で決める（点滅は
  録音アイコンの breathing 実装を流用できるが、過剰なら静的表示）。
- 状態テキストと Transcribe ボタンの詳細ペイン内の配置は実装時にレイアウトを見て決める。
