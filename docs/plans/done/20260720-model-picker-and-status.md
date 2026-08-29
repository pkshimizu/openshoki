# 文字起こしモデルの選択・情報表示・ダウンロード状況表示

- 作成日: 2026-07-20
- ステータス: ドラフト

## 概要

設定ウィンドウで、文字起こしに使う whisper モデルを複数の選択肢から選べるようにし、
使用中モデルの情報（名前・サイズ・精度/速度の説明）と、モデルのダウンロード状況
（未取得／ダウンロード中の進捗／取得済み／失敗）を表示する。内蔵モデル方式（#58）を
「固定 1 種（small）」から「カタログから選択」へ拡張する。

## 背景・前提（コンテキスト）

- **#58（whisper モデルの内蔵化）に依存**。`src/whisper_model.rs` が単一モデル
  （ggml-small）の URL・SHA-256 を定数で持ち、初回の文字起こし時に自動ダウンロード
  （SHA-256 検証・プロセス固有一時ファイル→rename の原子的配置・タイムアウト・サイズ上限）
  して `<データディレクトリ>/models/` へ保存する。本プランはこの機構をカタログ化して拡張する。
  **#58 のマージ後に着手する。**
- ダウンロードは文字起こしワーカー（1 本・逐次）上の `ensure_model()` で同期実行され、
  進捗はログのみ。UI へ進捗を渡す仕組みはまだ無い。
- UI 更新の既存パターン: メインループの 100ms タイマー（メニューイベントのポーリング）に
  相乗りして Slint プロパティを更新する。
- 設定は `Config`（TOML）。`whisper_model_path` は上級者向けの上書き（内蔵より優先）として
  存在し、本プランでも維持する。
- `docs/rules/security.md`「外部からの大容量ダウンロード」の定型（SHA-256 ピン・原子的配置・
  タイムアウト・上限・オプトイン・透明性）に従う。

## 要件

- **モデルカタログ（6 種）**から使用モデルを選択できる（設定画面の ComboBox）:

  | ID | 表示 | サイズ | 説明（UI 文言の元） |
  |----|------|--------|----------------------|
  | tiny | Tiny | 74 MB | Fastest, lowest accuracy |
  | base | Base | 141 MB | Fast, basic accuracy |
  | small | Small | 465 MB | Balanced speed and accuracy（**既定**） |
  | medium | Medium | 1.4 GB | High accuracy, slower |
  | large-v3-turbo | Large v3 Turbo | 1.5 GB | High accuracy, faster than Large |
  | large-v3 | Large v3 | 2.9 GB | Highest accuracy, slowest |

- 使用中モデルの**情報表示**: 名前・サイズ・精度/速度の説明。
- **ダウンロード状況の表示**: Not downloaded / Downloading（進捗 %）/ Downloaded / Failed（理由）。
- モデルを**選択したら即バックグラウンドでダウンロード開始**し、進捗を設定画面に表示する。
  取得済みモデルへの切替は即時反映（DL なし）。
- 選択は `Config` に永続化し、再起動後も保持。文字起こしは選択中モデルで実行する。
- スコープ外:
  - 量子化モデル（q5_1 等）・カタログ外モデルの追加 UI（`whisper_model_path` 上書きで代替可能）。
  - 取得済みモデルの削除 UI（ディスク管理）。
  - ダウンロードの一時停止・再開（レジューム）。
  - 言語設定の UI（従来どおり config 手編集）。

## 確定した論点

ユーザー確認で決定:

1. **モデル候補は 6 種**（tiny / base / small / medium / large-v3-turbo / large-v3）。
2. **選択時に即ダウンロード開始**（次回文字起こし時まで遅延しない）。「ダウンロード状況表示」の
   要件と噛み合い、初回の文字起こしが DL 待ちで遅れるのも防げる。
3. **表示は 名前・サイズ・状態 に加えて精度/速度の説明も付ける**。

調査で確定:

4. **全 6 モデルの正式サイズ・SHA-256 は HuggingFace の LFS メタデータから取得済み**
   （実装で定数テーブル化する。値は下記「モデル定数」）。
5. **進捗の総量は Content-Length**（ureq の応答ヘッダ）から得る。ヘッダが無い場合は
   カタログの既知サイズを分母に使う。
6. **UI への進捗伝達は既存の 100ms タイマー相乗り**。ワーカー側は共有状態
   （`Arc<Mutex<DownloadStatus>>`）を更新するだけにし、Slint へは触らない
   （Slint はメインスレッド専用のため）。

### モデル定数（実装で使う正式値）

```
tiny            77691713 bytes  be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21
base           147951465 bytes  60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe
small          487601967 bytes  1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b
medium        1533763059 bytes  6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208
large-v3-turbo 1624555275 bytes 1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69
large-v3      3095033483 bytes  64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2
```
URL はいずれも `https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-<id>.bin`。
`MAX_DOWNLOAD_BYTES` は最大の large-v3（約 2.9GB）を収める値へ引き上げる（モデルごとの
既知サイズ＋余裕で個別に上限を出すのがより厳密）。

## 実装方針

- **`whisper_model.rs` をカタログ化**:
  - `struct ModelSpec { id, display_name, filename, url, sha256, size_bytes, description }` の
    `const CATALOG: &[ModelSpec]`（6 件）。`fn spec_for(id: &str) -> Option<&ModelSpec>`、
    不明 ID は既定（small）へフォールバック。
  - `ensure_model(spec)` に一般化（現行の単一定数を spec 参照に置換。検証・原子的配置・
    タイムアウトの機構はそのまま）。
- **ダウンロード管理 `ModelDownloader`（whisper_model.rs 内）**:
  - 状態: `enum DownloadStatus { NotDownloaded, Downloading { received: u64, total: u64 }, Downloaded, Failed(String) }` を
    モデル ID ごとに `Arc<Mutex<HashMap<...>>>`（または「アクティブ DL は常に 1 つ」として単一状態＋対象 ID）で保持。
  - `request_download(id)`: 未取得なら DL スレッドを起動（既に同 ID を DL 中なら何もしない）。
    受信ループ内で `received` を定期更新。完了/失敗で状態を確定。
  - 文字起こし側の `ensure_model` も同じ状態を共有し、**同一モデルを二重にダウンロードしない**
    （DL 中なら完了を待つ。ワーカーは逐次なのでブロックで良い）。
- **`Config` に `whisper_model: String` を追加**（既定 `"small"`、`#[serde(default)]`）。
  不正 ID は使用側で既定へフォールバック（debounce の寛容デシリアライズと同じ思想。
  文字列なのでパース失敗はなく、使用側フォールバックで足りる）。
  優先順位: `whisper_model_path`（上書き）> `whisper_model`（カタログ選択）。
- **UI（`ui/app-window.slint`）**: 文字起こしトグルの配下に:
  - `ComboBox`（6 件。表示例: `Small — 465 MB — balanced speed and accuracy`）。
    `enabled: auto-transcribe` で従属を示す（既存 delay と同様）。
  - 状態行 `Text`（例: `Downloaded` / `Downloading 42%` / `Not downloaded` /
    `Download failed: <理由>`）。100ms タイマーから `set_model_status(...)` で更新
    （文字列は Rust 側で組み立て、秒単位の変化時のみ set して無駄な再描画を避ける）。
  - 既存の「First use downloads …」注記は、選択時 DL に合わせて文言を見直す
    （例: `Models download from Hugging Face — audio is never uploaded`）。
  - 選択変更コールバック `change-whisper-model(int/index)` → Rust 側で ID へ変換し、
    保存成功後に `request_download(id)`。保存失敗時は ComboBox を書き戻す
    （`docs/rules/slint.md` の in-out 巻き戻し）。
  - ウィンドウ高さを内容に合わせ再調整（`.slint` と `WINDOW_HEIGHT` を両方）。
- **文字起こし経路**: `TranscribeJob` に `model_id`（設定スナップショット）を追加し、
  `run_job` で `model_override.or(spec_for(model_id))` を解決。DL 中なら完了待ち。

## 実装ステップ

1. **カタログ化**: `ModelSpec` と 6 件の定数テーブル、`spec_for` と既定フォールバックを実装
   （単体テスト: 既知 ID の解決・不明 ID のフォールバック）。`ensure_model` を spec 引数に一般化し、
   既存の #[ignore] DL テスト・write_verified テストを通す。
2. **Config**: `whisper_model: String`（既定 "small"）を追加。ラウンドトリップ・旧 config 互換・
   不明 ID フォールバックのテスト。
3. **ダウンロード状態の共有**: `DownloadStatus` と `ModelDownloader`（request_download・
   状態照会・二重 DL 防止）を実装。受信ループの進捗更新（Content-Length or 既知サイズ）。
   純粋部分（進捗率計算・状態遷移）に単体テスト。
4. **文字起こし経路の接続**: `TranscribeJob.model_id` を追加し、`run_job` の解決を
   override > カタログ選択 に。DL 中の完了待ち。既存 E2E（override 経路）を再確認。
5. **UI**: ComboBox・状態行・注記の変更、選択コールバック（保存→DL 開始→失敗時巻き戻し）、
   100ms タイマーからの状態反映。高さ調整（`.slint`/`WINDOW_HEIGHT` 一致）。
6. **実機確認**: 小さいモデル（tiny/base）への切替→即 DL 開始→進捗表示→Downloaded 表示→
   録音停止で新モデルの文字起こしが走ること。取得済み（small）への切替が即 Downloaded に
   なること。DL 失敗（ネットワーク断）で Failed 表示になり、アプリが落ちないこと。
7. **仕上げ**: `cargo build` / `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
   `cargo test`。README・CONTEXT.md の記述（モデル選択・状況表示）を同期。

## 影響範囲・リスク

- **影響を受けるファイル/モジュール**:
  - 変更: `src/whisper_model.rs`（カタログ化・状態管理・DL 一般化）、`src/config.rs`
    （`whisper_model` 追加）、`src/transcribe.rs`（model_id の解決）、`src/main.rs`
    （コールバック・タイマーからの状態反映・`WINDOW_HEIGHT`）、`ui/app-window.slint`
    （ComboBox・状態行）。README・docs/CONTEXT.md。
- **リスクと対策**:
  - **二重ダウンロード**: UI 起点の DL と文字起こし起点の `ensure_model` が同時に走る競合。
    → 状態を一元管理し「DL 中なら待つ/相乗りする」ことで直列化（プロセス固有一時ファイルに
    よる多重起動安全は #58 のまま維持）。
  - **DL 中のモデル切替**: 進行中の DL をキャンセルするか完走させるか。v1 は**完走させて状態だけ
    切替後モデルへ向ける**（キャンセル機構はスコープ外。切替先の DL は前の DL 完了後に開始）。
    実装が複雑になるようなら「DL 中は ComboBox を無効化」に縮退してよい。
  - **大モデルのディスク圧迫**: large-v3 は約 2.9GB。カタログの説明にサイズを明示して
    ユーザー判断に委ねる（削除 UI はスコープ外として issue 候補）。
  - **メインスレッドの Mutex 競合**: 100ms タイマーが状態 Mutex を読む。ロックは短時間
    （enum のコピーのみ）に保ち、DL スレッド側も進捗更新を間引く（例: 1MB ごと）。
  - **Slint ComboBox の挙動**: 既定スタイルでの表示幅・長い項目名の描画を実機確認
    （崩れる場合は表示名を短縮）。

## 未確定事項

- DL 中のモデル切替の詳細挙動（完走 or ComboBox 無効化）は実装時の複雑さで決める（上記）。
- 状態表示の更新頻度（毎ティック vs 変化時のみ）は実装時に負荷を見て調整。
- 取得済みモデルの削除 UI は将来 issue（本プランのスコープ外）。
