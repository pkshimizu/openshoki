# マイクが使われ始めたら自動で録音を開始する

- 作成日: 2026-07-10
- ステータス: ドラフト

## 概要

macOS で、Zoom / Teams / Meet などの他アプリが既定入力デバイス（マイク）を使い始めた
ことを検知し、設定で有効化されていれば録音セッションを自動で開始する。会議の開始を
起点に録音を撮り逃さないための機能。文字起こし用途（マイク＋システム音声を別ファイルで
残す）と噛み合う、常駐アプリならではの体験を狙う。

## 背景・前提（コンテキスト）

- 本アプリは**メニューバー常駐型**（`docs/CONTEXT.md`）。録音はトレイメニューの
  「録音を開始／停止」から手動でトグルする（`src/main.rs` の `toggle_recording`）。
- 録音セッションはマイク（`cpal` の既定入力デバイス、必須）＋ システム音声
  （macOS `screencapturekit`、任意）で構成し、`<保存先>/<日時>/` に `mic.mp3` /
  `system.mp3` を書く（`src/recorder.rs` / `src/system_audio.rs`）。
- トレイのメニューイベントは `tray-icon` のグローバルチャネルへ流れ、`main.rs` の
  `slint::Timer`（`MENU_POLL_INTERVAL` = 100ms）が Slint イベントループ上でポーリングして
  処理する。**別スレッドのイベントを 100ms ポーリングでメインループへ橋渡しする**パターンが
  既にある（本機能もこれに倣う）。
- 設定は `Config`（`src/config.rs`）に持ち、OS 標準ディレクトリへ TOML で永続化する。
  現状の項目は `recording_dir` のみ。設定画面は `ui/app-window.slint`。
- `Recorder`／`cpal::Stream`／`SCStream` は `!Send` で、メインスレッド上でのみ保持・開始・
  停止する。自動開始もメインスレッド（タイマーコールバック）上で行えば整合する。

調査で確定した技術前提:

- macOS では CoreAudio の `AudioObjectAddPropertyListener` で、既定入力デバイスの
  `kAudioDevicePropertyDeviceIsRunningSomewhere`（＝いずれかのプロセスがデバイスを
  稼働させているか）の変化を監視できる。これが「他アプリがマイクを使い始めた」の検知に
  ちょうど対応する。プロパティ監視自体は**録音（マイク権限）を必要としない**（デバイス状態の
  参照のため）。
- リスナーは CoreAudio 管理のスレッドで呼ばれるため、共有フラグ（`Arc<AtomicBool>` 等）へ
  「稼働開始」を記録し、既存の 100ms タイマーで拾ってメインループ上で録音を開始する
  （トレイイベントと同じ橋渡し方式）。
- CoreAudio の C API は `coreaudio-sys`（cpal が既に依存ツリーに持つ）で叩ける。
  代替として `objc2-core-audio` もある。

## 要件

- 設定で「マイク使用時に自動録音」を有効化しているとき、既定入力デバイスが**非稼働→稼働**へ
  変化したら、録音中でなければ録音セッションを自動開始する。
- 自動開始は既存の手動開始と同じ経路（`Recorder::start` → セッションディレクトリ作成 →
  マイク＋システム音声）を使い、成果物・保存先・メニュー表示（「録音を停止」ラベル・
  点滅・経過時間）は手動時と同一にする。
- 設定はオプトイン（**既定 OFF**）。設定画面のトグルで切り替え、`Config` に永続化する。
- スコープ外:
  - **自動停止**（マイクが使われなくなったら止める）。停止は従来どおり手動。理由: 自プロセスが
    `cpal` でマイクを掴む間、`IsRunningSomewhere` は自分の稼働で真のままになり、外部アプリの
    解放を同じ信号で検知できないため（別手法が必要で複雑）。
  - Windows / Linux での検知（CoreAudio 依存のため macOS のみ）。
  - 「どのアプリが使い始めたか」の識別・フィルタ（プロセス単位の判定はしない）。

## 確定した論点

ユーザー確認で決定:

- **検知の意味**: 「他アプリがマイクを使い始めたとき」（会議開始の検知）。CoreAudio の
  `kAudioDevicePropertyDeviceIsRunningSomewhere` で実現する。
- **有効化**: 設定でオプトイン、**既定 OFF**（自動録音はプライバシーに関わるため、明示的に
  有効化したときだけ働かせる）。
- **自動停止**: 今回は**自動開始のみ**（停止は手動。上記スコープ外の制約による）。
- **対象 OS**: macOS のみ（システム音声キャプチャと同じく macOS 先行）。

調査で確定した設計上の前提:

- 監視対象は `cpal` の既定入力デバイスと概念的に一致させる（録音が実際に開くデバイスと
  検知対象がズレないよう、CoreAudio の `kAudioHardwarePropertyDefaultInputDevice` で得る
  既定入力デバイスを監視する）。
- 自動開始は「**非稼働→稼働の立ち上がり**」でのみトリガーする。自プロセスの録音による稼働で
  再トリガーしないよう、開始は `recorder.is_none()` のときだけ行う（既存のトグル判定と同じ）。
- 監視は録音中も含め常時行い、実際に開始するかはタイマー側で「設定 ON かつ 未録音」で判定する
  （リスナーの付け外しを設定変更やデバイス変更のたびに行う複雑さを避ける）。

## 実装方針

- **新規 `src/mic_monitor.rs`（`#[cfg(target_os = "macos")]`）**: 既定入力デバイスの
  `IsRunningSomewhere` を CoreAudio のプロパティリスナーで監視する音声使用モニタ。
  - `MicMonitor::start() -> Result<MicMonitor, _>` で、既定入力デバイスを取得し
    プロパティリスナーを登録する。リスナーのコールバックで、状態が稼働に変化したら
    共有フラグ（`Arc<AtomicBool>`）を立てる。
  - メイン側が毎ポーリングで参照・クリアできる `take_activated() -> bool`（立ち上がりを
    1 回だけ返す）を用意する。
  - `Drop` でリスナーを解除する（後始末。常駐が終わるまで保持）。
  - 既定入力デバイスの変更（`kAudioHardwarePropertyDefaultInputDevice`）への追随は、
    初期実装では起動時のデバイスに対して監視する（変更追随は「未確定事項」）。
  - CoreAudio 呼び出しは `coreaudio-sys` を用い、失敗（デバイス無し・登録失敗）は
    エラーを返し、呼び出し側はモニタ無しで常駐を続ける（アプリは落とさない、
    `docs/rules/error-handling.md`）。
- **`src/config.rs`: 設定項目を追加**する。
  - `auto_record_on_mic_active: bool`（既定 `false`）を追加。既存の `config.toml`
    （この項目が無い）を読めるよう **`#[serde(default)]`** を付ける（付けないと旧設定の
    読み込みが失敗し `recording_dir` ごとデフォルトへ落ちる）。TOML ラウンドトリップの
    既存テストに項目を足す。
- **`ui/app-window.slint`: 設定トグルを追加**する。
  - `in-out property <bool> auto-record;` と `callback toggle-auto-record(bool);` を追加し、
    CheckBox（std-widgets）で表示。ウィンドウ高さ（`min-height`/`WINDOW_HEIGHT`）が
    足りなければ `src/main.rs` の定数と両方揃えて調整する（`ui/app-window.slint` の既存
    コメントの制約）。
- **`src/main.rs`: 起動時にモニタを常駐させ、タイマーで自動開始を橋渡し**する。
  - 起動時に `Config` から初期トグル状態を UI に反映（`choose_folder` と同じ要領で
    `on_toggle_auto_record` を実装し、変更を `Config::save()` で永続化）。
  - Slint バックエンド初期化後に `MicMonitor::start()` を呼ぶ（失敗時はログしてモニタ無しで続行）。
  - `build_menu_event_handler` のタイマー処理に、モニタの立ち上がり検知を加える:
    設定が ON かつ `recorder.is_none()` かつ `monitor.take_activated()` が真なら録音を開始する。
    開始処理は現状の `toggle_recording` の**開始側を関数として切り出して共用**する
    （`start_recording(recorder, record_item, config)`）。
  - 自動開始・手動開始のどちらでも、アイコン点滅／経過時間はタイマーが `recorder` の有無を
    見て駆動する既存ロジックのまま動く（変更不要）。
- **`Cargo.toml`**: `[target.'cfg(target_os = "macos")'.dependencies]` に `coreaudio-sys` を追加。

## 実装ステップ

1. **`Config` に `auto_record_on_mic_active`（既定 false, `#[serde(default)]`）を追加**する。
   - 検証: 既存項目のみの TOML を読んでも成功しデフォルト false になる／ラウンドトリップの
     テストが通る（`cargo test`）。
2. **`mic_monitor.rs` を追加**し、既定入力デバイスの `IsRunningSomewhere` を監視して
   立ち上がりを `take_activated()` で 1 回返すモニタを実装する（`coreaudio-sys` 追加）。
   - 検証: `cargo build`／`cargo clippy --all-targets -- -D warnings`。手動確認で、他アプリ
     （例: QuickTime やブラウザの通話）がマイクを使い始めた瞬間にログが出る（暫定ログで確認）。
3. **`toggle_recording` の開始側を `start_recording` として切り出す**（挙動不変のリファクタ）。
   - 検証: 手動の開始／停止が従来どおり動く（`cargo build`／目視）。
4. **`main.rs` にモニタ常駐とタイマー連携を実装**する（設定 ON・未録音・立ち上がりで
   `start_recording`）。
   - 検証: 設定 ON のとき、他アプリのマイク使用開始で録音が自動で始まり、`mic.mp3`
     （権限があれば `system.mp3` も）が保存される。設定 OFF では自動開始しない。録音中は
     再トリガーしない。
5. **設定画面にトグルを追加**し、`Config` への永続化・起動時復元を実装する。
   - 検証: トグルの ON/OFF が保存され、アプリ再起動後も保持される。ウィンドウレイアウトが
     崩れない（`docs/rules/slint.md` の確認手順に準ずる）。
6. **目視の総合確認**: 会議アプリでマイクを開始→自動録音開始→手動停止→保存物を確認。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - 追加: `src/mic_monitor.rs`。
  - 変更: `src/config.rs`（設定項目）、`ui/app-window.slint`（トグル）、`src/main.rs`
    （モニタ常駐・タイマー連携・`start_recording` 切り出し・設定コールバック）、
    `Cargo.toml`（`coreaudio-sys`）、必要なら `WINDOW_HEIGHT`/`min-height`。
  - `src/recorder.rs`／`src/system_audio.rs` は変更なし（`Recorder::start` を再利用）。
- リスクと対策:
  - **自プロセスによる再トリガー／フィードバック**: 録音開始で自分もデバイスを稼働させるが、
    開始は「未録音時の立ち上がり」に限るため再入しない。監視（リスナー登録）自体はデバイスを
    稼働させないので、待機中に自動発火することはない。
  - **既定入力デバイスの変更に未追随**: 起動後にデバイスを差し替えると監視対象がズレる。
    初期実装では未対応とし、`kAudioHardwarePropertyDefaultInputDevice` 監視での再登録を
    後続に回す（「未確定事項」）。
  - **旧設定ファイルの互換性**: 新項目に `#[serde(default)]` を付けないと読み込み失敗で
    `recording_dir` を失う。テストで担保する。
  - **誤検知/意図しない録音**: 短時間のマイク使用でも開始しうる。既定 OFF のオプトインで緩和し、
    録音中はメニューバーの点滅・経過時間で明示する（既存表示で気付ける）。
  - **CoreAudio 連携の実装難度**: `unsafe` な C API を扱う。`system_audio.rs` と同様に薄い
    ラッパへ隔離し、失敗時はモニタ無しで常駐を続ける。

## 未確定事項

- 既定入力デバイスの変更追随（デバイス差し替え時にリスナーを付け替えるか）。初期実装では
  起動時デバイスのみ。
- 有効化時／起動時に**すでにマイクが使用中**だった場合に遡って自動開始するか。初期実装では
  立ち上がりエッジのみを拾い、既に稼働中のケースでは自動開始しない（手動で開始できる）。
- 検知から自動開始までのデバウンス（ごく短い使用で開始しないための猶予）を入れるか。
- CoreAudio バインディングを `coreaudio-sys` と `objc2-core-audio` のどちらにするか
  （実装着手時に、依存の重さと API の扱いやすさで最終決定）。
