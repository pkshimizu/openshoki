# 文字起こしの言語を設定できるようにする（既定は英語）

- 作成日: 2026-07-21
- ステータス: ドラフト

## 概要

文字起こしの認識言語を設定ウィンドウから選べるようにする。既定は**英語**とし、
主要 8 言語＋自動判定から ComboBox で選択できるようにする。現状は `config.toml` の
手編集（`transcribe_language`、未指定は自動判定）でしか変えられず、既定も自動判定に
なっているのを、UI 化と同時に「既定＝英語」へ仕様変更する。

## 背景・前提（コンテキスト）

- 文字起こし（#30）は録音停止時にオンデバイス whisper で実行される（オプトイン）。
  認識言語は `Config.transcribe_language: Option<String>`（`#[serde(default)]`、
  `None`＝whisper の自動判定）で、**設定 UI は無く config 手編集のみ**。
- `src/transcribe.rs` は `job.language` を `set_language` に渡す（NUL バイトは弾いて
  自動判定へフォールバック。未知の言語コードは whisper.cpp 側が検証して `full()` が
  `Err` を返し、当該音源をスキップする）。保存する JSON の `language` フィールドは
  指定値または `"auto"`。
- 設定ウィンドウには文字起こしトグル（`auto_transcribe`）と初回ダウンロードの注記がある。
  既存の設定コールバックは「永続化成功後に反映・失敗時は表示を書き戻す」パターンで統一
  （`docs/rules/slint.md` / `error-handling.md`）。
- **#59（モデル選択 ComboBox）が同じ「文字起こし」節を触る予定**（未着手）。本件と #59 は
  機能的に独立だが、同じ画面領域のため着手順を直列にしてコンフリクトを避ける。

## 要件

- 設定ウィンドウの文字起こしトグル配下に「Language」の ComboBox を追加する
  （トグル OFF 時は無効化。既存の従属コントロールの流儀に合わせる）。
- 選択肢は**主要 8 言語＋自動判定**（表示名 / whisper 言語コード）:

  | 表示 | コード |
  |------|--------|
  | English（**既定**） | en |
  | Japanese | ja |
  | Chinese | zh |
  | Korean | ko |
  | Spanish | es |
  | French | fr |
  | German | de |
  | Portuguese | pt |
  | Auto detect | auto |

- **既定は英語（`en`）**。言語未指定の既存 `config.toml` も英語扱いになる
  （従来の自動判定に戻したい場合は UI で Auto detect を選ぶ）。
- 選択は `Config` に永続化し、再起動後も保持。文字起こしは選択言語で実行する
  （`auto` は whisper の自動判定）。
- スコープ外:
  - 全 99 言語の選択肢（テーブルに 1 行足すだけで将来追加できる構造にはする）。
  - 翻訳（whisper の translate 機能）。
  - 保存済み JSON の `language` フィールドの形式変更。

## 確定した論点

ユーザー確認で決定:

1. **選択肢は主要 8 言語＋自動判定**（上表）。
2. **既存 config で言語未指定の場合も英語になる**（新しい既定に統一。挙動を分岐させる
   状態管理を持ち込まない）。

調査で確定:

3. 既存フィールドは `Option<String>` だが、TOML 表現は `transcribe_language = "ja"` の
   有無なので、**`String`（既定 `"en"`、`"auto"` を自動判定の特別値とする）へ型変更しても
   旧 config はそのまま読める**（未指定→serde default で `"en"`、指定あり→同じ文字列）。
4. 未知の言語コード（手編集）は whisper.cpp が検証して失敗ログになる既存動作を維持する
   （config の値検証を UI 側の責務にし、手編集値はそのまま渡す。NUL ガードは既存のまま）。
5. UI の ComboBox は既知 9 値のみをマッピングする。config に catalog 外の値が手編集で
   入っていた場合、**動作にはその値を使い続け**、ComboBox 表示は English 位置に
   フォールバックする（ユーザーが ComboBox を操作した時点で上書き保存される）。

## 実装方針

- **言語カタログ（`src/transcribe.rs` か `src/config.rs` に定数テーブル）**:
  `const LANGUAGES: &[(code, display)]`（9 件）。UI の表示列・インデックス⇔コード変換・
  既定（`"en"`）解決を 1 箇所に集約する。#59 のモデルカタログと同じ考え方。
- **`Config`**: `transcribe_language: Option<String>` → **`String`（`#[serde(default = "en")]`）**
  へ変更。ラウンドトリップ・旧 config（未指定→"en"、"ja" 指定→保持）のテストを更新。
- **`transcribe.rs`**: `TranscribeJob.language: Option<String>` → `String` に変更。
  `"auto"` のとき `set_language` を呼ばない（現行の `None` と同じ経路）。JSON の
  `language` はそのまま設定値（`"auto"` 含む）を書く（現行出力と互換）。
- **UI（`ui/app-window.slint`）**: 文字起こしトグル配下に
  `Language` ラベル＋`ComboBox`（`model: ["English", "Japanese", …, "Auto detect"]`、
  `enabled: root.auto-transcribe`）。プロパティ/コールバックの宣言グルーピング維持。
  コールバックはインデックスを渡し、Rust 側でコードへ変換して保存（成功後反映・失敗時
  巻き戻し。既存 debounce/トグルと同型）。ウィンドウ高さを再調整（`.slint` と
  `WINDOW_HEIGHT` を両方）。
- **ドキュメント同期**: README（文字起こし節に言語設定を追記）・docs/CONTEXT.md
  （`transcribe_language` の記述を「UI で選択・既定 en」に更新）。

## 実装ステップ

1. **言語カタログと Config 変更**: `LANGUAGES` テーブル、`transcribe_language: String`
   （既定 "en"）への移行、インデックス⇔コード変換ヘルパ。テスト（ラウンドトリップ・
   旧 config 未指定→"en"・指定保持・catalog 外コードの表示フォールバック）。
2. **transcribe 経路**: `TranscribeJob.language: String` 化、`"auto"` の分岐、既存の
   NUL ガード・E2E（`#[ignore]`）の追従。`cargo test` 通過。
3. **UI**: ComboBox 追加・初期値反映・変更コールバック（保存→反映・失敗時巻き戻し）・
   高さ調整。
4. **ドキュメント同期**: README・CONTEXT.md。
5. **仕上げ**: `cargo build` / `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
   `cargo test`。実機で 言語を Japanese に変更→録音→停止→`mic.json` の `language` が
   `ja` で日本語文字起こしになることを確認。既定（未操作）で英語認識になることを確認。

## 影響範囲・リスク

- **影響を受けるファイル/モジュール**:
  - 変更: `src/config.rs`（フィールド型変更・カタログ or ヘルパ）、`src/transcribe.rs`
    （`language: String` 化）、`src/main.rs`（コールバック・初期反映・`WINDOW_HEIGHT`）、
    `ui/app-window.slint`（ComboBox）。README・docs/CONTEXT.md。
- **リスクと対策**:
  - **既定変更の互換性**: これまで自動判定だった未指定ユーザーが英語固定になる。仕様として
    ユーザー確認済み（Auto detect を選べば戻せる）。PR 本文に挙動変更として明記する。
  - **`Option<String>` → `String` の型変更**: TOML 表現は互換（調査で確定）。
    `whisper_model_path` など他の `Option` フィールドには触れない。
  - **#59 とのコンフリクト**: 同じ設定画面の文字起こし節を触るため、#59 より先に本件を
    完了させる（または #59 側でリベース）。issue の依存欄に相互関係を明記。
  - **ComboBox の既定スタイル表示**: 9 項目のドロップダウンが 420px 幅で収まるか実機確認
    （表示名は短い英単語なので問題ない想定）。

## 未確定事項

- 言語カタログの置き場所（`config.rs` か `transcribe.rs` か）は実装時に凝集度で決める
  （UI 変換と whisper 渡しの両方から参照するため、`config.rs` 側が有力）。
- #59（モデル選択）との着手順は issue 化時点の状況で決める（本件が小さいので先行が有力）。
