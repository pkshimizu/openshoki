//! UI 操作テストの共通土台。テストバックエンドの初期化だけを持つ（UI テストを増やすときに
//! thread-local ガードをコピペしないよう切り出してある）。

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

/// 設定ウィンドウの本文がすべて収まる高さにする。
///
/// #127 で本文が `ScrollView` に入ったため、**既定の大きさだと画面外の要素が見つからない**
/// （下端にある議事録のコントロールなど）。要素を探す前にこれを呼び、スクロール位置に
/// 依存しないテストにする。高さは実アプリの標準（900）より十分大きい値。
// このモジュールは `#[path]` で各テストバイナリへ取り込まれるので、**使わないバイナリでは
// 未使用**になる（設定ウィンドウを開かないテストなど）。共通土台としては正しい形なので、
// dead_code は許可する。
#[allow(dead_code)]
pub fn fit_settings_content(window: &impl slint::ComponentHandle) {
    window
        .window()
        .set_size(slint::LogicalSize::new(460.0, 1800.0));
}
