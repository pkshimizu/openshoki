# shoki

メニューバー／タスクバーに**常駐**して音声を録音する、Rust 製のデスクトップアプリです。
ウィンドウを主役にせず、常駐したまま素早く録音を開始・停止できることを狙っています。

現在は **macOS を主対象**に開発しています（Windows／Linux は一部機能が後続）。

## 主な機能

- **トレイ常駐**: 起動するとウィンドウを出さずにメニューバー／タスクバーへ常駐し、アイコンの
  メニューから操作します（macOS では Dock・アプリスイッチャーに出ない常駐アプリ）。
- **多重起動しない**: 既に起動している状態で再度起動しても、二重に常駐せず終了します（自動録音の
  二重発火や保存先の競合を防ぐため）。ロックは起動中だけ有効で、終了・クラッシュ後は再び起動できます。
- **ワンクリックで録音の開始／停止**: メニューの「録音を開始」「録音を停止」で切り替えます。
- **録音中インジケーター**: 録音中はメニューバーのアイコン（筆の一画）が赤く点滅し、ツールチップで
  状態が分かります。待機中はモノクロの template 画像として表示されるため、ライト／ダークの
  メニューバーに自動で追従します。
- **マイク音声とシステム音声を別ファイルで保存**: マイク（発話）を `mic.mp3`、スピーカー等の
  システム音声（再生音）を `system.mp3` として、混ぜずに別々の MP3 で保存します
  （将来の文字起こしで発話と再生音を分けて扱うため）。
  - マイク録音は全 OS 共通（`cpal`）。
  - システム音声の録音は現状 **macOS のみ**（ScreenCaptureKit、macOS 13 以降）。
- **録音セッションをディレクトリ単位でまとめる**: 保存先の配下に、録音ごとの `<日時>`
  （例 `20260628-143025`）サブディレクトリを作り、その中に `mic.mp3` / `system.mp3` を置きます。
- **保存先の設定画面**: メニューの「設定を開く」から、録音ファイルの保存先を選べます。設定は
  OS 標準の設定ディレクトリに TOML で永続化されます。
- **会議中の自動録音（オプトイン・macOS 14.4 以降）**: 設定画面で登録したアプリがマイクを使い
  始めたら録音を自動で開始し、使い終わって一定時間（既定 4 秒、設定可）経つと自動で停止します。
  通話の開始・終了に合わせて撮り逃さないための機能で、既定は無効です。合図は音量ではなく
  「アプリがマイクを掴んでいるか」なので、ミュート中や沈黙では止まりません。
  - Chrome（Google Meet 等）・Zen / Firefox・Zoom・Slack のように、アプリが自分の `.app` の中で
    音声を扱う構成に対応します。ブラウザはヘルパープロセスがマイクを掴みますが、親アプリの
    登録で拾います。
  - **Safari（および WebKit ベースのアプリ）は対象外**です。マイクを掴むのがアプリ自身ではなく
    OS 共有のプロセス（`com.apple.WebKit.GPU`）で、そこから元のアプリを特定する手段が公開 API に
    無いためです（private API なら可能ですが、Mac App Store が禁じているため使いません）。
    Safari で会議に出る場合は手動で録音を開始してください。設定画面の登録一覧にも同じ注意書きを
    常時出しています（アプリ名の下に出る個別の注記は Safari / Safari Technology Preview のみです。
    WKWebView を使う他のアプリも同じ制約を受けますが、登録時に見分ける手段が無いためです）。
- **極小音量の自動リカバリ**: 会議アプリ（ブラウザの Google Meet 等）の自動ゲイン調整で
  マイク録音が極端に小さくなった場合（再生すると無音に聞こえる）、録音停止後に自動で音量を
  正規化して保存し直します。正常な音量の録音には手を加えません。
- **録音停止時の自動文字起こし（オプトイン）**: ローカルの whisper.cpp で各音源をオンデバイス
  文字起こしし、セグメントの開始/終了時刻付き JSON（`mic.json` / `system.json`）をセッション
  ディレクトリへ保存します（音声を外部送信しません）。設定画面のトグルで有効化するだけで使え、
  認識言語も設定画面で選べます（英語が既定。日本語・中国語など主要 8 言語と自動判定）。
  whisper モデルは設定画面で 6 種（Tiny 74MB〜Large v3 2.9GB、既定は Small 465MB）から選べ、
  未取得のモデルは選択時に Hugging Face から自動ダウンロードしてデータディレクトリへ保存・
  再利用します（SHA-256 検証つき。進捗は設定画面に表示）。通信はこのダウンロード（受信）のみで、
  音声や文字起こし結果を送信することはありません。
- **議事録の自動生成（オプトイン）**: 文字起こしが終わると、ローカルの llama.cpp で議事録
  Markdown（議事概要・議題内容・決定事項・アクションアイテム）を生成し、セッションディレクトリへ
  `summary.md` として保存します（文字起こしを外部送信しません）。設定画面のトグルで有効化します
  （文字起こしの結果を使うため、自動文字起こしが ON のときだけ動きます）。要約の言語は認識言語の
  設定に追従します。使う LLM は設定画面で 2 種（Qwen2.5 Instruct Q4_K_M の 3B 2.0GB /
  7B 4.4GB、既定は 7B）から選べ、未取得のモデルは Hugging Face から自動ダウンロードします
  （whisper と同じ SHA-256 検証つき）。ダウンロードが始まるのは、この機能を ON にしている
  状態でモデルを選び直した時点か、初回の生成時（Recordings ウィンドウからの手動生成を含む）
  です（OFF のまま設定画面で選んでも通信は起きません）。
  選択肢には所要時間とメモリの目安を添えています。**CPU で動くため、既定の 7B は 4 分の会議で
  1 分弱・実行時 8GB 前後のメモリを使います**。長い会議はチャンクに分けた 2 段要約になり、
  そのぶん時間がかかります。
  軽さを優先したい場合は 3B を選ぶと速度とメモリが半分ほどになりますが、細部を取り違える
  ことがあります。生成した議事録は Recordings ウィンドウの Summary タブで読めます（下記）。
- **録音の一覧と再生**: メニューの「Recordings…」から、録音済みセッションを新しい順に一覧し、
  選んで再生（Play / Pause / Stop、経過/全体時間の表示）できます。マイクとシステム音声の両方が
  あるセッションは、ミックスして同時に再生します。再生バーはクリックした位置へ再生位置が移動し、
  つまみをドラッグしている間は音は動かず、つまみ・バーの塗り・時刻表示だけが追従して、離した位置へ
  再生位置が移動します（再生できないセッション・全体長が不明な場合は表示専用になります）。
- **文字起こしの表示と発話へのスキップ**: 文字起こし済みのセッションを選ぶと、mic / system を
  マージした文字起こしが時刻順に表示されます。セグメントをクリックするとその発話の開始秒へ
  再生位置がスキップし、再生中は現在位置のセグメントがハイライトされます（文字起こしが無い
  セッションは状態に応じて「Transcribing…」「Transcription Failed」「Not Transcribed Yet」を
  表示）。
- **文字起こしの状態表示と再実行**: 一覧の各行に文字起こしの状態（前 / 実行中 / 完了 / 失敗）が
  ドットで表示され、選択中セッションの状態は詳細ペインにテキストで表示されます（失敗は
  赤字で表示されます）。詳細ペインの
  「Transcribe」ボタンで文字起こしを手動実行でき、完了済みのセッションもやり直せます
  （言語やモデルを変えた後の再実行に使えます）。議事録の生成中・順番待ちの間は押せません。
  失敗の記録はアプリ再起動で消えます。
- **議事録の表示と手動生成**: 右ペインの「Transcript」「Summary」タブで表示を切り替えられ、
  Summary では生成済みの議事録（`summary.md`）を見出しを強調して表示します。詳細ペインの
  「Summarize」ボタンで手動生成でき、生成済みのセッションもやり直せます（モデルを変えた後の
  再生成に使えます。文字起こしが無いセッション・文字起こし中・生成中・順番待ちの間は
  押せません）。生成中・失敗は状態として表示され、完了すると表示が自動で最新の内容に
  切り替わります。手動生成は設定の
  トグルが OFF でも動くので、モデルが未取得ならこのときダウンロードが始まります（数 GB。
  生成の完了まで数分かかります）。複数のセッションに続けて指示すると**順番待ち**
  （`Waiting to summarize…`）になり、待っている間は状態表示の隣に出る「Cancel」で取り消せます
  （生成が始まってからは取り消せません）。
- **モデルの管理（一覧・取得・削除・選択）**: 設定画面の「Manage Models…」から、文字起こし用
  （whisper）と議事録要約用（LLM）のモデルを**種別ごとに一覧**できます。カタログ全件が並ぶので、
  まだ取得していないモデルも見えます。行ごとに表示名・説明・サイズ・状態（未取得 / 取得中の進捗 /
  取得済み / 失敗）が出て、末尾に取得済みの合計使用量が出ます。
  - **使うモデルを選べます**（「Use」）。設定画面の ComboBox と同じ経路なので、どちらから選んでも
    同じ結果になります。文字起こし用のモデルは、選んだ時点で未取得なら**取得も始まります**
    （議事録要約の LLM は、議事録の生成が ON のときだけ取得を始めます）。
  - **先に取得できます**（「Download」）。通信は Hugging Face からの**受信のみ**（SHA-256 検証つき）
    で、音声や文字起こし結果を送信することはありません。取得中の進捗と、失敗した理由
    （空き容量が足りない等）はウィンドウを開いている間だけ更新されます。
  - **削除できます**（「Delete」）。確認ダイアログを経て**完全削除**します（録音の削除と違い
    ゴミ箱には入れません。カタログに URL と SHA-256 があり再取得できるので、ゴミ箱に数 GB
    残さないほうが目的に合います）。選択中のモデルも削除でき、次に必要になったときに自動で
    再ダウンロードされます。**文字起こし・議事録生成のジョブが積まれている間はそのモデルを
    削除できません**（押した時点でも確かめるので、一覧を開いたあとに始まったジョブでも消えません）。
    削除できなかったときは一覧の下に理由が出ます。
  - `models/` にあるカタログ外のファイル（モデル入れ替え後の旧ファイルなど）も末尾に並ぶので
    掃除できます（これらはアプリでは再取得できないので、確認ダイアログでその旨を伝えます）。
    `config.toml` でモデルパスを上書きしている場合、その種別の行には「not used because config.toml
    sets the model file」と表示し、「Use」「Download」を出しません（上書き中はカタログのモデルが
    使われないため）。上書き先が同じフォルダにあれば、その行には「set in config.toml」と表示し、
    **消しても再取得されない**ことを確認ダイアログで伝えます（その行は取得できます — 上書き先を
    落とすのが動かす唯一の手段のため）。
- **書きかけの一時ファイルの片付け**: 強制終了などで残った書きかけのファイル
  （`*.part.<番号>`）を、Recordings ウィンドウを開いたときに片付けます。対象はアプリが書く
  ファイルの書きかけのうち、**最後の更新から 1 時間以上経ったもの**だけです（書き込み中の
  ファイルと取り違えないため。同じフォルダに置いた他のファイルには触れません）。
- **録音の削除（ゴミ箱へ移動）**: 詳細ペインの「Delete」ボタンで、選択中の録音セッションを
  削除できます。確認ダイアログで承認すると、セッションのフォルダごと OS のゴミ箱へ移動します
  （完全削除はしないため、ゴミ箱から復元できます）。文字起こし中・議事録の生成中のセッションは
  削除できません（ファイルを読み書きしている最中のため）。議事録の順番待ちのセッションは削除でき、
  そのとき生成の指示は取り消されます。

## 動作要件

- **OS**:
  - macOS 13（Ventura）以降を主対象（システム音声録音に必要）。
  - Windows／Linux はマイク録音のみ動作対象で、システム音声録音は後続対応です。
- **権限（macOS）**: マイク録音にマイクの許可、システム音声録音に画面収録の許可が必要です。
  画面収録の許可が無い場合もアプリは落ちず、マイク録音は継続します。

## ビルドと実行

ソースからビルドして実行します（配布用の `.app` バイナリは今後提供予定）。

### 前提

- **Rust ツールチェーン**（edition 2024 を使うため Rust 1.85 以降）。
- **C コンパイラ**: `mp3lame-encoder` が libmp3lame をビルドするために必要です。
- **CMake**: `whisper-rs` が whisper.cpp を、`llama-cpp-2` が llama.cpp をビルドするために
  必要です（`brew install cmake`）。どちらもソースからビルドするため、初回ビルドは数分かかります。
- **macOS**: 安定版の Xcode コマンドラインツール。ScreenCaptureKit の Swift ブリッジの
  ビルド・リンクに使います（ベータ版 Xcode では Swift 後方互換ライブラリを解決できず
  リンクに失敗することがあります）。

### 実行

```sh
cargo run
```

起動するとウィンドウは開かず、メニューバー／タスクバーのアイコンに常駐します。アイコンの
メニューから録音や設定を操作してください。

### リリースビルド

```sh
cargo build --release
```

## プロジェクト構成

```
shoki/
├── Cargo.toml            クレート定義・依存
├── build.rs              Slint UI のコンパイルと macOS 向けリンク設定
├── ui/
│   ├── app-window.slint       設定画面の UI 定義（Slint。他ウィンドウの再エクスポートも行う）
│   ├── recordings-window.slint 録音一覧・再生ウィンドウの UI 定義（Slint）
│   ├── models-window.slint    モデル管理（一覧・取得・削除・選択）ウィンドウの UI 定義（Slint）
│   ├── controls.slint         複数ウィンドウで使う自作コントロール（DangerButton）
│   └── style.slint            タイポグラフィ・淡色のトークン（全ウィンドウ共有）
├── assets/
│   ├── icon/             アプリアイコンの資産（マスターと生成物。scripts/generate-icons.sh 参照）
│   │   ├── mark.svg          筆の一画のマスター（形の正はこの 1 本だけ）
│   │   ├── shoki.icon/   Icon Composer 形式のマスター（icon.json と Assets/seal.svg。
│   │   │                     Assets/mark-ink*.svg は mark.svg から生成する色違い）
│   │   ├── tray.png          メニューバー常駐アイコン（36x36 RGBA。ビルド時に埋め込む。生成物）
│   │   └── generated/        actool の生成物（shoki.icns。Assets.car は追跡しない）
│   ├── menu/             トレイメニュー項目のアイコン（PNG, 32x32 RGBA。ビルド時に埋め込む）
│   └── samples/          検証用の架空トランスクリプト（summary_probe と要約の通しスモーク
│                         テストが埋め込む。出荷バイナリには入らない）
├── scripts/
│   ├── generate-icons.sh アイコン資産の再生成（.icon → Assets.car / .icns、SVG → tray.png）
│   └── check-icons.sh    生成物がマスターと一致するかの検査（CI でも実行）
└── src/
    ├── main.rs           エントリ。トレイ初期化と Slint イベントループ起動
    ├── tray.rs           トレイアイコン／メニューの構築とイベントのディスパッチ
    ├── recorder.rs       録音セッション（マイク＋システム音声）の開始・停止と MP3 書き出し
    ├── player.rs         録音の再生（rodio でファイルをストリーミング再生）
    ├── mixdown.rs        録音停止後の mic＋system ミックス音声（mix.mp3）生成（バックグラウンド）
    ├── recordings.rs     録音セッションの探索（新しい順に一覧）と一時ファイルの回収
    ├── system_audio.rs   macOS のシステム音声キャプチャ（ScreenCaptureKit）
    ├── transcribe.rs     録音停止後の自動文字起こし（whisper.cpp、バックグラウンド）
    ├── transcript.rs     文字起こし JSON の読み込みと mic／system の時刻順マージ（表示用）
    ├── summarize.rs      文字起こしから議事録（summary.md）を生成（バックグラウンド）
    │   └── on_device.rs  議事録生成のオンデバイス実装（llama.cpp）
    ├── inference_slot.rs 重い ML 推論（whisper／要約 LLM）を同時に 1 本しか走らせない共有スロット
    ├── model_download.rs 検証つきモデルダウンロードの共有基盤（SHA-256・原子的配置・状態管理）
    ├── atomic_replace.rs 一時ファイル→リネームで原子的に置き換える定型（後始末・取り残しの掃除）
    ├── whisper_model.rs  内蔵 whisper モデルのカタログ（選べるモデルの一覧）
    ├── summary_model.rs  議事録要約 LLM のカタログ（選べるモデルの一覧）
    ├── single_instance.rs 多重起動を防ぐ排他ロック（起動時に取得）
    └── config.rs         設定（保存先など）の読み込み・保存（TOML）
```

主な依存: GUI に [Slint](https://slint.dev/)、トレイ常駐に `tray-icon`、マイク取得に `cpal`、
MP3 エンコードに `mp3lame-encoder`、再生に `rodio`、設定の永続化に `directories` / `serde` / `toml`、
多重起動防止に `fs2`、
文字起こしに `whisper-rs`（whisper.cpp）/ `symphonia`（MP3 デコード）/ `rubato`（リサンプル）、
議事録要約に `llama-cpp-2`（llama.cpp）。
macOS では `screencapturekit` と `objc2` 系を使います。

## 現状と今後

- **現状**: macOS 先行。マイク録音は全 OS、システム音声録音は macOS のみ。
- **今後**:
  - Windows（WASAPI loopback）／Linux（monitor source）のシステム音声録音（[#23](https://github.com/pkshimizu/openshoki/issues/23) / [#24](https://github.com/pkshimizu/openshoki/issues/24)）
  - 配布用 macOS `.app` バンドルの生成（[#20](https://github.com/pkshimizu/openshoki/issues/20)）

## 開発

- **バージョニングとリリース手順**: バージョンは **SemVer**で、**`Cargo.toml` の `version` が
  唯一の正**とする方針です（二重管理や bot コミットによる自動注入はしません）。
  現時点でここから導出しているのは**設定画面の表示だけ**で、タグとの整合チェックと
  `Info.plist` への注入はリリースワークフロー（[#20](https://github.com/pkshimizu/openshoki/issues/20)、
  未実装）で入ります。

  リリースは次の順で行います。

  1. `Cargo.toml` の `version` を上げるコミットを作る（例: `chore: v0.2.0 へバンプする`）
  2. `main` へマージしたあと `git tag v0.2.0 && git push origin v0.2.0`
  3. リリースワークフロー（#20）が `.app` バンドルを組む。タグと `Cargo.toml` の不一致は
     ここで fail させる

  配布は **Mac App Store に寄せる**方針（[#109](https://github.com/pkshimizu/openshoki/issues/109)）で、
  GitHub Releases には添付しません。1.0.0 への引き上げは、署名・公証や機能の安定を目安に
  後続で判断します。

- **ホットリロード（自動再ビルド・再起動）**: `cargo dev` でソース（`src` / `ui` /
  `build.rs` / `Cargo.toml`）の変更を監視し、保存するたびに自動で再ビルドして起動し直します。
  事前に `cargo install cargo-watch` が必要です。

  ```sh
  cargo install cargo-watch   # 初回のみ
  cargo dev
  ```

- **アイコン資産の再生成**: 筆の一画の形は `assets/icon/mark.svg` 1 本が正で、アプリアイコンの
  色違いレイヤーもメニューバーのグリフもここから生成します。マスター（`mark.svg` /
  `shoki.icon/icon.json` / `shoki.icon/Assets/seal.svg`）を変えたら次を実行し、
  生成物ごとコミットしてください（`Assets.car` は例外。後述）。

  ```sh
  ./scripts/generate-icons.sh
  ```

  Xcode 26 以降（`xcrun actool`。Icon Composer 形式の `.icon` を扱えるバージョン）、
  `rsvg-convert`、ImageMagick（`magick`）が必要です。

  `actool` が出す `Assets.car`（macOS 26 のレイヤードアイコン）だけは**コミットしません**。
  入力が同じでも毎回バイト列が変わり、意味のない差分が毎回出るためです（`.gitignore` 済み）。
  使うのは `.app` のパッケージングのときだけなので、その場でこのスクリプトを実行して生成します。

  生成物がマスターと一致しているかの確認だけなら次を実行します（作業ツリーは変更しません）。
  Xcode 26 以降があれば `shoki.icns` まで、無ければ `mark.svg` 由来の生成物だけを検査します
  （`Assets.car` は毎回変わるので対象外）。アイコン資産を変更した PR では CI でも同じ検査が走ります。

  ```sh
  ./scripts/check-icons.sh
  ```

- コミット前の検証コマンド:

  ```sh
  cargo fmt --check
  cargo clippy --all-targets -- -D warnings
  cargo build
  cargo test
  ```

- CI（GitHub Actions）で上記の build／fmt／clippy／test と `cargo audit`（依存の脆弱性検査）を
  実行しています。

- **設定画面の描画確認**: Trigger apps の一覧は固定高さで clip されるため、折り返す注記を入れると
  潰れます。ビルドやテストでは検出できないので、確認用バイナリで目視します。

  ```sh
  cargo run --example settings_view                     # 表示して screencapture で確認
  cargo run --example settings_view -- snapshot out.png # PNG に書き出す（画面収録の許可が不要）
  ```

- **議事録要約の LLM 検証プローブ**: オンデバイス LLM で議事録を生成する方針
  （[#78](https://github.com/pkshimizu/openshoki/issues/78)）の検証に使う
  `examples/summary_probe.rs`。example なので出荷バイナリには含まれません
  （`llama-cpp-2` 自体は本体の依存なので、`cargo build` でも llama.cpp はビルド・リンク
  されます）。プロンプトの正は本実装（`src/summarize.rs`）側で、このプローブが持つ文面は
  #78 当時のスナップショットです。

  ```sh
  cargo run --release --example summary_probe -- --model <path.gguf> --lang ja
  cargo run --release --example summary_probe -- --model <path.gguf> --lang en
  ```

  サンプルのトランスクリプト（`assets/samples/meeting-{ja,en}.txt`）は架空の会議で、
  実データを使わずに再現できるようにしてあります。計測値と採用モデルは
  ローカルのプラン `docs/plans/` にあります（`docs/` は追跡対象外）。

- **Mac App Store 可否の検証プローブ**: MAS 対応（[#77](https://github.com/pkshimizu/openshoki/issues/77)）の
  技術検証に使う `examples/mas_probe.rs` を、App Sandbox の有無を切り替えて実行します。
  出荷バイナリには含まれません（`cargo` の example）。

  ```sh
  ./scripts/mas-probe.sh --sandbox    -- --verbose --skip-screen   # サンドボックス有り
  ./scripts/mas-probe.sh --no-sandbox -- --verbose --skip-screen   # 比較用
  ./scripts/mas-probe.sh --sandbox --open                          # TCC を伴う検証
  ```

  `.app` に包んで ad-hoc 署名する理由と、`--open`（LaunchServices 経由の起動）が要る理由は
  `scripts/mas-probe.sh` の冒頭コメントにあります。検証の結論は次のとおりで、後続作業は
  [#107](https://github.com/pkshimizu/openshoki/issues/107) /
  [#108](https://github.com/pkshimizu/openshoki/issues/108) /
  [#109](https://github.com/pkshimizu/openshoki/issues/109) に切り出してあります
  （測定値の全文はローカルのプラン `docs/plans/`。`docs/` は追跡対象外です）。

  - App Sandbox 下でも、CoreAudio のプロセス照会・ScreenCaptureKit・security-scoped bookmark・
    マイク取得はすべて動く。
  - 自動録音が使っていた private API（responsible pid）は、Chrome / Zen / Slack / Zoom について
    公開 API で置き換えられる（[#107](https://github.com/pkshimizu/openshoki/issues/107) で実施済み）。
  - **Safari（WebKit ベース）だけは置き換えられない**。マイクを掴むのが WebKit の GPU プロセスで、
    そこから Safari へ辿れるのは private API だけのため。Safari は自動録音の対象外とした。

  `--open` を付けた実行では、プロセス一覧を含むレポートが
  `~/Library/Containers/net.noncore.shoki.masprobe/Data/` に一時的に作られます
  （表示後にスクリプトが削除します）。
