//! Recordings ウィンドウ（Transcript・Playback セクション）の描画確認用バイナリ
//! （`docs/rules/slint.md` の検証手順）。ダミーの状態を流し込んで RecordingsWindow を表示する。
//! 実行: `cargo run --example transcript_view [引数...]` → screencapture で目視確認。
//!
//! 引数（順不同・組み合わせ可）:
//! - 数値: セグメント件数（0 で Transcript の縮退表示。既定は `DEFAULT_SEGMENT_COUNT`）
//! - `modal`: 削除確認モーダルを重ねた状態
//! - `no-seek`: シークバーを表示専用へ縮退させた状態（再生不可・全体長不明のセッション相当）

slint::include_modules!();

use std::rc::Rc;

use slint::{ModelRc, VecModel};

/// 引数で件数を指定しなかったときのセグメント件数。
const DEFAULT_SEGMENT_COUNT: usize = 30;

fn main() {
    let win = RecordingsWindow::new()
        .expect("creating the window should succeed in this verification binary");

    // 引数はフラグと混ざるため、位置ではなく「数値として読めた最初の引数」を件数にする。
    let count: usize = std::env::args()
        .skip(1)
        .find_map(|arg| arg.parse().ok())
        .unwrap_or(DEFAULT_SEGMENT_COUNT);
    let rows: Vec<TranscriptRow> = (0..count)
        .map(|i| TranscriptRow {
            speaker: if i % 2 == 0 { "Mic" } else { "System" }.into(),
            is_mic: i % 2 == 0,
            time: format!("{:02}:{:02}", i / 6, (i * 13) % 60).into(),
            text: format!(
                "Segment {i}: this is a fairly long transcript line that should wrap onto \
                 multiple lines when the pane is narrow enough to require word wrapping."
            )
            .into(),
        })
        .collect();
    win.set_segments(ModelRc::from(Rc::new(VecModel::from(rows))));
    win.set_has_selection(true);
    win.set_detail_datetime("2026-07-21 12:00:00".into());
    win.set_detail_summary("Mic + System".into());
    win.set_detail_transcript_text("Transcribed".into());
    win.set_current_segment(2);
    // Playback セクション（シークバー）の確認用のダミー再生状態。
    win.set_playable(true);
    win.set_progress(0.35);
    win.set_time_text("01:45 / 05:00".into());
    win.set_seekable(!std::env::args().any(|arg| arg == "no-seek"));
    if std::env::args().any(|arg| arg == "modal") {
        win.set_show_delete_confirm(true);
    }

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    win.window().set_size(slint::LogicalSize::new(720.0, 540.0));
    win.show()
        .expect("showing the window should succeed in this verification binary");
    slint::run_event_loop().expect("the event loop should run in this verification binary");
}
