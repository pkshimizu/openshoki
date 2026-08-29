# 起動時に Dock アイコンを表示せずトレイのみ常駐する

- 作成日: 2026-07-14
- ステータス: ドラフト

## 概要

`cargo run` などで openshoki を起動すると、意図に反して Dock にアプリアイコンが表示される。
メニューバー常駐アプリとして、トレイだけを出し Dock アイコンは出さないようにする。既存の
`hide_dock_icon()`（activation policy を Accessory に設定）が Slint/winit の起動処理に
上書きされているのが原因で、設定するタイミングを直すことで解決する。

## 背景・前提（コンテキスト）

- openshoki は「常駐型」を前提としたメニューバー／タスクバー常駐の録音アプリ。Dock や
  アプリスイッチャーに出さず、トレイから操作する体験を中心に据える（`docs/CONTEXT.md`）。
- GUI は Slint、トレイ常駐は `tray-icon`。Slint のイベントループは winit ベース
  （`slint = 1.17.0` → `i-slint-backend-winit-1.17.0` → `winit 0.30.13`）。
- 配布時は `.app` バンドルの `Info.plist` に `LSUIElement` を持たせて Dock 非表示にする方針だが、
  そのパッケージング（`docs/plans/done/20260710-release-binary-github-actions.md`）はまだ本リポジトリに
  実装されていない。現状の起動手段は未バンドルの `cargo run`（+ `cargo dev`）。

### 調査で分かった原因

`src/main.rs` は `main()` の冒頭で `hide_dock_icon()` を呼び、`NSApplication` の activation policy を
`Accessory`（Dock・アプリスイッチャーに出さない）に設定している。しかしこの呼び出しは Slint の
イベントループ（winit）が起動する前で、winit の起動処理に後から上書きされる。

winit 0.30.13 の macOS 実装 `applicationDidFinishLaunching:`（`platform_impl/macos/app_state.rs`）は
次のように振る舞う:

- Slint は winit に activation policy を明示指定しない（＝ `None`）。
- `None` のとき winit は、
  - **バンドル済み**（`bundleIdentifier` が存在）なら policy を一切設定せず、`Info.plist` の
    `LSUIElement` に委ねる。
  - **未バンドル**（`cargo run` の生バイナリ）なら `setActivationPolicy(Regular)` を強制する。

このため未バンドル起動では、`main()` 冒頭で設定した `Accessory` が、イベントループ開始時
（`applicationDidFinishLaunching:`）の `Regular` 設定で上書きされ、Dock アイコンが出る。
一方、`LSUIElement` 付きの `.app` バンドルでは winit が policy を触らないため Dock には出ない
（＝リリース版は将来のパッケージングで解決される）。

## 要件

- 未バンドル起動（`cargo run` / `cargo dev`）で、起動後に Dock アイコンが表示されず、トレイ
  だけが常駐する。
- トレイメニュー・設定ウィンドウ・録音機能など既存の挙動は一切変えない。
- スコープ外:
  - リリース `.app` バンドルの `Info.plist`（`LSUIElement`）整備。未バンドルでは無効で、
    パッケージング実装時に扱う（`LSUIElement` があれば winit は policy を触らないため別途対応で足りる）。
  - Windows / Linux のタスクバー挙動（本件は macOS の Dock に限る）。

## 確定した論点

- **原因はタイミング**（調査で確定）: `hide_dock_icon()` が早すぎて winit の起動時設定に負ける。
  設定ロジック自体（`setActivationPolicy(Accessory)`）は正しい。
- **修正はイベントループ開始後に Accessory を適用する**: winit が `Regular` を設定するのは
  `applicationDidFinishLaunching:` の 1 回だけ（グローバルに一度）なので、その後に一度
  `Accessory` へ戻せば恒久的に維持される。
- **未バンドル起動時の一瞬のちらつきは許容する**: winit が `Regular` にした直後に `Accessory`
  へ戻すため、理論上ごく短時間 Dock アイコンが見える可能性があるが、同一ランループ内での
  適用で実害は小さい。ちらつきを完全に消すには `.app` バンドル + `LSUIElement` が必要で、
  それはパッケージング側の解決（スコープ外）。
- **i18n 等と同様、追加依存は入れない**: 既に使っている `objc2` / `objc2-app-kit` の範囲で行う。

## 実装方針

`hide_dock_icon()` の呼び出しを、`main()` 冒頭ではなく **Slint イベントループが動き始めた後**に
一度実行するよう移す。具体的には `slint::run_event_loop_until_quit()` を呼ぶ前に
`slint::invoke_from_event_loop(...)` で `hide_dock_icon()` をキューし、ループ開始後
（＝winit の `applicationDidFinishLaunching:` が `Regular` を設定した後）に実行させる。これにより
winit の設定を確実に上書きして `Accessory` を維持する。`hide_dock_icon()` 関数自体は変更しない。

補足: `main()` 冒頭の早すぎる呼び出しは、未バンドルでは上書きされて無意味なので削除する。
バンドル済みでは winit が policy を触らないため、そもそも早期呼び出しは不要（`LSUIElement`
またはこの遅延適用のどちらでも `Accessory` を維持できる）。呼び出し箇所を 1 か所に集約して
「いつ効くのか」を分かりやすくする。

（代替案として「最初のタイマー tick で 1 回だけ適用」もあるが、`invoke_from_event_loop` の
方が意図が明確で 100ms を待たずに適用できるため採用する。）

## 実装ステップ

1. `src/main.rs` の `main()` 冒頭にある `hide_dock_icon()` 呼び出し（`#[cfg(target_os = "macos")]`）
   を削除する。
2. `slint::run_event_loop_until_quit()` の直前に、`#[cfg(target_os = "macos")]` で
   `slint::invoke_from_event_loop(|| hide_dock_icon())` を追加し、ループ開始後に一度だけ
   `Accessory` を適用する（`invoke_from_event_loop` が返す `Result` は握りつぶさずログに残す。
   `docs/rules/error-handling.md`）。
3. 関連コメント（`main()` 冒頭の「常駐アプリとして Dock にアイコンを出さない」）を、
   「なぜループ開始後に設定するのか（winit が起動時に Regular を強制するため）」が分かる
   説明へ更新する。`hide_dock_icon()` の doc コメントにも同趣旨を補足する。
4. 検証（macOS 実機）:
   - `cargo run`（または `cargo dev`）で起動し、**Dock にアイコンが出ない**こと、トレイ
     アイコン・メニュー・設定ウィンドウ・録音開始/停止が従来どおり動くことを目視確認する。
   - `cargo build` / `cargo clippy --all-targets -- -D warnings` / `cargo test` が通ることを確認する。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール: `src/main.rs`（`main()` の呼び出し位置と関連コメントのみ。
  `hide_dock_icon()` 本体は不変）。
- リスクと対策:
  - **`invoke_from_event_loop` の実行順**: winit の `applicationDidFinishLaunching:`（Init 時）で
    `Regular` が設定された後にキュー済みクロージャが処理されるため、上書き順は担保される。
    実機起動で Dock 非表示を目視確認する。
  - **起動時の一瞬のちらつき**: 許容（前述）。完全排除はバンドル + `LSUIElement` で別途。
  - **他プラットフォームへの影響**: 変更は `#[cfg(target_os = "macos")]` 配下のみで、
    Windows/Linux のビルド・挙動に影響しない。

## 未確定事項

- リリース `.app` バンドルに `LSUIElement` を入れる対応はパッケージング実装時に別途行う
  （本プランのスコープ外）。パッケージング着手時、この遅延適用と `LSUIElement` が二重に
  効いても害はないため、そのまま共存させてよい。
