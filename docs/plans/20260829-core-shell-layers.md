# core / shell の層を入れる

- 作成日: 2026-08-29
- ステータス: 確定

## 概要

状態を持つ場所を 1 つにし、判断を副作用から引き剥がす。「セッション X のいまの状態は何か」に
答える関数がいま 6 つあり、それぞれ独自の優先順位を持っていることが、直近 4 本の PR で
レビューが収束しなかった原因だった。純粋な `core`（クレート `shoki-core`）と副作用のある
`shell`（いまの `shoki`）に分け、状態は 1 つの `AppState` に畳む。

## 背景・前提（コンテキスト）

### 測ったこと

PR あたりのコミット数（≒レビュー往復）が 8 月下旬に不連続に跳ねている。

| 時期 | PR | コミット数 | 最終差分 |
|---|---|---|---|
| 7/25〜8/22 | #101〜#177（35 本） | 3〜4 でほぼ一定 | 27〜4550 行 |
| 8/26 | #180 | 8 | 554 |
| 8/26 | #183 | 10 | 1189 |
| 8/26 | #185 | 18 | 1578 |
| 8/29 | #186 | 8 | 2275 |

**差分の大きさでは説明できない。** #149 は 4550 行で 4 コミット、#180 は 554 行で 8 コミット。
相関しているのは規模ではなく「何を触ったか」。同一 PR 内の手戻り率（各コミットの行数合計に
対する最終差分の割合）は #180 が 59%、#185 が 38%。

### 往復の中身は 4 PR 連続で同じ 1 種類

`#178` → `#182` → `#175` → `#176` のコミット件名を並べると、いずれも
**「本番だけが通る 1 行が、テストから呼べない位置にある」** という同じ形の指摘だった。
`docs/rules/testing.md` に 4 回書き足したが再発しているので、ルールで直る種類の問題ではない。

### 原因

この 4 本はすべて同じ経路を通る issue だった。

```
デコーダ → ジョブ結果 → 状態マップ(Mutex) → ペイン状態 → Slint プロパティ
   ↓          ↓             ↓                  ↓
 whisper    ディスク      ワーカー          ウィンドウ
```

5 ホップあり、**どのホップも副作用に直結している**。純粋な判断だけを取り出せる層が無いので、
テストを書こうとするたびに「呼べる一番外側」を探す作業が発生し、見つからなければ継ぎ目を
切り出す → 穴が 1 段上がる → 次の周で同じ指摘、を繰り返す。

「文字起こしは完了したか」という 1 つの問いに答える関数が 6 つある。

- `main::transcript_display_status`（一覧の行）
- `main::transcript_pane_of`（詳細ペイン）
- `main::summary_display_status`（議事録の行）
- `main::summary_pane_of`（議事録ペイン）
- `main::LoadedTranscript::stored`（ディスクの印）
- `reading_pane::TranscriptInput::of`（議事録から見た入力）

依存も一方向でない。`transcribe` と `summarize` が相互参照し、`reading_pane` に 4 つの
モジュールが型を預けている。

## 要件

- 「セッション X のいまの状態は何か」に答える場所を 1 つにする
- 判断（保存してよいか・議事録を続けてよいか・揃っているか）を副作用の外へ出し、ディスクも
  whisper もスレッドもウィンドウも使わずにテストできるようにする
- 常に動く状態を保ったまま移行する。各段階でビルドでき、テストは緑で、リリースできる

**スコープ外**:

- 言語・フレームワークの変更（Rust + Slint のまま。A 案）
- 捕捉と再生（`recorder` / `system_audio` / `app_audio_monitor` / `mixdown` / `player`、
  本番 1704 行）には手を入れない。いま問題を起こしていない
- 段階 03〜05（議事録の状態・保存判断の押し出し・`main.rs` の解体）はこのプランに設計を
  書くが、着手は機能開発の合間に行う

## 確定した論点

すべて 2026-08-29 に決定。

### 0. 層の名前は `core` / `shell`

訳語（「芯」「殻」「コア層」）は使わない。クレート名 `shoki-core` と対応する語なので、原語の
まま扱うのがこのプロジェクトの規約（`docs/CONTEXT.md` の Language に登録済み）。

`core` はモジュール名にはしない（Rust の sysroot の `core` と紛らわしい。ハイフン付きの
クレート名なら衝突しない）。

### 1. 状態は 1 つの `AppState`。`view` は 3 つの粒度

派生は**同一性だけ**を持つ。`search.matched` は `Vec<SessionDir>` で、行のコピーを持たない
——一覧を「全部」と「絞り込み後」で二重に持てないので、#161（削除しても検索を解除すると
戻ってくる）が構造的に起きない。

| 粒度 | 関数 | いつ呼ぶか |
|---|---|---|
| 1 件ぶん（軽い） | `view_tray` / `view_detail` / `view_settings` / `view_models` | 毎 tick 組んでよい |
| 行（n 件） | `row_key(&state, dir) -> RowKey`（Copy・安い）<br>`view_row(&state, dir) -> Row`（文字列を組む） | shell が `RowKey` で差分を取り、変わった行だけ `view_row` を呼ぶ |
| 本文（重い） | `view_segments(&state)` | `loaded` の `(dir, generation)` が変わったときだけ |

**変更追跡（どこが変わったかを `update` が返す形）は入れない。** フラグを立て忘れると古い表示が
残る——性能リスクが正しさリスクに変わる。差分を取る形なら、落としても遅くなるだけで、表示は
常に状態から導かれる。

守り方: 「`row_key` が同じなら `view_row` の出力も同じ」を `core` のテストで固定する。いまは
#162 の「走っていない行は文言を組み直さない」がコメントと `if` でしか守られていない。

### 2. `Effect` は `core` が返す。`core` は別クレートにする

「enum が肥大する／継続的な効果を表せない」という懸念は成り立たなかった。**継続的なものは
`Effect` ではない**——進捗や部分結果は「すでに走り出した効果の出力」なので `Event` として流れる。
全操作を列挙すると `Effect` は 14 個で、`main.rs` の 88 本のコールバック配線がここへ畳まれる。

`decide` と `reduce` は `update` 1 本に統合する。押した瞬間に状態と `Effect` を同時に決めないと、
その間に状態が「何も起きていない」と答える窓が開く（#163 がその窓のバグだった）。ジョブの
通番も `update` の中で採番する。

向きを守るのは借用ではなく**クレート境界**。`shoki-core` は `shoki` に依存しないので、
`core` からのディスク読み取りはコンパイルが通らなくなる。

**例外**: リアルタイム捕捉は輪の外。録音スレッドはサンプルを `core` に通さず、粗い `Event`
（開始・経過・停止）だけを戻す。

### 3. 並走させない。PR 単位で切り替える

恒久的な並走もフラグ切替も影実行もしない。作業中は両方あってよいが、その PR の最後で旧経路を
消し、マージ時点では常に 1 本にする。

**影実行が不要な理由**: 等価性の確認は、すでに持っているテストがやる。#164〜#176 で積み上げた
状態解決と文言のテストは、ミューテーションで「壊すと落ちる」ことまで確かめてある。それを
`update` / `view` へ書き直せば、それが等価性の検査になる。加えて**まだ配布していない**
（#109 未着手）ので、「常にリリースできる」ことの価値がいまは低い。

**消し忘れは移動が防ぐ**: 段階 01 で `src/reading_pane.rs` は `shoki-core` へ*移動*するので、
元のファイルが無くなり、旧経路が参照していた型ごと消える。`pub` な関数は使われなくなっても
dead_code 警告が出ないという既知の穴（`docs/rules/testing.md`）にも当たらない。

### 4. 段階 00 → 01 → 02 を先に通し、その間は機能開発を止める

Ready の 12 件を移行のどの段階に当たるか読み合わせた結果:

| 段階 | 当たる Ready | 中身 |
|---|---|---|
| **01** | **#181** | issue 本文が「走査を別スレッドへ、世代を持たせる」——`Effect::ScanSessions { generation }` → `Event::Scanned` そのもの |
| **02** | **#173** / #150 | 「取得と推論スロットの待ちの最中に中断フラグを見ていない」。キュー・取り消し・スロットを 1 本が持てば構造的に解ける |
| **03** | #184 / #83〜#85 | #184 は議事録の状態と `summary.md` の形式を触る。オンライン LLM は adapter 差し替え |
| **05** | #151 / #126 | #151 の通知は `windows/models.rs` にあるので段階 01〜02 ではなくここ |
| 独立 | #143 / #108 / #121 | どこでもよい。#143 は移行で触る場所が動くので後のほうが安い |

止める期間は「3 PR ぶん」。まだ配布していないので外部への影響はない。そのあと Ready に戻り、
先頭が #184 なので、その直前に段階 03 を通すかを判断する。

**段階 01 の PR では、着手前の設計攻撃を必須にする。** #176 で工程あたりの費用対効果が最も
高かったのがこれ（実装前に blocker 5 件を潰した。実装後のレビュー 3 周で見つかった blocker は
2 件）。段階 01 は触る層の数が最大なので、必ず先に叩く。

## 実装方針

### 層

```
shell（副作用がある層）
  UI（Slint 配線）／ job runtime ／ adapters
        ↓ Command / Event        ↑ Effect / View
core（純粋な層。I/O なし・スレッドなし・Slint なし）
  update(&mut AppState, Msg) -> Vec<Effect>
  view_*(&AppState) -> ...
```

**下り**は `decide` 相当（状態は読むだけ）、**上り**は `reduce` 相当（状態を変える唯一の口）だが、
関数は `update` 1 本にまとめる。UI は状態を読まず、adapters は状態を知らない。

### 型の骨格

```rust
enum Msg { Command(Command), Event(Event) }

enum Event {
    Scanned       { dir: SessionDir, on_disk: DiskFacts },
    JobQueued     { job: JobId, dir: SessionDir, kind: JobKind, position: usize },
    JobProgressed { job: JobId, percent: u8 },
    JobFinished   { job: JobId, outcome: JobOutcome },
    Deleted       { dir: SessionDir },
    // ほか、録音・モデル取得・検索・読み込みの事実
}

enum Effect {
    // 録音（輪への入口だけ。サンプルは core を通らない）
    StartRecording { dir: SessionDir },
    StopRecording,
    // 重いジョブ（走り出したら進捗と完了は Event で戻る）
    RunTranscription { job: JobId, dir: SessionDir, sources: Vec<Speaker>, model: ModelId },
    RunSummary       { job: JobId, dir: SessionDir, model: ModelId },
    DownloadModel    { job: JobId, model: ModelId },
    CancelJob(JobId),
    // 読み取り（結果は Event で戻る。世代つき）
    ScanSessions   { generation: u64 },
    LoadSession    { dir: SessionDir, generation: u64 },
    SearchSessions { needle: String, generation: u64 },
    // 書き込み
    WriteTranscript { dir: SessionDir, source: Speaker, body: Transcription },
    WriteSummary    { dir: SessionDir, body: String },
    DeleteSession   { dir: SessionDir },
    SaveConfig(Config),
    // 画面
    ShowWindow(WindowKind),
}

struct AppState {
    sessions: Vec<Session>,               // 走査で得た全件（新しい順）
    by_dir: HashMap<SessionDir, usize>,
    selected: Option<SessionDir>,
    loaded: Option<Loaded>,               // 選択中の重い中身。1 件だけ
    search: Search,                       // needle と matched: Vec<SessionDir>（同一性のみ）
    jobs: Jobs,                           // キュー・走行中・通番
    recording: Recording,
    config: Config,
    models: ModelCatalog,
    generations: Generations,             // scan / load / search の世代
}

struct Loaded { dir: SessionDir, generation: u64, transcript: Transcript }
```

### 事実は adapter が集める

いまの `transcribe::write_decision` は純関数のつもりで `transcript::stored_reach(path)` を
呼んでいる（`src/transcribe.rs:1063`）。だからテストは一時ディレクトリに JSON を書いて回す
しかない。新しい形では adapter が whisper を回したついでに書き込み先の既存 JSON も読み、
その事実を `Event` に載せて渡す。

```rust
Event::TranscriptionProduced {
    job, dir, source,
    produced: Transcription,        // whisper の結果
    existing: Option<StoredReach>,  // 書き込み先にすでに在ったもの
}

fn write_decision(produced: &Transcription, existing: Option<StoredReach>) -> WriteDecision
// → テストはファイルを 1 つも作らない
```

### 既存モジュールの行き先

| いま | 本番行 | 行き先 | 備考 |
|---|---|---|---|
| `main.rs` | 3631 | core + UI | 状態解決 6 関数と文言適用が core へ。残りは `apply(view)` と操作の送出 |
| `transcribe.rs` | 1674 | core + adapters | 判断は core、whisper 呼び出しとデコードは adapter |
| `windows/models.rs` | 1279 | UI | 行の状態計算は core。自己完結しているので後回しでよい |
| `model_download.rs` | 1241 | adapters | ほぼそのまま。取得の担当決めだけ job runtime へ |
| `summarize.rs` | 1177 | core + adapters | キュー管理は job runtime へ。プロンプトと llama 呼び出しは adapter |
| `reading_pane.rs` | 864 | core | **ほぼそのまま core になる**（すでに純粋な層として書かれている） |
| `recordings.rs` | 579 | core + adapters | 走査は adapter、`DiskFacts` の組み立てと判断は core |
| `transcript.rs` | 529 | core + adapters | JSON の読み書きは adapter、`ShortfallMarks` と合成は core |
| `recorder` / `system_audio` / `app_audio_monitor` / `mixdown` / `player` | 1704 | adapters | **手を入れない** |
| `dataless` / `atomic_replace` / `private_file` / `inference_slot` | 655 | adapters | そのまま。`inference_slot` は job runtime に吸収 |

## 実装ステップ

### 段階 00 — ワークスペース化して `shoki-core` の器だけ作る

`Cargo.toml` を `[workspace]` にし、空の `shoki-core` を足して `shoki` から依存させる。
**中身はまだ移さない。**

- `shoki-core` の依存は `std` / `serde` / `chrono` のみ（`transcript.rs` は serde、
  `recordings.rs` は chrono を使う。どちらも純粋）
- CI・`cargo dev`（cargo-watch のエイリアス）・MAS パッケージング（#109）への影響を確認する

**完了の判定**: 挙動が一切変わらない。`cargo build` / `cargo fmt --check` /
`cargo clippy --all-targets -- -D warnings` / `cargo test` がすべて通り、`cargo dev` が動く。

### 段階 01 — 文字起こしの状態を `core` へ移す

**着手前に設計攻撃を 1 回入れる**（論点 4）。

- `AppState` / `Event` / `Command` / `Effect` / `update` / `view_*` を新設し、まず
  **文字起こしの状態だけ**を通す
- `reading_pane.rs` を `shoki-core` へ移動する。**Slint 生成型を外す**のがここの主な作業:
  いまの `src/reading_pane.rs:19` は `use crate::{PaneAction, PaneActionKind, SummaryStatus,
  TranscriptStatus}` で Slint の生成型に依存している。core は自前の語彙を定義し、shell が
  Slint 型へ写す（網羅 match なのでコンパイラが同期を守る）
- 状態解決 6 関数のうち文字起こし側 4 つを `view_*` に畳み、**旧経路を同じ PR で削除する**
- `#181`（一覧の走査を UI スレッドから外す）をここで閉じる。走査は
  `Effect::ScanSessions { generation }` → `Event::Scanned` になる
- **副産物**: `examples/transcript_view.rs` の `#[path = "../src/reading_pane.rs"]` と
  `library_text.rs` の共有ハックが不要になる（example が `shoki-core` に普通に依存できる）

**完了の判定**: 議事録・モデル一覧・設定は旧経路のまま動く。文字起こしの状態については
`view_*` が唯一の経路になっている（`grep` で旧 4 関数が残っていない）。
「`row_key` が同じなら `view_row` の出力も同じ」のテストが入っている。

### 段階 02 — ジョブ実行系を統合し、`Event` を唯一の入口にする

- `TranscribeWorker` と `SummarizeWorker` を 1 本のスケジューラへ。状態マップを廃し、
  `JobQueued` / `JobProgressed` / `JobFinished` だけが core へ入る形にする
- `inference_slot` はスケジューラの内側へ
- `#173`（取得・推論スロットの待ちの最中も止められるようにする）をここで閉じる。キュー・
  取り消し・スロットを 1 本が持てば、待ちの中断は 1 箇所の判断になる

**完了の判定**: `queue.status` のような状態マップが無い。#133 / #163 の再発余地（判定と行動が
別のロック取得に分かれる／走っている印と最新エントリがずれる）が構造的に消えている。

### 段階 03 — 議事録の状態を `core` へ（着手は #184 の直前に判断）

残る状態解決 2 関数を `view_*` へ。`summarize.rs` のキュー管理は段階 02 で空になっているので、
プロンプトと llama 呼び出しだけの adapter に痩せる。

### 段階 04 — 保存判断と読み書きを adapter へ押し出す

`Effect::WriteTranscript` を core が返し、shell が書く形へ。#176 で作った `save_transcript` /
`transcribe_each` / `job_outcome` はここで消える（判断と副作用のあいだの詰め替えなので、層が
分かれていれば不要）。

### 段階 05 — `main.rs` を解体する

core へ移し終えた残りを画面単位で `src/ui/` へ。3631 行が、ウィンドウごとの `apply(view)` と
操作の送出に落ちる。`#151` / `#126` はここで解ける。

## 影響範囲・リスク

- **影響を受けるモジュール**: 上の「既存モジュールの行き先」の表を参照。捕捉・再生には
  手を入れない
- **リスク: 段階 01 の PR が大きい**。層の数も差分も大きいので、レビューが重くなる。
  対策は (1) 段階 00 を独立した PR に切って準備だけ先に通す、(2) 着手前の設計攻撃を必須にする
- **リスク: Slint 生成型を core から外す作業が想定外のコスト**。`reading_pane.rs` は
  「crate に依存しない」ものとして書かれているが、実際には Slint の生成型 4 つに依存して
  いる。写像は 4 つの enum ぶんなので大きくはないが、ゼロではない
- **リスク: 3 PR ぶん機能開発が止まる**。まだ配布していないので外部への影響は無いが、
  Ready の 12 件は待つことになる（うち 2 件は段階 01 / 02 で閉じる）

## 未確定事項

- **ワークスペース化が CI・`cargo dev`・MAS パッケージング（#109）に与える影響**。段階 00 で
  実際に確かめる。ここで問題が出たら、`shoki-core` を同一クレート内のモジュール（`src/core/`）
  に留める選択へ戻る余地を残す（その場合、向きを守るのは規約とレビューになる）
- **`clippy.toml` の `disallowed-types` がワークスペースのメンバー単位で効くか**。効けば
  `core` から `std::fs::File` / `Mutex` / `thread` を塞ぐ補助になるが、クレート境界だけでも
  目的は達せられるので必須ではない
- **一覧が数千件になったときの `view_row` の再構築コスト**。`RowKey` の差分で足りるはずだが、
  測ってから判断する。足りなければそのとき版数を入れる
