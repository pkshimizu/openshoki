# アプリを Mac App Store に登録する

- 作成日: 2026-07-22
- ステータス: フェーズ 1（技術検証）完了・**GO**（#77）。**配布は MAS 一本**に決定
  （2026-07-26。当初の「GitHub Releases と並行提供」から変更。理由は下記「配布方針の変更」）。Safari は自動録音の対象外とする判断つき
  （結果と判断の根拠は末尾「フェーズ 1 の検証結果」参照）。フェーズ 2 以降は未着手

## 概要

openshoki を Mac App Store（MAS）で有料アプリとして配布する。ただし**機能を落とさない**ことを
絶対条件とする: 自動録音（ブラウザの Google Meet 等を含む）が MAS の制約（App Sandbox・
private API 禁止）の下で成立するかをまず技術検証し、**解決できなければ MAS 登録は見送る**
（GitHub Releases 配布のみ継続）。検証が通った場合のみ、サンドボックス対応→パッケージング→
申請へ進む。

## 背景・前提（コンテキスト）

- **Apple Developer Program は加入済み**（証明書・App Store Connect が使える前提）。
- 配布の既存方針: GitHub Releases に `.app` zip（#20、ad-hoc 署名、未実装）。MAS とは
  **並行提供**する。バージョンは `Cargo.toml` が正（#74）。LP は #75/#76。
- MAS の必須制約:
  - **App Sandbox 必須**。現状のコードはサンドボックス前提ではない。
  - **private API 禁止**。`src/app_audio_monitor.rs` が
    `responsibility_get_pid_responsible_for_pid`（非公開シンボル、dlsym 解決）を使用中。
    Chrome 等のブラウザヘルパープロセス（バンドル ID が nil）を親アプリに解決し、
    「ブラウザで Meet 中」を検知する要。**App Review のリジェクト対象**であり、代替が必要。
- サンドボックスへの影響が既知の箇所:
  - **保存先フォルダ**: `rfd`（NSOpenPanel）で選択したパスを TOML に文字列保存
    （`config.rs`）。サンドボックスでは選択時のアクセス権が再起動で失効するため、
    **security-scoped bookmark** の保存・復元が必要。
  - **モデルダウンロード**（`whisper_model.rs`、HTTPS 受信）: `network.client`
    entitlement で可。
  - **マイク**（cpal）: `device.audio-input` entitlement ＋ TCC で可。
  - **ScreenCaptureKit**（システム音声）と **CoreAudio プロセス照会**
    （`kAudioProcessPropertyIsRunningInput`、macOS 14.4+ 公開 API）: サンドボックス内での
    動作は**未検証**（フェーズ 1 で確認する）。
- 審査観点: 録音アプリのためプライバシー説明（マイク・画面収録の usage description、
  App Privacy の「データ収集なし」申告）が重要。オンデバイス処理・外部送信なしは強み。

## 要件

- MAS で有料アプリとして公開する（価格は未確定事項）。
- **機能を落とさない**: 自動録音（ブラウザヘルパーの親アプリ解決を含む）・システム音声録音・
  文字起こしが MAS 版でも GitHub 版と同等に動く。
- GitHub Releases 配布（#20）と並行提供する。可能な限りビルドを分岐させない
  （private API の公開 API 置換が成功したら**両配布共通**でそれを使う）。
- スコープ外:
  - iOS / iPadOS 対応。
  - アプリ内課金・サブスクリプション（買い切り有料のみ）。
  - GitHub 配布側の正式署名・公証（Developer ID）への切替（別プランで扱う。#20 は当面
    ad-hoc のまま）。
  - MAS 提出の CI 自動化（初回は手動。自動化は運用が回り始めてから後続で）。

## 確定した論点

ユーザー確認で決めた事項:

- **Apple Developer Program は加入済み**（加入手続きはプラン外）。
- **自動録音の劣化は受け入れない**: ブラウザ（Meet 等）の自動録音ができないのは
  アプリのメリットを大きく減らすため、公開 API で解決できなければ **MAS 登録を見送る**。
  この判断をフェーズ 1 の go/no-go ゲートとして最初に置く。
- **並行提供**（MAS ＋ GitHub Releases）。機能差は作らない。
- **有料**で公開する（金額は未確定。App Store Connect の Paid Apps 契約・銀行口座・
  税務情報の登録が追加で必要）。

調査で解消した事項:

- private API の使用箇所は `app_audio_monitor.rs` の responsible-pid 解決**のみ**
  （`grep dlsym/responsibility` で確認）。他はすべて公開 API。
- 公開 API での代替候補（フェーズ 1 で検証した。**結果は末尾の節が正**。以下は着手前の見立て）:
  1. **`proc_pidpath`（libproc、公開ヘッダ）でヘルパーの実行パスを取得し、パスから外側の
     `.app` バンドルを特定してバンドル ID を得る**。Chrome ヘルパーは
     `Google Chrome.app/Contents/Frameworks/...` 配下にあるため、パスの先頭側の `.app` を
     切り出せば親アプリに解決できる。プロセスツリーに依存せず素直。
  2. `sysctl(KERN_PROC_PID)` で ppid を辿り、親プロセスのバンドル ID を
     `NSRunningApplication` で引く（ヘルパーの親がブラウザ本体である前提。zygote 構成で
     の多段親子も辿る）。
  - いずれもサンドボックス内で他プロセスの情報がどこまで読めるかが鍵（フェーズ 1 で実測）。

## 実装方針

**フェーズ 1（技術検証・go/no-go）→ フェーズ 2（サンドボックス対応）→ フェーズ 3
（パッケージング・申請）**の 3 段階。フェーズ 1 が NO ならフェーズ 2 以降は行わず、
検証結果をこのプランに追記して見送りを記録する。

- フェーズ 1 は「検証用の最小ビルド」で行う（本実装に手を入れる前に結論を出す）:
  - 公開 API 代替（`proc_pidpath` 方式）で Chrome/Meet のヘルパー→親アプリ解決が動くか。
  - App Sandbox を有効化した検証ビルドで、(a) CoreAudio のプロセス列挙＋
    `kAudioProcessPropertyIsRunningInput` 照会、(b) ScreenCaptureKit のシステム音声取得、
    (c) 上記代替 API、がそれぞれ動くか。
- フェーズ 2 で置換が成功したら、**private API は両配布から完全に削除**する（ビルド分岐を
  作らない。`docs/rules/ffi.md` の dlsym 節も実態に合わせ更新）。
- 保存先は security-scoped bookmark を config に保存する方式へ移行する（非サンドボックス
  ビルドでも bookmark API はそのまま動くため、ここも分岐不要。旧 config のパス文字列からの
  移行パスを用意する）。
- MAS パッケージングは #20 の `packaging/`（Info.plist・アイコン）を土台に、MAS 用
  entitlements（app-sandbox / audio-input / network.client / user-selected read-write）と
  Apple Distribution 署名＋provisioning profile、`productbuild` での pkg 化、Transporter
  アップロードを加える（初回は手動手順として文書化）。

## 実装ステップ

1. **［フェーズ 1］公開 API 代替の検証**（go/no-go その 1）
   `proc_pidpath` によるヘルパー→親 `.app` 解決の検証コードを書き、Chrome（Meet）・
   Zoom.app・Slack ハドルで responsible-pid 方式と同じ解決結果になるか比較する。
   確認: 主要ケースで一致。不一致のケースと影響を記録する。
2. **［フェーズ 1］サンドボックス動作検証**（go/no-go その 2）
   App Sandbox ＋必要 entitlements を付けた検証ビルド（ad-hoc 署名で可）で、CoreAudio
   プロセス照会・ScreenCaptureKit・代替 API・NSOpenPanel＋bookmark が動くか実測する。
   確認: すべて動作。**どちらかが NO なら見送りを決定**し、結果をプランに追記して終了。
3. **［フェーズ 2］private API を公開 API 実装へ置換**
   `app_audio_monitor.rs` の responsible-pid 解決を `proc_pidpath` 方式に置き換え、
   dlsym 経路を削除する（両配布共通）。既存の自動録音の受け入れ条件（Chrome/Meet で
   開始・停止）で回帰確認し、`docs/rules/ffi.md` を更新する。
4. **［フェーズ 2］保存先の security-scoped bookmark 化**
   `config.rs` に bookmark の保存・復元（`startAccessingSecurityScopedResource`）を実装し、
   旧 config（パス文字列）からの移行を入れる。サンドボックス有無の両ビルドで、再起動後も
   保存先へ書けることを確認する。
5. **［フェーズ 3］MAS パッケージングと手動提出手順の整備**
   MAS 用 entitlements・Apple Distribution 署名・provisioning・`productbuild`・Transporter
   の手順を `packaging/`（と手順書）に整備し、App Store Connect にアプリを登録
   （bundle ID 確定・有料価格・Paid Apps 契約・App Privacy「収集なし」申告）。
   TestFlight（Mac）で配布ビルドの全機能（録音・自動録音・文字起こし・再生）を確認する。
6. **［フェーズ 3］審査提出**
   スクリーンショット・説明文（LP #75 と整合）・レビュー用メモ（マイク/画面収録の用途、
   オンデバイス処理）を添えて提出する。リジェクト時は指摘をプランに追記して対応する。
   確認: 審査通過・ストア公開。README / LP に App Store リンクを追記する。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - `src/app_audio_monitor.rs`（private API → 公開 API 置換）
  - `src/config.rs`・`src/main.rs`（保存先の bookmark 化と移行）
  - `packaging/`（#20 の成果物に MAS 用 entitlements・署名・pkg 手順を追加）
  - `docs/rules/ffi.md`・README・LP（記載の同期）
- リスクと対策:
  - **公開 API 代替が不成立**（解決率が実用にならない／サンドボックスで読めない）:
    フェーズ 1 で早期に判定し、見送りを記録して撤退する（本実装に手を付けない構成に
    している）。
  - **サンドボックスでの CoreAudio プロセス照会が不可**: 同上（go/no-go その 2 で判定）。
  - **審査リジェクト**（録音アプリのプライバシー懸念・4.2 最小機能等）: オンデバイス
    処理・外部送信なし・usage description を明記し、レビュー用メモで用途を説明する。
    リジェクト理由はプランに追記して個別対応する。
  - **bookmark 移行の失敗で保存先が失われる**: 移行失敗時は既定の保存先へフォールバック
    し、設定画面で再選択を促す（`docs/rules/error-handling.md` の縮退方針に従う）。
  - **有料化に伴う契約系の待ち時間**（Paid Apps 契約・銀行・税務の承認に日数がかかる）:
    フェーズ 3 の早い段階で並行して着手する。

## 未確定事項

- 価格（金額・地域別価格）。フェーズ 3 の App Store Connect 登録までに決める。
- `CFBundleIdentifier` の確定値（例: `net.noncore.openshoki`。#20 と共通。TCC が identity に
  紐づくため一度決めたら変えない）。
- GitHub 配布側を Developer ID 署名＋公証へ引き上げる時期（別プラン。加入済みのため
  技術的には可能になった）。
- MAS 提出の CI 自動化（初回リリース後に運用を見て判断）。

## フェーズ 1 の検証結果（#77）

- 実施日: 2026-07-26
- 環境: macOS 26.5.2（25F84）/ Apple Silicon
- 道具: `examples/mas_probe.rs`（検証専用プローブ）と `scripts/mas-probe.sh`
  （ad-hoc 署名した `.app` に包み、App Sandbox の有無を切り替えて同じバイナリを走らせる）。
  本実装（`src/`）には手を入れていない。

```sh
./scripts/mas-probe.sh --sandbox    -- --verbose --skip-screen   # サンドボックス有り
./scripts/mas-probe.sh --no-sandbox -- --verbose --skip-screen   # 比較用
./scripts/mas-probe.sh --sandbox --open                          # TCC を伴う検証は open 経由
```

`--open` を付けると LaunchServices 経由で起動する。TCC（画面収録・マイク）とフォルダ選択
パネルは responsible process で「どのアプリの要求か」を見るため、シェルから実行ファイルを
直接叩くと**ターミナル側**の権限として扱われ、`.app` の許可を試せない。

### go/no-go その 1: private API の公開 API 代替（結果: **1 クラスだけ劣る**）

母集団は CoreAudio のプロセスオブジェクト一覧（＝自動録音が実際に走査する集合）。件数は
動いているアプリで変わるので、以下の数値は 1 回の観測のもの。

**比較は集合で行う**。本体の `input_running_bundle_ids` は「直接のバンドル ID」と「親から解決した
バンドル ID」の**両方**を集合へ入れて照合するので、方式ごとに 1 値へ畳んで比べると、直接の ID が
取れる行で両辺が自明に一致してしまい、本体が依存している親解決の経路を検証できない
（初回の計測はこれを誤り、「29/31 一致」と実態より良い数字を出していた）。

**本題は「親解決が ID を足した行」**（9 行）。直接のバンドル ID が取れる行は private 方式でも
公開 API でも同じ値になるので、そこを含めた比率は判定に使えない。以下は 1 回の観測
（33 プロセス／うち親解決が寄与 9 行）の値で、母集団は動いているアプリで変わる。

| 方式 | 全行 equal/compared（missing） | 親解決が寄与した行 equal/compared（missing） |
|---|---|---|
| `proc_pidpath` → 外側の `.app` | 15/20（4） | **5/9（4）** |
| `proc_pidinfo` で親 PID を辿る | 14/19（5） | 4/9（5） |
| `kAudioProcessPropertyBundleID` | 10/31（9） | 0/9（9） |
| 置き換え想定（direct + path + coreaudio） | 13/31（4） | **3/9（4）** |

- `compared` は「private 側も方式側も空」の行を除いた数（除かないと、`.app` の外にいるデーモンが
  `None == None` で一致に積まれて分母が水増しされる）。方式ごとに値が違うのはこのため。
- **`missing`（private が持っていた ID を公開 API が取りこぼした行）が代替可否の要**。
- 案 2（親 PID 辿り）は置き換え想定に含めない。案 1 に対する追加の解決力が無く（XPC サービスは
  `launchd` の子なので辿れない）、この観測でも案 2 が救う行は 0 件だった。
- `extra`（公開 API 側だけが持つ ID）はほぼ `kAudioProcessPropertyBundleID` がシステムデーモン
  （`audiomxd` / `corespeechd` / `avconferenced` 等）やヘルパー自身の ID（`….helper`）を返すぶんで、
  登録アプリとの照合には無害。

置き換え想定の `missing` 4 行の内訳:

| pid | private | 置き換え想定 | 実行ファイル |
|---|---|---|---|
| 75093 | `com.apple.WebKit.GPU` + **`com.apple.Safari`** | `com.apple.WebKit.GPU` | `WebKit.framework/…/com.apple.WebKit.GPU.xpc/…` |
| 1816 | `com.apple.WebKit.GPU` + **`com.raycast.macos`** | `com.apple.WebKit.GPU` | 同上 |
| （2 件） | `dev.zed.Zed` | なし / 自分自身 | `target/debug/openshoki`、プローブ自身 |

後ろの 2 件は「`.app` に入っていない開発中バイナリが、起動元のターミナル/エディタに帰属する」もので、
doc に書いてある副作用そのもの。公開 API 方式はこれを拾わない＝**帰属範囲が狭まる望ましい方向**の差
（`.app` として起動すれば自分自身に解決する。`--open` で実測）。**実質の取りこぼしは WebKit 型の
2 件だけ**。

アプリ単位で見ると差がはっきりする（`rows / 親解決が寄与した行 / うち取りこぼし無し`）:

```
com.tinyspeck.slackmacgap:      3 / 2 / 2
com.anthropic.claudefordesktop: 3 / 2 / 2
com.apple.Safari:               2 / 1 / 0
```

**問題は `com.apple.Safari` の行**（および `com.apple.WebKit.GPU` の行）。フレームワークに
同梱された XPC サービスが**別アプリの代理**で音声を扱う構成
（`com.apple.WebKit.GPU`、`AudioToolbox.framework/…/com.apple.audio.SandboxHelper.xpc`）では、
実行パスに `.app` が無く親も `launchd` なので、案 1・2 では帰属できない。案 3
（`kAudioProcessPropertyBundleID`）も XPC サービス自身の ID（`com.apple.WebKit.GPU`）を返すだけで、
ホストアプリは分からない。private 方式だけが `com.apple.Safari` / `com.raycast.macos` を返せている。

**これは Safari に効く**。WebKit はマイク取得を GPU プロセスで扱うため、Safari で Google Meet を
使う構成では、private 方式なら `com.apple.Safari` に帰属できる（＝Safari 登録で自動録音が動く）が、
公開 API では帰属できない可能性が高い。プランは「自動録音の劣化は受け入れない」を絶対条件に
しているので、ここが**判定を分ける唯一の点**として残る。

一方、issue が名指ししていた **Chrome / Slack / Zoom 系（バンドル内にヘルパー `.app` を持つ構成）は
公開 API で完全に解決できる**:

| プロセス | direct | private | proc_pidpath | CoreAudio |
|---|---|---|---|---|
| `Slack Helper`（`Slack.app/Contents/Frameworks/…`） | なし | `com.tinyspeck.slackmacgap` | `com.tinyspeck.slackmacgap` | `com.tinyspeck.slackmacgap.helper` |
| `Claude Helper`（`Claude.app/Contents/Frameworks/…`） | なし | `com.anthropic.claudefordesktop` | `com.anthropic.claudefordesktop` | `com.anthropic.claudefordesktop.helper` |

CoreAudio 方式が返す `….helper` は、既存の `trigger_matches`（`base` + `.` の前方一致）が親アプリ
登録で拾うため実用上は同じ。Chrome ヘルパーの実行パスは
`Google Chrome.app/Contents/Frameworks/…/Google Chrome Helper.app/…` と `.app` が入れ子になるため、
**最も外側**の `.app` を採ることが要点（内側を採ると親アプリ登録と一致しない）。

### go/no-go その 2: App Sandbox 下での動作

ad-hoc 署名＋entitlements（`app-sandbox` / `device.audio-input` / `network.client` /
`files.user-selected.read-write` / `files.bookmarks.app-scope`）で実測した。サンドボックスが
実際に効いていることは対照実験で確認済み（下記 bookmark の項）。

| 項目 | 結果 |
|---|---|
| CoreAudio のプロセス列挙（`kAudioHardwarePropertyProcessObjectList`） | **動く**（サンドボックス無しと同じ 31 プロセス） |
| `kAudioProcessPropertyIsRunningInput` / `PID` / `BundleID` | **動く**（値も同一） |
| 他プロセスへの `proc_pidpath` / `proc_pidinfo` / `NSRunningApplication` | **動く**（出力が完全に一致） |
| security-scoped bookmark（作成→保存→別プロセスで解決→書き込み） | **動く** |
| ScreenCaptureKit のシステム音声 | **動く**（ディスプレイ 3 件、音声バッファ 6 個を受信） |
| マイク取得（cpal、`device.audio-input`） | **動く**（チェックリスト外だが必須なので追加確認） |

- **bookmark**: コンテナ内フォルダで作成 856 バイト → 別プロセスで解決 → `stale: false` →
  `startAccessingSecurityScopedResource: true` → 実際に書き込み成功。
  対照として、パネルを介さずコンテナ**外**（`~/Music`）を bookmark 化しようとすると
  サンドボックス下では `The file "Music" couldn't be opened.` で失敗し、サンドボックス無しでは
  成功した。**サンドボックスが実際に効いていること**と、**API と entitlement が正しいこと**の
  両方がこれで示せている。
- **bookmark（パネル経由）**: フォルダ選択パネルで**コンテナ外**の `~/Documents/openshoki` を選ぶと
  bookmark を作成でき（716 バイト）、**別プロセス**で解決 → `stale: false` →
  `startAccessingSecurityScopedResource: true` → 書き込み成功。powerbox の付与が bookmark で
  永続化できることが確認できた（フェーズ 2 のステップ 4 は成立する）。
- **ScreenCaptureKit**: 画面収録の TCC を許可する前は
  「ユーザがアプリケーション、ウインドウ、ディスプレイ取り込みの TCC を拒否しました」で
  `SCShareableContent::get` が失敗した（サンドボックス無しの同じ `.app` でも同じ結果で、
  `sandboxd` の拒否ログも無し）。**許可後はサンドボックス下でも動作**した。つまり制約は
  サンドボックスではなく TCC であり、これは MAS でも GitHub 配布でも同じ条件。
- **マイク**: `--hold-mic` で既定入力デバイスを開いたまま走査すると、サンドボックス下でも
  自分自身が「マイク入力中」として CoreAudio に現れ、**4 方式すべてが自分のバンドル ID**
  （`net.noncore.openshoki.masprobe`）へ解決した。**サンドボックスされたアプリのマイク使用が
  `com.apple.audio.SandboxHelper` に化けるわけではない**ことが分かり、懸念していた
  「サンドボックス化が代行プロセスを生む」経路は否定できた。ただしこれは WebKit 型
  （`com.apple.WebKit.GPU`）とは別の話で、そちらは案 3 でも塞がらない（前節参照）。

### Safari の帰属（実測で確定）

`--watch-mic` でマイクを掴んでいるプロセスを見張り、Safari がマイクを使っている瞬間を捕まえた:

```
pid 75093 → private=com.apple.Safari+com.apple.WebKit.GPU
            path=com.apple.WebKit.GPU  ppid=com.apple.WebKit.GPU  coreaudio=com.apple.WebKit.GPU
            exec=WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.GPU.xpc/…
  ⚠ the public APIs cannot attribute this process to com.apple.Safari
```

つまり **Safari は現状 private 方式で `com.apple.Safari` に帰属できており（＝Safari を登録すれば
自動録音が動く）、公開 API に置き換えるとできなくなる**。推測ではなく実測。

Safari で Google Meet を開いた状態でも再確認した。90 秒の監視中にマイクの掴み・解放が 3 回
切り替わった（ミュート操作・再読み込みに対応）が、掴んでいる間は毎回この同じプロセス・同じ
解決結果だった。

影響範囲は WebKit を音声ホストに使うアプリ（Safari、WKWebView を使う Raycast 等）。
Chrome（バンドル内ヘルパー）・Zen / Firefox 系（本体プロセスが直接 ID を持つ）・Slack・Zoom は
公開 API で解決できる。

## フェーズ 1 の結論: **GO（Safari を自動録音の対象外とする）**

- **go/no-go その 2（サンドボックス）**: 文句なく成立。CoreAudio のプロセス照会・代替 API・
  ScreenCaptureKit・security-scoped bookmark・マイク取得のすべてがサンドボックス下で動いた。
  対照実験（パネルを介さないコンテナ外アクセスの拒否）でサンドボックスが実際に効いていることも
  確認済み。追加検証は不要。
- **go/no-go その 1（公開 API 代替）**: Chrome / Zen / Slack / Zoom では成立。Safari（WebKit 系）
  だけは公開 API で帰属できず現状より落ちるが、**Safari を自動録音の対象外と明示したうえで
  GO とする**（2026-07-26 のユーザー判断）。

### 判断の記録: なぜ Safari を捨てて進めるか

プランは「自動録音の劣化は受け入れない」を絶対条件に置いていたので、本来この差分は見送り事由に
あたる。それでも進めると決めた理由と、受け入れた代償を残しておく:

- 影響範囲は「**Safari で会議に出る**」構成に限られる。openshoki が主対象としてきた
  Chrome の Google Meet・Zoom.app・Slack ハドル、および Zen / Firefox 系はすべて公開 API で
  従来どおり解決できる（実測）。
- 代償: Safari を登録しても自動録音が発火しない。手動の開始・停止は従来どおり使える
  （自動録音はオプトイン機能で、既定は無効）。
- この差分は **MAS 版だけでなく GitHub 配布版にも及ぶ**。ビルドを分岐させない方針
  （private API を両配布から削除する）を維持するため。分岐させれば Safari 対応を残せるが、
  「配布経路で挙動が違う」ほうが説明もテストも難しくなると判断した。
- 将来 Apple が「オーディオプロセスの責任アプリ」を返す公開 API を追加したら、
  `examples/mas_probe.rs` を再実行して見直す（プローブはそのために残す）。

### 対象外を伝えるための作業（#107 に含める）

- README の自動録音の説明に「Safari（WebKit ベース）は対象外」を明記する。
- 設定画面でアプリを登録するとき、Safari（および WebKit を音声ホストに使うアプリ）を選んだら
  「このアプリは自動録音の対象外」と伝える。黙って発火しないのが一番悪い。

### フェーズ 2 の実装で決めておくこと（検証から得た指針）

- 解決は「最初に取れたものを使う」ではなく、本体の現行実装と同じく**得られた ID を全部集合へ
  入れる**（`kAudioProcessPropertyBundleID` / `NSRunningApplication` の直接値 /
  `proc_pidpath` の外側 `.app`）。3 つとも公開 API なのでビルドは分岐しない。1 値に畳むと、
  ヘルパー自身の ID（`….helper`）と親アプリの ID のどちらかしか照合に使えなくなる。
- `proc_pidpath` 方式は**最も外側**の `.app` を採ること（Chrome ヘルパーは
  `Google Chrome.app/…/Google Chrome Helper.app/…` と入れ子になるため、内側を採ると
  `com.google.Chrome.helper` 相当になり、親アプリ登録と一致しなくなる）。
- 親 PID を辿る案 2 は採らない。案 1 に対する追加の解決力がほぼ無く（XPC サービスは
  `launchd` の子なので辿れない）、コードだけ増える。
- 置換で「ターミナルから起動した CLI の音声を、そのターミナルの登録で拾う」挙動は無くなる。
  `docs/rules/ffi.md` と `app_audio_monitor` の doc に書いてある副作用の記述も更新すること。

## 配布方針の変更（2026-07-26）

当初は「MAS ＋ GitHub Releases の**並行提供**」を前提にしていたが、リポジトリを private に
する方針（#112）が決まったため、**配布は MAS 一本**にする。

- private リポジトリでは Releases を公開できず、Pages も使えない（有料プラン限定）。
- #20 は「`.app` バンドルを組む」ところまでをスコープとし、Releases への添付は外す
  （組んだバンドルは MAS パッケージングの土台として使う）。
- アプリ紹介 LP（#75 / #76）は専用の public リポジトリを別に作って公開する。ダウンロード導線は
  App Store へ向ける。
- このプラン本文に残る「GitHub Releases と並行提供」「機能差は作らない」といった記述は、
  この決定より前のもの。**配布経路については本節が正**。ビルドを分岐させない方針
  （private API を持たない・Safari は対象外）は変わらない。
