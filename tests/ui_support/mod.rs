//! UI 操作テスト（`tests/ui_*.rs`）で共有する土台。テストバックエンドの初期化だけを持つ。

use std::cell::Cell;

/// Slint のテストバックエンドを初期化する。**スレッドごとに 1 回だけ**呼べる仕様（2 回目は
/// `platform already initialized` で panic する）なので、`--test-threads=1` で同じスレッドに
/// 複数のテストが載っても壊れないよう thread-local のフラグで 1 回に絞る。
///
/// バックエンドはウィンドウを表示せず（ヘッドレスで動く）、`Timer` と `invoke_from_event_loop`
/// も動かない。100ms の再生 tick に依存する振る舞いはここでは再現しない。
pub fn init_backend() {
    thread_local! {
        static INITIALIZED: Cell<bool> = const { Cell::new(false) };
    }
    INITIALIZED.with(|initialized| {
        if !initialized.replace(true) {
            i_slint_backend_testing::init_no_event_loop();
        }
    });
}
