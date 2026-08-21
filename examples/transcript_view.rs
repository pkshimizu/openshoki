//! Recordings ウィンドウ（Transcript / Summary・Playback セクション）の描画確認用バイナリ
//! （`docs/rules/slint.md` の検証手順）。ダミーの状態を流し込んで RecordingsWindow を表示する。
//! 実行: `cargo run --example transcript_view [引数...]` → screencapture で目視確認。
//!
//! 引数（順不同・組み合わせ可）:
//! - 数値: セグメント件数（0 で Transcript の縮退表示。既定は `DEFAULT_SEGMENT_COUNT`）
//! - `modal`: 削除確認モーダルを重ねた状態
//! - `no-seek`: シークバーを表示専用へ縮退させた状態（再生不可・全体長不明のセッション相当）
//! - `summary`: Summary タブを開いた状態（議事録の見出し強調・折り返しの確認）
//! - `queued` / `summarizing` / `summary-failed`: Summary タブを開き、キュー待ち／生成中／
//!   失敗の状態にした状態（状態行の色・縮退ラベル・状態行の隣に出る取り消しの確認）
//! - `no-transcript`: 文字起こしも議事録も無いセッション（両タブの縮退表示・Summarize の無効化）。
//!   要約は入力が無いので、上の要約状態の指定より優先される
//! - `transcribing` / `transcript-failed`: Transcript タブの空表示を実行中／失敗にする
//!   （見出し・理由・操作の 3 段と、最長の理由の折り返しの確認。件数 0 と組み合わせる）
//! - `no-follow`: 再生位置の追従を OFF にした状態（プレイヤー帯のスイッチの確認）
//! - `far`: 再生位置を一覧の末尾寄りに置く（追従でその行が見えているかの確認。`no-follow`
//!   と組み合わせると先頭のままになる）。**スナップショットは表示後の更新を反映しない**ので、
//!   追従はここで見える初期表示ぶんだけを確認できる
//! - `snapshot <path>`: PNG に書き出す（画面収録の許可が無い環境用）

slint::include_modules!();

#[path = "verification/snapshot.rs"]
mod snapshot;

use std::rc::Rc;

use slint::{ModelRc, VecModel};

/// 引数に含まれるフラグか。
fn flag(name: &str) -> bool {
    std::env::args().any(|arg| arg == name)
}

/// 引数で件数を指定しなかったときのセグメント件数。
const DEFAULT_SEGMENT_COUNT: usize = 30;

/// 生成中のラベル。状態テキストと縮退ラベルで同じ文言を使うため 1 箇所に置く
/// （`src/main.rs` の `SUMMARIZING_LABEL` の複製。あちらを変えたらここも合わせること）。
const SUMMARIZING_LABEL: &str = "Writing notes…";

/// キュー待ちのラベル（同上。`src/main.rs` の複製。状態行と空表示で同じ文言）。
const SUMMARY_QUEUED_LABEL: &str = "Waiting to summarize…";

/// 空表示のボタン列を Slint のモデルにする。
fn pane_actions(actions: Vec<(&str, PaneActionKind, bool)>) -> ModelRc<PaneAction> {
    ModelRc::from(Rc::new(VecModel::from(
        actions
            .into_iter()
            .map(|(label, kind, primary)| PaneAction {
                label: label.into(),
                kind,
                primary,
            })
            .collect::<Vec<_>>(),
    )))
}

/// Transcript タブの空表示（`src/main.rs` の `TranscriptPane::message` の複製。bin クレート
/// なので import できない。あちらを変えたらここも合わせること）。
fn transcript_empty_state() -> (
    &'static str,
    &'static str,
    Vec<(&'static str, PaneActionKind, bool)>,
) {
    if flag("transcribing") {
        return (
            "Transcribing — 48%",
            "Medium is running on this Mac. Finished lines appear here as they are recognized.",
            Vec::new(),
        );
    }
    if flag("transcript-failed") {
        return (
            "Transcription failed",
            // ワーカーが返す中でいちばん長い理由（折り返しを見る）。
            "mic.mp3, system.mp3 could not be transcribed.",
            vec![("Try again", PaneActionKind::Transcribe, true)],
        );
    }
    (
        "No transcript yet",
        "Automatic transcription is off, so this recording was kept as audio only.",
        vec![("Transcribe now", PaneActionKind::Transcribe, true)],
    )
}

/// Notes タブの空表示（同上。`src/main.rs` の `SummaryPane::message` の複製）。
fn summary_empty_state(
    status: SummaryStatus,
    has_transcript: bool,
) -> (
    &'static str,
    &'static str,
    Vec<(&'static str, PaneActionKind, bool)>,
) {
    if !has_transcript {
        return (
            "No notes yet",
            "Notes are written from the transcript, and this recording has none. Transcribing it \
             first will let notes run.",
            vec![
                ("Transcribe now", PaneActionKind::Transcribe, true),
                (
                    "Open transcription",
                    PaneActionKind::OpenTranscription,
                    false,
                ),
            ],
        );
    }
    match status {
        SummaryStatus::Queued => (
            "Waiting to start — number 2 in the queue",
            "Notes start once the work ahead of this recording finishes. Nothing is running for \
             it yet, so it can still be canceled.",
            vec![("Cancel", PaneActionKind::CancelNotes, false)],
        ),
        SummaryStatus::Summarizing => (
            SUMMARIZING_LABEL,
            "Qwen2.5 3B Instruct is running on this Mac, started 40 seconds ago. Re-transcribing \
             is unavailable until this finishes, because it would change the input.",
            Vec::new(),
        ),
        SummaryStatus::Failed => (
            "Notes could not be written",
            // いちばん長い理由（2 つ並ぶボタンと一緒に収まるかを見る）。
            "The model could not finish. It may need more free memory than this Mac has right \
             now — closing other apps, or choosing a smaller model, can let it run.",
            vec![
                ("Try again", PaneActionKind::WriteNotes, true),
                ("Open Meeting notes", PaneActionKind::OpenNotes, false),
            ],
        ),
        SummaryStatus::NotSummarized | SummaryStatus::Done => (
            "No notes yet",
            "Notes are not written automatically, so this recording does not have any.",
            vec![("Write notes", PaneActionKind::WriteNotes, true)],
        ),
    }
}

/// Summary タブの確認用のダミー議事録。見出しの強調・本文の折り返し・段落の間隔を見たいので、
/// 実際の生成物（`src/summarize.rs` のプロンプトが作る 4 見出し構成）と同じ形にする。
fn sample_summary_rows() -> Vec<SummaryRow> {
    let row = |text: &str, is_heading: bool| SummaryRow {
        text: text.into(),
        is_heading,
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
    // 一覧のサンプル（見出しのまとまり・選択の縦罫・状態のドットを目視する）。**行の高さは
    // 固定**なので、いちばん長い文言でクリップされることも見る。文言は `src/main.rs` の
    // `session_detail_text` の複製（あちらを変えたらここも合わせること）。
    win.set_sessions(ModelRc::from(Rc::new(VecModel::from(vec![
        SessionRow {
            group_heading: "Today".into(),
            time_text: "14:02".into(),
            date_text: "Aug 10, 2026".into(),
            detail_text: "Mic + system · transcribing".into(),
            transcript_status: TranscriptStatus::Transcribing,
        },
        SessionRow {
            group_heading: "".into(),
            time_text: "09:30".into(),
            date_text: "Aug 10, 2026".into(),
            detail_text: "Mic only · transcribed".into(),
            transcript_status: TranscriptStatus::Done,
        },
        SessionRow {
            group_heading: "Yesterday".into(),
            time_text: "16:45".into(),
            date_text: "Aug 9, 2026".into(),
            detail_text: "System only · transcription failed".into(),
            transcript_status: TranscriptStatus::Failed,
        },
        SessionRow {
            group_heading: "".into(),
            time_text: "11:00".into(),
            date_text: "Aug 9, 2026".into(),
            detail_text: "Mic + system · not transcribed".into(),
            transcript_status: TranscriptStatus::NotTranscribed,
        },
        SessionRow {
            group_heading: "Aug 5, 2026".into(),
            time_text: "15:30".into(),
            date_text: "Aug 5, 2026".into(),
            detail_text: "Mic + system · transcribed".into(),
            transcript_status: TranscriptStatus::Done,
        },
    ]))));
    win.set_selected_index(0);
    win.set_library_summary("5 recordings".into());

    win.set_has_selection(true);
    win.set_detail_datetime("Aug 10, 2026 · 14:02".into());
    win.set_detail_sources("Mic + system".into());

    // 文字起こしと議事録は**実アプリで起こりうる組み合わせ**に揃える（要約は文字起こしを
    // 入力にするので、文字起こしが無いセッションには議事録も無い）。状態の文言は `src/main.rs` の
    // transcript_* / summary_* の対応表の複製（bin クレートなので import できない。あちらを
    // 変えたらここも合わせること）。
    // 読み込み中の表示（#152）。選んだ直後は中身が空で、その間もウィンドウは操作できる。
    if flag("loading") {
        win.set_loading(true);
    }
    let has_transcript = !flag("no-transcript");
    win.set_has_transcript(has_transcript);
    win.set_detail_transcript_status(if has_transcript {
        TranscriptStatus::Done
    } else {
        TranscriptStatus::NotTranscribed
    });
    win.set_detail_transcript_text(if has_transcript {
        "Transcribed".into()
    } else {
        "Not transcribed".into()
    });
    // 読む領域の空表示（#154）。**9 通りの状態を引数で選べる**ようにする——見出し・理由・
    // 操作の 3 段が最長文言で崩れないか、ボタンが 2 つ並んだときに収まるかを目視する。
    let (heading, body, actions) = transcript_empty_state();
    win.set_detail_transcript_heading(heading.into());
    win.set_detail_transcript_body(body.into());
    win.set_detail_transcript_actions(pane_actions(actions));
    if has_transcript {
        win.set_segments(ModelRc::from(Rc::new(VecModel::from(rows))));
        // 追従（#154）の確認: `far` は再生位置を一覧の末尾寄りに置く。ON なら開いた時点で
        // その行が見えているはず（`no-follow` なら先頭のまま）。
        win.set_current_segment(if flag("far") {
            i32::try_from(count.saturating_sub(3)).unwrap_or(0)
        } else {
            2
        });
    }

    // 議事録の状態を引数で選べるようにする（状態行の色・縮退ラベル・取り消しの確認用）。
    let summary_status = if !has_transcript {
        SummaryStatus::NotSummarized
    } else if flag("queued") {
        SummaryStatus::Queued
    } else if flag("summarizing") {
        SummaryStatus::Summarizing
    } else if flag("summary-failed") {
        SummaryStatus::Failed
    } else {
        SummaryStatus::Done
    };
    win.set_detail_summary_status(summary_status);
    win.set_detail_summary_status_text(
        match summary_status {
            SummaryStatus::NotSummarized => "Not summarized",
            SummaryStatus::Queued => SUMMARY_QUEUED_LABEL,
            SummaryStatus::Summarizing => SUMMARIZING_LABEL,
            SummaryStatus::Done => "Notes ready",
            SummaryStatus::Failed => "Summarization failed",
        }
        .into(),
    );
    let (heading, body, actions) = summary_empty_state(summary_status, has_transcript);
    win.set_detail_summary_heading(heading.into());
    win.set_detail_summary_body(body.into());
    win.set_detail_summary_actions(pane_actions(actions));
    win.set_detail_summary_footer("Written from the transcript · Aug 9, 2026 · 09:14".into());
    // 生成済みのときだけ行を入れる（生成中・失敗は旧議事録が無い状態＝縮退表示を見る）。
    if summary_status == SummaryStatus::Done {
        win.set_summary_rows(ModelRc::from(Rc::new(
            VecModel::from(sample_summary_rows()),
        )));
    }
    // タブは通常 UI が所有するが、Summary 側の見た目を確認できるよう引数で選べるようにする。
    win.set_showing_summary(
        flag("summary")
            || flag("queued")
            || flag("summarizing")
            || flag("summary-failed")
            || flag("no-transcript"),
    );
    // Playback セクション（シークバー）の確認用のダミー再生状態。
    win.set_playable(true);
    win.set_progress(0.35);
    win.set_time_text("01:45 / 05:00".into());
    win.set_seekable(!flag("no-seek"));
    win.set_follow_transcript(!flag("no-follow"));
    if flag("modal") {
        win.set_show_delete_confirm(true);
    }

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    win.window()
        .set_size(slint::LogicalSize::new(1100.0, 720.0));
    win.show()
        .expect("showing the window should succeed in this verification binary");

    // `snapshot <path>` が指定されていれば 1 フレーム後に PNG を書く（`snapshot` モジュール）。
    let _snapshot_timer = snapshot::arm(win.as_weak());

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}
