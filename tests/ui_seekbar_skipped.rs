//! デバッグ情報を持たないプロファイル（`--release` / `bench` 等）で `cargo test` したときに、
//! UI 操作テスト（`tests/ui_seekbar.rs`）が飛ばされたことを黙らせないための報告用テスト。
//! 「0 件で通った」を「実行できないので飛ばした」と区別できるようにする。

#![cfg(not(slint_debug_info))]

/// 飛ばした理由と有効化の方法を出す（`cargo test -- --nocapture` で本文が読める）。
#[test]
fn seek_bar_interaction_tests_need_slint_debug_info() {
    println!(
        "Skipped the seek bar interaction tests because the generated Slint code has no debug \
         info in this profile. Re-run with the dev/test profile, or set SLINT_EMIT_DEBUG_INFO=1 \
         (see build.rs)."
    );
}
