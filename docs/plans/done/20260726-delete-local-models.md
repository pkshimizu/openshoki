# ローカルモデルを削除できるようにする

- 作成日: 2026-07-26
- ステータス: ドラフト

## 概要

文字起こし（whisper）や将来の議事録要約（オンデバイス LLM）のためにダウンロードしたモデル
ファイルを、アプリから削除できるようにする。モデルは 1 つで最大 3.1GB あり、いくつか試すと
データディレクトリが数 GB 単位で膨らむが、今は Finder で
`~/Library/Application Support/net.noncore.openshoki/models/` を開いて手で消すしかない。
設定画面から開くモデル一覧ウィンドウを新設し、そこで何が容量を食っているかを見て削除できる
ようにする。

## 背景・前提（コンテキスト）

- モデル取得は**種別非依存の共有基盤** `src/model_download.rs` が持つ。`ModelSpec`
  （`kind` / `id` / `display_name` / `description` / `size_bytes` / `filename` / `url` /
  `sha256`）と `ModelDownloader`（`status_of` / `request_download` / `ensure_model`）。
- 保存先は `config::data_dir()/models/<filename>`。**全種別を同じ `models/` に混ぜて置き、
  `filename` は種別をまたいで一意**という規約（`ModelSpec::filename` の doc コメント）。
- whisper のカタログは `src/whisper_model.rs`（6 件・77MB〜3.1GB、既定 Small）。要約 LLM の
  カタログは未実装で、#78 が Qwen2.5-3B-Instruct GGUF（Q4_K_M・約 2GB）を検証中。
  `whisper_model.rs` の doc には「議事録要約 LLM のカタログは別モジュールが同じ形で持つ想定」と
  書かれている。
- 設定画面（`ui/app-window.slint`・**幅 420 固定**、高さもセクション構成から計算してコメントに
  残してある）のモデル UI は ComboBox 1 つと**選択中モデルの状態行 1 行**（`whisper-model-status`）
  だけ。状態文言は Rust の `selected_model_status_text` が組み立て、イベントループの 100ms tick で
  更新する（`src/main.rs`）。
- `DownloadStatus` は `NotDownloaded` / `Downloading { received, total }` / `Downloaded` /
  `Failed(String)`。`status_of` は**メモリの状態マップを優先**し、無ければディスクを stat して
  マップへ記録する。doc に「取得後にファイルを外部で消しても表示は Downloaded のまま」と
  明記されている。
- 別ウィンドウの前例は Recordings。`ui/recordings-window.slint` が
  `export component RecordingsWindow inherits Window` を出し、`ui/app-window.slint` が
  `export { RecordingsWindow, SessionRow } from "recordings-window.slint";` で再エクスポートする。
  `build.rs` は `ui/app-window.slint` だけをコンパイルするので、**新ウィンドウを足しても
  build.rs の変更は不要**。
- 削除の前例は録音セッションの削除。Slint 内で完結する確認モーダル（`show-delete-confirm`）＋
  `delete-session(int)` コールバック → `move_recording_to_trash`（macOS は `NsFileManager` 方式。
  Finder 方式は Automation 権限プロンプトとフルパスの子プロセス渡しを伴うため避けている。
  `docs/rules/security.md`）。文字起こし中のセッションは削除できない作りになっている。
- `config.whisper_model_path` は上級者向けのモデルパス上書き（設定 UI なし・手編集のみ）。
  `models/` の外の任意パスを指しうる。
- 関連する `docs/rules/slint.md` のルール: イベントループ稼働中に初めて `show()` するウィンドウは
  初回ジオメトリを明示する／件数が増えうる一覧は `ListView`（固定少数なら `ScrollView` + `for` で
  よい）／Rust ⇄ Slint の状態は int でなく `export enum`／状態→文言の対応表は Rust の網羅 match に
  置く／複数ウィンドウで使うトークンは `style.slint`／「失敗したら表示を更新しない」はポーリング
  tick の上書きまで考える／操作の検証は `tests/` のテストバックエンド・見た目は `examples/` ＋
  screencapture。

## 要件

- 設定画面にモデル一覧を開くボタンを追加し、押すとモデル一覧ウィンドウを表示する。
- モデル一覧ウィンドウで、**ディスクにあるモデル**を一覧できる。各行に表示名・種別・サイズ・
  状態を出し、末尾に合計使用量を出す。
- 各行から削除できる。削除は確認モーダルを経て、**ファイルを完全削除**する（ゴミ箱へは入れない）。
- 選択中（これから使う）モデルも削除できる。確認モーダルに「次回の文字起こしで再ダウンロード
  される」旨を出す。
- 文字起こし中・ダウンロード中のモデルは削除できない（Delete を無効化し、理由が分かる表示にする）。
- `models/` にあるカタログ外のファイルも一覧に出し、削除できる。
- 削除後、モデル一覧と**設定画面の状態行**が「未取得」に戻る。
- スコープ外:
  - 要約 LLM のカタログ追加そのもの（#78 → #80）。今回は種別非依存に作り、カタログが増えたら
    一覧に自動で並ぶようにするところまで。
  - モデルの明示的な再ダウンロードボタン（既存の「選択すると自動ダウンロード」を維持する）
  - `whisper_model_path` が指す外部ファイルの削除
  - 録音・文字起こし結果の削除（既存の Recordings 側）

## 確定した論点

**ユーザーへの確認で決まったこと**

- **UI は「設定画面のボタン → モデル一覧ウィンドウ → そこで削除」**: 設定画面は幅 420 固定で
  高さも計算済みのため一覧を抱えるのに向かず、Recordings と同じ「別ウィンドウ」の前例に乗れる。
  導線をトレイメニューではなく設定画面に置くのは、モデル選択のすぐ隣が自然だから。
- **完全削除にする**: カタログに URL と SHA-256 があるので再取得できる。ディスクを空けるのが
  目的なのにゴミ箱に数 GB 残るのは本末転倒。録音の削除（ゴミ箱へ移動）とは扱いを変える。
- **選択中モデルも削除できる**: 消しても `ensure_model` が次回利用時に再取得するので機能は
  壊れない。確認モーダルで再ダウンロードを明示する。
- **カタログ外のファイルも削除対象にする**: カタログを差し替えたときの旧ファイルを掃除できる。

**調査で決めたこと**

- **一覧は「ディスクにあるものだけ」を並べる**: カタログ全件を並べると未取得の行が並び、削除 UI
  としてはノイズになる。一覧の目的は「何が容量を食っているか」なので、`models/` の走査結果を
  正とし、カタログは表示名・種別の解決にだけ使う。この形なら未実装の要約 LLM も、カタログが
  増えた時点で自動的に並ぶ。
- **一覧と削除は基盤（`model_download`）に置く**: whisper 固有のモジュールに置くと、要約 LLM が
  来たときに同じコードが 2 つになる。基盤の doc が既に「1 つの `ModelSpec` を取ってきて置く
  ことだけを担う」と書いているので、そこに「置いたものを列挙して消す」を足すのが素直。
- **行数は `ScrollView` + `for` で足りる**: 取得済みのみなので通常 1〜3 行、カタログが増えても
  十数行。`docs/rules/slint.md` の「固定数行の小さな一覧は `ScrollView` + `for` のままでよい」に
  該当する。

## 実装方針

### 基盤（`src/model_download.rs`）に「列挙」と「削除」を足す

```rust
pub struct InstalledModel {
    pub filename: String,
    pub size_bytes: u64,                     // 実ファイルのサイズ（カタログ値ではない）
    pub kind: Option<&'static str>,           // カタログ外は None
    pub display_name: Option<&'static str>,
    pub catalog_id: Option<&'static str>,
}

pub fn installed_models(catalogs: &[&'static [ModelSpec]]) -> Vec<InstalledModel>;

impl ModelDownloader {
    pub fn delete(&self, filename: &str) -> Result<(), Box<dyn std::error::Error>>;
}
```

- `installed_models` は `models/` **直下の通常ファイルだけ**を走査し、`filename` でカタログを
  引いて名前・種別・ID を埋める。引けなければカタログ外として `None` を入れる。サイズは実
  ファイルの長さを使う（カタログの `size_bytes` ではない。途中で壊れたファイルの実サイズを
  見せたいため）。
- `delete` は `is_plain_filename` で検証してから `models/` 直下へ join し、`std::fs::remove_file`
  する。**同時に状態マップから該当 ID のエントリを消す**。`status_of` はメモリ優先なので、
  消さないと 100ms tick が「削除したのに Downloaded」と表示し続ける。対象が `Downloading` 中
  なら削除せずエラーを返す。
- サイズの表示は既存の `format_size` を使う。

### ウィンドウは Recordings と同じ形で足す

- `ui/models-window.slint` に `export component ModelsWindow inherits Window` を作り、
  `ui/app-window.slint` で再エクスポートする（`build.rs` は変更不要）。
- 初回 `show()` のジオメトリを `main.rs` の既存定数と同じ流儀で明示する（`docs/rules/slint.md`）。
- 行の状態は `export enum ModelRowState { Installed, InUse, Downloading, Unknown }` の形で渡し、
  表示文言は Rust の網羅 match で作る。enum から導ける bool を別プロパティで渡さない。
- 確認モーダルは Recordings の `show-delete-confirm` と同じ作りにし、対象の表示名とサイズ、
  選択中モデルなら再ダウンロードの注意を出す。

### 削除できない条件

- **文字起こし中**: whisper.cpp がファイルを掴んでいる可能性がある。macOS では unlink 自体は
  成功して開いているプロセスは読み続けられるため即クラッシュはしないが、状態としては不健全な
  ので Delete を無効化する（判定には既存の `transcriber.status_of` を使う）。
- **ダウンロード中**: 完了時のリネームでファイルが復活し、削除したつもりが残る。基盤側でも
  エラーにするが、UI でも無効化して押させない。

## 実装ステップ

### 1. 基盤に列挙と削除を足す（`src/model_download.rs`）

`InstalledModel` / `installed_models()` / `ModelDownloader::delete()` を追加する。走査と削除の
基点ディレクトリを引数で受けられる内部関数に切り出し、`data_dir()` に依存せずテストできる形に
する。

単体テストで次を固定する:

- カタログ内のファイルは名前・種別・ID が埋まり、カタログ外は `None` になる
- サブディレクトリとシンボリックリンクは一覧に出ない
- `../` や絶対パスを含む filename は削除を拒否する（`models/` の外へ出ない）
- 削除すると状態マップの該当エントリが消え、`status_of` が `NotDownloaded` を返す
- `Downloading` 中のモデルは削除できずエラーになる

**完了条件**: `cargo test` が通り、`models/` の外に触れないことがテストで示されている。

### 2. モデル一覧ウィンドウを作る（`ui/models-window.slint` / `src/main.rs`）

行（表示名・種別・サイズ・状態・Delete）、合計使用量、空表示、確認モーダルを作る。`main.rs` で
ウィンドウを生成して `installed_models()` の結果を Slint モデルへ流し、`delete-model(int)`
コールバックで基盤の `delete` を呼んで一覧と合計を再構築する。

**完了条件**: `cargo run` でウィンドウを開き、一覧・合計・削除・確認モーダルが動く。

### 3. 設定画面から開く導線（`ui/app-window.slint` / `src/main.rs`）

Model 行の下にボタンを置き、`open-models-window()` コールバックで既存の `show_window` を使って
表示・前面化する（Recordings と同じ流儀）。

**完了条件**: 設定画面のボタンでウィンドウが開く。ウィンドウで削除したあと、設定画面の状態行が
「未取得」表示へ戻る（状態マップのクリアが効いていることの確認）。

### 4. テストと見た目確認

- `tests/ui_models.rs`（テストバックエンド）: Delete 押下で確認モーダルが出る、確定で
  `delete-model` が 1 回・正しいインデックスで発火する、Cancel では発火しない、無効化条件では
  発火しない。100ms tick に依存する挙動はテストしない（`docs/rules/slint.md` の制約）。
- `examples/models_view.rs`（`examples/settings_view.rs` に倣う）＋ screencapture で、空・1 行・
  複数行・カタログ外あり・長い名前の縮退を確認する。

**完了条件**: `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` が
通り、screencapture で崩れがない。

### 5. ドキュメント同期

- README の機能説明にモデル削除を追記する。
- `docs/CONTEXT.md` の構成に `ui/models-window.slint` を足す。
- 繰り返しそうな知見（削除時は状態マップも消す）が `docs/rules/` に足すべきものなら追記する。

## 影響範囲・リスク

**影響を受けるモジュール**: `src/model_download.rs`（列挙・削除の追加）、`src/main.rs`
（ウィンドウ生成・配線・状態行の整合）、`ui/app-window.slint`（ボタンと再エクスポート）、
`ui/models-window.slint`（新規）、`tests/ui_models.rs`（新規）、`examples/models_view.rs`（新規）、
`README.md`、`docs/CONTEXT.md`。

**リスクと対策**:

- **使用中ファイルの削除**: 文字起こし中の削除は不健全なので Delete を無効化する（上記）。
- **状態表示が戻らない**: `status_of` はメモリ優先。削除で状態マップを消し忘れると「消したのに
  Downloaded」と出る。基盤側の削除でマップ操作まで済ませ、テストで固定する。
- **`models/` の外を消す**: filename を UI 経由で受けるため、`is_plain_filename` の検証を通して
  から `models/` 直下へ join する。走査・削除の対象は通常ファイルのみ（ディレクトリ・シンボリック
  リンクは対象外）。テストで担保する。
- **上書き設定の外部ファイル**: `whisper_model_path` が指すファイルは `models/` の外にあるため、
  走査が `models/` 直下限定である限り自然に対象外になる。
- **誤って大きいモデルを消して再取得**: 3.1GB の再ダウンロードは分オーダーかかる。確認モーダルに
  サイズと再ダウンロードの注意を出す。
- **ログにフルパスが出る**: 既存の `trash_error_kind` と同じ流儀で、失敗ログにはパスを含めない
  （`docs/rules/security.md`）。
- **要約 LLM の実装（#80）との接続**: 今回は種別非依存に作るが、実際に並ぶかは LLM カタログが
  できるまで確認できない。`installed_models()` の引数にカタログを足すだけで済む形にしておく。

## 未確定事項

- ボタンとウィンドウの文言（"Manage models…" / "Models" / "Downloaded Models" など）。実装時に
  screencapture を見て決める。
- 種別の見せ方（行に "Whisper speech" を出すか、種別で見出しを分けるか）。2 種別になる #80 の
  時点で判断してよい。
- カタログ外ファイルの表示名の文言（"Unknown model file" など）と、サイズ以外に出せる情報の有無。
