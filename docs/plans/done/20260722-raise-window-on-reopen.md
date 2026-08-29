# メニューから開き直したとき、開いているウィンドウを前面に表示する

- 作成日: 2026-07-22
- ステータス: 確定

## 概要

トレイメニューから設定ウィンドウ・Recordings ウィンドウを開こうとしたとき、そのウィンドウが
既に開いていて他アプリのウィンドウに隠れていると、何も起きないように見える。ウィンドウが
既に開いている場合は前面に出してキーウィンドウにし、「メニューを押したのに反応しない」
体験をなくす。

## 背景・前提（コンテキスト）

- ウィンドウ表示は `src/main.rs` の `show_window()`（設定・Recordings 共通）で行い、
  初回のみジオメトリを明示してから `window.show()` を呼ぶ（`docs/rules/slint.md`）。
  **`show()` は既に表示中のウィンドウには no-op** で、前面化もフォーカスもしない。
- 本アプリは Dock に出ない **Accessory 常駐アプリ**（`hide_dock_icon()` が
  `NSApplicationActivationPolicy::Accessory` を設定）。トレイメニューのクリックでは
  アプリ自体がアクティブ化されないため、ウィンドウが表示中でも他アプリの背後に残る。
- Slint の公開 API にはウィンドウの raise / focus が無い。ネイティブ操作が必要:
  - `objc2` / `objc2-app-kit` は既存依存（`hide_dock_icon` で `NSApplication` を使用中）。
  - Slint ウィンドウから NSWindow を得るには `slint::Window::window_handle()` の
    raw-window-handle 連携を使う（Slint の `raw-window-handle-06` feature ＋
    `raw-window-handle` クレート。AppKit ハンドルは NSView を指すので `view.window()` で
    NSWindow を得る）。
- 閉じるボタンはウィンドウを hide する運用のため、「閉じた後に開き直す」経路は現状の
  `show()` で動いている。問題は「表示中＋背後」の経路のみ。

## 要件

- トレイメニューの「Open Settings」「Recordings…」で、対象ウィンドウが既に表示中なら
  前面に出してキーウィンドウ（フォーカス）にする。
- 非表示（閉じた後）の場合は従来どおり表示する（現状の挙動を維持）。
- Cmd+M で最小化（miniaturize）されている場合も、メニューから開いたら復元して前面に出す。
- 設定・Recordings の両方が開いているとき、選んだメニューに対応するウィンドウがキーになる
  （もう一方だけが前に来ない）。
- macOS 以外では現状の `show()` にフォールバックする（前面化はベストエフォート。
  現状の主対象は macOS）。
- スコープ外: グローバルショートカット、複数ディスプレイ間の位置調整、Dock 復帰時の挙動。

## 確定した論点

（調査で解消。ユーザーへの質問なし）

- **原因**: `show()` の no-op ＋ Accessory アプリが非アクティブのままであること（上記）。
- **方式**: `show_window()` の末尾に前面化処理を追加する。macOS では
  1. raw-window-handle 経由で対象の NSWindow を取得し、
  2. miniaturized なら `deminiaturize`、`makeKeyAndOrderFront(None)` で前面化・キー化、
  3. `NSApplication` をアクティブ化する（Accessory アプリはこれをしないと他アプリの
     前に出ない。既存の `hide_dock_icon` と同じ `MainThreadMarker` パターン）。
  対象 NSWindow を直接 `makeKeyAndOrderFront` するため、両ウィンドウが開いていても
  選んだ方がキーになる。
- **依存の追加は最小**: `raw-window-handle` クレートと Slint の `raw-window-handle-06`
  feature のみ（objc2 系は既存依存）。
- FFI 規約（`docs/rules/ffi.md`）に従い、ハンドル取得失敗（非 AppKit バックエンド等）は
  ログして `show()` のみで続行する（落とさない・握りつぶさない）。

## 実装方針

`src/main.rs` の `show_window()` を「表示＋前面化」に拡張する（呼び出し側は変更不要。
設定・Recordings の両方が同じ経路を通る）:

```text
show_window():
    初回のみ set_position / set_size（既存）
    window.show()（既存。非表示→表示の経路）
    bring_to_front(window)（新設）
```

- `bring_to_front(window: &slint::Window)`（macOS 実装＋他 OS は no-op スタブ）:
  - `window.window_handle()` → `raw_window_handle::RawWindowHandle::AppKit` → `NSView`
    ポインタ → `view.window()` で `NSWindow` を取得。取得できなければログして return。
  - `isMiniaturized` なら `deminiaturize(None)`。
  - `makeKeyAndOrderFront(None)`。
  - `NSApplication::sharedApplication(mtm)` を `activate()`（macOS 14+。それ以前も
    考慮するなら `activateIgnoringOtherApps(true)`。最低対応が macOS 13 のため後者を採用）。
  - ポインタの参照化は表示中のウィンドウに対して行う（メインスレッド上・Slint が
    ウィンドウを所有している間のみ触る）。SAFETY コメントに前提を書く。

## 実装ステップ

1. **依存と前面化ヘルパの追加**
   `Cargo.toml` に `raw-window-handle` と Slint の `raw-window-handle-06` feature を追加し、
   `bring_to_front()` を実装して `show_window()` から呼ぶ。
   確認: `cargo build` / `cargo clippy --all-targets -- -D warnings` が通る。
2. **実機での動作確認**
   `cargo run` で次を確認する:
   - 設定ウィンドウを開く → 他アプリのウィンドウで覆う → メニューから開く → 前面に出て
     キーになる（Recordings も同様）。
   - Cmd+M で最小化 → メニューから開く → 復元して前面に出る。
   - 閉じる（hide）→ メニューから開く → 従来どおり表示される（回帰なし）。
   - 両ウィンドウを開いた状態で片方をメニューから開く → そのウィンドウがキーになる。
3. **ドキュメント同期**
   `docs/CONTEXT.md` の該当箇所（ウィンドウ表示の設計判断）に前面化の一文を追加する。
   確認: `cargo build` / `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
   `cargo test` がすべて通る。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - `src/main.rs`（`show_window` の拡張と `bring_to_front` の新設）
  - `Cargo.toml`（`raw-window-handle` 追加・slint feature 追加）
  - `docs/CONTEXT.md`（設計判断の追記）
- リスクと対策:
  - **raw-window-handle の取得失敗**（バックエンド差異・将来の Slint 変更）: 失敗時は
    ログして `show()` のみで続行（現状と同じ挙動に縮退。`docs/rules/error-handling.md`）。
  - **`activateIgnoringOtherApps` の副作用**（他アプリからフォーカスを奪う）: ユーザーが
    メニューをクリックした直後にのみ呼ばれる（ユーザー起点の操作への応答）ため許容。
    バックグラウンドから勝手に呼ぶ経路は作らない。
  - **NSWindow ポインタの寿命**: メインスレッド上で、Slint がウィンドウを保持している間に
    のみ参照する（`docs/rules/ffi.md`。SAFETY コメントに前提を明記）。
  - **検証の自動化が難しい**: ウィンドウの前後関係はスクリーンショットでも判定しづらい
    ため、実機の手動確認を受け入れ条件にする（`bring_to_front` は「パニックせず戻る」
    スモークテストのみ置く）。

## 未確定事項

- なし
