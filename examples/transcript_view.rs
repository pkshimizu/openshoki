//! Recordings ウィンドウ（Transcript / Summary・Playback セクション）の描画確認用バイナリ
//! （`docs/rules/slint.md` の検証手順）。ダミーの状態を流し込んで RecordingsWindow を表示する。
//! 実行: `cargo run --example transcript_view [引数...]` → screencapture で目視確認。
//!
//! 引数（順不同・組み合わせ可）:
//! - 数値: セグメント件数（0 で Transcript の縮退表示。既定は `DEFAULT_SEGMENT_COUNT`）
//! - `modal`: 削除確認モーダルを重ねた状態
//! - `no-seek`: シークバーを表示専用へ縮退させた状態（再生不可・全体長不明のセッション相当）
//! - `summary`: Summary タブを開いた状態（議事録の見出し強調・折り返しの確認）
//! - `no-summary`: Summary タブを開き、議事録を未生成にした状態（縮退表示・Summarize の無効化）
//! - `snapshot <path>`: PNG に書き出す（画面収録の許可が無い環境用。`settings_view` と同じ）

slint::include_modules!();

use std::rc::Rc;

use slint::{ModelRc, VecModel};

/// 引数で件数を指定しなかったときのセグメント件数。
const DEFAULT_SEGMENT_COUNT: usize = 30;

/// Summary タブの確認用のダミー議事録。見出しの強調・本文の折り返し・段落の間隔を見たいので、
/// 実際の生成物（`src/summarize.rs` のプロンプトが作る 4 見出し構成）と同じ形にする。
fn sample_summary_rows() -> Vec<SummaryRow> {
    let row = |text: &str, heading: bool| SummaryRow {
        text: text.into(),
        heading,
    };
    vec![
        row("議事概要", true),
        row(
            "リリース判定の打ち合わせ。残課題の確認と、次スプリントの分担を決めた。長い本文が \
             ペインの幅で折り返されることも確認する。",
            false,
        ),
        row("", false),
        row("議題内容", true),
        row("- 残課題の棚卸し（3 件）", false),
        row("- 検証環境の再構築の要否", false),
        row("", false),
        row("決定事項", true),
        row("- リリースは来週前半に実施する", false),
        row("", false),
        row("アクションアイテム", true),
        row("- <担当者>: 検証環境の再構築（期限: 未定）", false),
    ]
}

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
    win.set_has_transcript(true);
    win.set_current_segment(2);
    // Summary タブ。生成済み（行あり）と未生成（縮退表示）を引数で切り替える。文言は
    // `src/main.rs` の summary_status_text / summary_placeholder_text の複製（bin クレートなので
    // import できない。あちらを変えたらここも合わせること）。
    if std::env::args().any(|arg| arg == "no-summary") {
        win.set_detail_summary_status(SummaryStatus::NotSummarized);
        win.set_detail_summary_status_text("Not summarized".into());
        win.set_detail_summary_placeholder("Not Summarized Yet".into());
        win.set_has_transcript(false);
    } else {
        win.set_detail_summary_status(SummaryStatus::Done);
        win.set_detail_summary_status_text("Summarized".into());
        win.set_detail_summary_placeholder("Not Summarized Yet".into());
        win.set_summary_rows(ModelRc::from(Rc::new(
            VecModel::from(sample_summary_rows()),
        )));
    }
    // タブは通常 UI が所有するが、Summary 側の見た目を確認できるよう引数で選べるようにする。
    win.set_showing_summary(std::env::args().any(|arg| arg == "summary" || arg == "no-summary"));
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

    // `snapshot <path>`: 最初のフレームが描かれてから書き出す（ループ開始前だと中身が空になる。
    // 画面収録の許可が無い環境でも見た目を確認できる。`examples/settings_view.rs` と同じ）。
    let snapshot_path = std::env::args()
        .skip_while(|arg| arg != "snapshot")
        .nth(1)
        .map(std::path::PathBuf::from);
    let timer = slint::Timer::default();
    if let Some(path) = snapshot_path {
        let handle = win.as_weak();
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(500),
            move || {
                if let Some(win) = handle.upgrade() {
                    write_snapshot(&win, &path);
                }
                slint::quit_event_loop().expect("quitting the event loop should succeed");
            },
        );
    }

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}

fn write_snapshot(win: &RecordingsWindow, path: &std::path::Path) {
    let buffer = match win.window().take_snapshot() {
        Ok(buffer) => buffer,
        Err(err) => {
            eprintln!("Could not take a snapshot: {err}");
            return;
        }
    };
    let file = match std::fs::File::create(path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("Could not create {}: {err}", path.display());
            return;
        }
    };
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        buffer.width(),
        buffer.height(),
    );
    encoder.set_color(png::ColorType::Rgba);
    let write = encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(buffer.as_bytes()));
    match write {
        Ok(()) => println!("Wrote {}", path.display()),
        Err(err) => eprintln!("Could not write {}: {err}", path.display()),
    }
}
