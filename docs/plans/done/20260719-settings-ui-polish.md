# 設定ウィンドウの UI デザイン改善

- 作成日: 2026-07-19
- ステータス: ドラフト

## 概要

設定ウィンドウ（`ui/app-window.slint`）の見た目とレイアウトを整える。機能は一切変えず、
現状の平坦な縦積みを「セクション枠＋余白＋階層」で構造化し、関連コントロールのまとまりと
依存関係（自動停止 delay・Trigger apps が自動録音トグルに従属すること）を視覚的に分かりやすく
する。**デザインカンプは claude design で作成済み**で、本プランはそれを実装へ落とし込む指針。

- デザインカンプ: https://claude.ai/design/p/2d761dcc-fbcf-4fd7-8cc6-c5a58d0d036c?file=openshoki+Settings.dc.html
  （`openshoki Settings.dc.html`。Light/Dark × 自動録音 ON/OFF の 4 バリアント、420px 幅）

## 背景・前提（コンテキスト）

- GUI は **Slint**（`slint = "1.17.0"`）。設定ウィンドウは `ui/app-window.slint` 単一。
  スタイルは未指定で、既定スタイル（システムのライト/ダークに追従）を使う。
- ユーザー向け文言は英語（`docs/rules/messages.md`）。GUI ラベルは Title Case。製品名
  `openshoki` は保持。
- ウィンドウは常駐アプリのトレイメニューから開く。初回 show はジオメトリを明示する必要があり
  （`docs/rules/slint.md`）、`src/main.rs` の `WINDOW_WIDTH/HEIGHT` と `.slint` の
  `min/preferred-*` を**一致させる**規約がある（片方だけ変えない）。
- 現状の構成（縦 `VerticalBox`、padding 20 / spacing 12、固定 420×530）:
  1. Text「openshoki Settings」（20px 中央）
  2. Text「Recording folder」＋ 保存先パス（グレー）＋ Button「Choose Folder」
  3. CheckBox「Auto-record while a registered app uses the mic … (macOS 14.4+)」
  4. 「Auto-stop delay (seconds)」ラベル＋ SpinBox（1–60、`enabled: auto-record-app`）
  5. Text「Trigger apps」＋ ScrollView（高さ120）内の登録アプリ一覧（各行 Text＋Remove）＋
     Button「Add app…」
- Slint 側の公開プロパティ/コールバック（**改名しない**。Rust 側 `src/main.rs` が依存）:
  `recording-dir` / `auto-record-app` / `auto-stop-debounce-secs` / `app-list`、
  `choose-folder` / `toggle-auto-record-app` / `change-auto-stop-debounce` / `add-app` /
  `remove-app`。

## 要件

- デザインカンプの構成・階層・状態表現を、std-widgets の範囲で再現する（下記「デザイン仕様」）。
- 設定項目を意味のまとまりで**セクション化**する: 「Recording」「Auto-record」。
- **依存コントロールをインデント**して従属関係を示す。自動録音 OFF のときは従属コントロール
  （delay・Trigger apps の一覧/追加/削除）を**無効化**し、薄く表示する。
- ライト/ダーク両テーマで破綻しない（既定スタイル追従を維持）。
- ウィンドウは**固定幅 420px のまま**、高さは新レイアウトの内容に合わせて調整する
  （`.slint` と `WINDOW_HEIGHT` を一致させる）。
- スコープ外:
  - 機能・挙動の変更（コールバック・プロパティの意味/名前、保存ロジック、値の範囲など）。
  - Slint スタイルの明示切替（cupertino 等）。
  - ウィンドウのリサイズ対応。
  - Recordings ウィンドウ（別 issue #53/#54）や他画面。

## デザイン仕様（カンプから抽出）

カンプ `openshoki Settings.dc.html` の再現ポイント。ピクセル値は Slint 実装時の目安
（±1–2px の丸めは可。既定スタイルのウィジェット固有の見た目は尊重する）。

### 構造

```
openshoki Settings                ← タイトル（中央・semibold・約15px、下余白 16）
┌─ Recording ────────────────┐   ← セクション枠（角丸8・1px 枠線・padding 約13-15）
│ Folder                     │   ← 本文ラベル（13px）
│ ~/Documents/openshoki  [Choose Folder] │ ← パス（12px・グレー・折返し）と
└────────────────────────────┘      ボタンを同一行・両端揃え
        （セクション間 14）
┌─ Auto-record ──────────────┐
│ [x] Auto-record while a registered app │ ← チェックボックス＋複数行ラベル
│     uses the mic — for calls (macOS 14.4+) │
│     ┌ 従属ブロック（左インデント約23 = チェック幅+gap、上余白14、行間13）
│     │ Auto-stop delay (seconds)   [ 4 ▲▼] │ ← ラベル左・SpinBox 右の両端揃え
│     │ Trigger apps                        │
│     │ ┌──────────────────────────┐        │ ← フィールド背景＋角丸6＋1px 枠の
│     │ │ Zoom            [Remove] │        │   リストボックス。行間に罫線、
│     │ │ Google Chrome   [Remove] │        │   行 padding 約 6/12
│     │ └──────────────────────────┘        │
│     │ [Add app…]                          │ ← 左寄せ
└────────────────────────────┘
```

### 視覚階層・状態

- タイポグラフィ: タイトル 15px semibold（現行 20px から縮小）> セクション見出し 12px
  semibold > 本文 13px > 補助（パス）12px グレー。
- セクションは「枠線＋角丸＋内側見出し」の箱。**塗りは持たず**ウィンドウ背景と同色
  （枠線のみで区切る）。
- Trigger apps 一覧は**フィールド風**（入力欄と同じ背景色）の箱に行区切り罫線。空のときも
  箱の枠は保つ。
- 無効化（自動録音 OFF）: 従属ブロック全体を `enabled: false` ＋ **opacity ≈ 0.4** で薄くする
  （カンプは light 0.40 / dark 0.38。Slint では一律 0.4 でよい）。
- 配色はハードコードせず、std-widgets の **`Palette`**（`Palette.background` /
  `Palette.foreground` / `Palette.border` / `Palette.control-background` 等）でテーマ追従させる。
  カンプの具体色（light: bg #ECECEC・枠 #d2d2d5・フィールド #fff、dark: bg #2b2b2d・
  枠 #48484a・フィールド #1f1f21、アクセントはシステム標準の青）は Palette が近似する想定で、
  再現しきれない部分は既定スタイルの色を優先する。
- ウィンドウ余白: 上 18 / 左右 20 / 下 22 目安。

## 確定した論点

ユーザー確認で決めた事項:

1. **方向性**: **std-widgets の範囲で整える**（採用）。カスタムスタイルや cupertino 切替は
   採らない（低リスク・ネイティブ寄り・機能不変を優先）。
2. **依存の見せ方**: **セクション見出し＋依存をインデント**（採用）。OFF 時は従属を無効化。
3. **ウィンドウ**: **固定のまま整理**（採用）。リサイズ対応はしない。
4. **テーマ**: 既定スタイル（システムのライト/ダーク追従）を維持。
5. **セクションの表現**（カンプで確定）: `GroupBox` ではなく**「枠付き Rectangle（角丸・
   1px 枠線・塗りなし）＋ 内側に見出し Text」**で組む。カンプの見た目（見出しが枠の内側、
   フラットな箱）は GroupBox の既定描画よりこの構成が近く、Palette を使えばテーマ追従も保てる。
6. **Recording 行のレイアウト**（カンプで確定）: パスと「Choose Folder」を**同一行・両端揃え**
   にする（現行の縦積みから変更）。長いパスは折り返し、ボタンは折り返さない。

## 実装方針

- `ui/app-window.slint` のレイアウトを「デザイン仕様」どおりに再構成する。
  - セクション: `Rectangle { border-width: 1px; border-color: Palette.border; border-radius: 8px; }`
    ＋ 内側 `VerticalBox`（見出し Text → 中身）。共通化のため `SettingsSection`
    （title プロパティ＋ `@children`）のような小コンポーネントを `.slint` 内に定義してよい。
  - Trigger apps 一覧: フィールド風 Rectangle（`Palette.control-background` 等）＋ 行区切り。
    現行の `ScrollView`（高さ 120 目安）は箱の中に維持してスクロール可能に保つ。
  - 従属ブロック: 左インデント（約 23px）した `VerticalBox` にまとめ、
    `enabled: root.auto-record-app` と `opacity: root.auto-record-app ? 1 : 0.4` を親に付けて
    一括制御（SpinBox 個別の enabled 連動は親ゲートに統合）。
  - チェックボックスのラベルは複数行のため、既定 CheckBox の text ではなく
    「CheckBox（テキストなし）＋ 折返し Text」の横並びが必要になる可能性がある。まず
    CheckBox.text の折返し挙動を確認し、折り返せない場合のみ分解する（クリック領域の維持に注意）。
- **文言・機能は現状維持**。プロパティ/コールバックは不変。Rust 側は `WINDOW_HEIGHT` の
  再調整のみ（新レイアウトはやや縦詰めになるため、実測して `.slint` と一致させる）。
- **目視検証**: `examples/` に設定ウィンドウを表示する確認用バイナリを置き、`screencapture` で
  ライト/ダーク × ON/OFF の 4 状態をカンプと見比べる（カンプと同じマトリクスで確認）。

## 実装ステップ

1. **セクション化**: `SettingsSection`（枠付き Rectangle＋見出し）を定義し、「Recording」
   「Auto-record」へ再編。Recording はパス＋ボタンを同一行に。結線は現状のまま移し替え、
   ビルドと全操作（フォルダ選択・トグル・SpinBox・追加/削除）の動作を確認。
2. **階層・無効化**: 従属ブロックをインデントし、`enabled`＋`opacity` の親ゲートで一括制御。
   OFF で薄く操作不可、ON で復帰することを確認。
3. **リストボックス化**: Trigger apps をフィールド風の箱＋行区切りに変更（ScrollView 維持）。
   空一覧でも枠が保たれることを確認。
4. **タイポグラフィ・余白**: デザイン仕様のサイズ・余白（タイトル 15/見出し 12/本文 13/補助 12、
   セクション間 14 等）へ調整。
5. **サイズ調整**: 実測で `.slint` の `min/preferred-height` と `src/main.rs` の
   `WINDOW_HEIGHT` を一致させて再設定。クリッピングがないか確認。
6. **テーマ確認**: `examples/` の確認用バイナリ＋`screencapture` で、ライト/ダーク × ON/OFF の
   4 状態をカンプと見比べる。
7. **仕上げ**: `cargo build` / `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
   `cargo test` を通し、実機で設定ウィンドウの一連の操作を確認。

## 影響範囲・リスク

- **影響を受けるファイル/モジュール**:
  - 変更: `ui/app-window.slint`（レイアウト再構成が主）。
  - 場合により: `src/main.rs`（`WINDOW_HEIGHT` の再調整のみ。ロジックは触らない）。
  - 追加（任意）: `examples/`（目視確認用バイナリ）、`build.rs`（examples を足す場合のコンパイル）。
- **リスクと対策**:
  - **機能退行**: 要素の移動でコールバック結線を取りこぼす恐れ。プロパティ/コールバック名は
    不変とし、手順1で全操作の動作確認を必ず行う。
  - **サイズ不一致**: `.slint` と `WINDOW_HEIGHT` の片方だけ変えると初回表示が崩れる
    （`slint.md`）。両方を必ず一致させる。
  - **Palette の色差**: 既定スタイルの Palette がカンプの色と完全一致しない。ピクセル一致は
    目標にせず「構造・階層・状態表現の再現」を合格基準にする（配色はテーマ追従を優先）。
  - **CheckBox の複数行ラベル**: 折り返し非対応なら CheckBox＋Text に分解する。その際
    ラベルクリックでトグルできるよう TouchArea を検討（できなければチェック部のみで妥協し、
    実装時に判断）。
  - **無効化の一貫性**: 従属ブロックを 1 つの `enabled`/`opacity` ゲートにまとめ、
    部分的な無効化漏れを防ぐ。

## 未確定事項

- 高さの最終値は再構成後の実測で決める（固定幅 420 は確定）。
- CheckBox 複数行ラベルの実現方法（text 折返し or CheckBox＋Text 分解）は実装時に確認して決める。
- `examples/` の確認用バイナリを成果物に含めるかは着手時に判断（`slint.md` は examples/ での
  確認を推奨）。
