//! Transcript セクションの描画確認用バイナリ（`docs/rules/slint.md` の検証手順）。
//! ダミーのセグメントを流し込んで RecordingsWindow を表示する。
//! 実行: `cargo run --example transcript_view` → screencapture で目視確認。

slint::include_modules!();

use std::rc::Rc;

use slint::{ModelRc, VecModel};

fn main() {
    let win = RecordingsWindow::new()
        .expect("creating the window should succeed in this verification binary");

    // セグメント件数は引数で変えられる（0 で縮退表示の確認）。既定 30。
    let count: usize = std::env::args()
        .nth(1)
        .and_then(|arg| arg.parse().ok())
        .unwrap_or(30);
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
    // 引数に "modal" を含めると削除確認モーダルを重ねた状態で表示する（#66 の検証）。
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
