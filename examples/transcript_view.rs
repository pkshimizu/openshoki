//! Library ウィンドウ（Transcript / Summary・Playback セクション）の描画確認用バイナリ
//! （`docs/rules/slint.md` の検証手順）。ダミーの状態を流し込んで LibraryWindow を表示する。
//! 実行: `cargo run --example transcript_view [引数...]` → screencapture で目視確認。
//!
//! 引数（順不同・組み合わせ可）:
//! - 数値: セグメント件数（0 で Transcript の空表示。既定は `DEFAULT_SEGMENT_COUNT`）
//! - `modal`: 削除確認モーダルを重ねた状態
//! - `no-seek`: シークバーを表示専用へ縮退させた状態（再生不可・全体長不明のセッション相当）
//! - `summary`: Summary タブを開いた状態（議事録の見出し強調・折り返しの確認）
//! - `queued` / `summarizing` / `summary-failed`: Summary タブを開き、キュー待ち／生成中／
//!   失敗の状態にした状態（状態行の色・空表示の 3 段・状態行の隣に出る取り消しの確認）
//! - `no-transcript`: 文字起こしも議事録も無いセッション（両タブの空表示・Summarize の無効化）。
//!   要約は入力が無いので、上の要約状態の指定より優先される
//! - `transcribing` / `stopping` / `transcript-failed` / `transcript-unreadable`:
//!   Transcript タブの空表示を、実行中／停止中／失敗／JSON が読めなかった状態にする
//!   （見出し・理由・操作の 3 段と、最長の理由の折り返しの確認。件数 0 と組み合わせる）
//! - `not-read-to-the-end`: 走り終わっているが音源を最後まで読めていない状態（#175。ディスクの
//!   印から分かるので再起動しても消えない）。**件数と組み合わせる**——セグメントが在っても
//!   `Show partial` を押すまで空表示が出る。`summary` と重ねると Notes タブの言い分も見られる
//! - `transcript-partial`: 途中まで読めて失敗した状態（#164）。**件数と組み合わせる**——
//!   セグメントが在っても `Show partial` を押すまで空表示が出るところを見る
//! - `show-partial`: その途中結果を開いた状態（一覧に切り替わるところを見る）
//!   Notes タブの入力待ち・入力の失敗（#165）は、`no-transcript` に `transcribing` /
//!   `transcript-failed` を重ねて作る（Transcript タブと同じ値から出るので、別のフラグは無い）
//! - `auto-on`: 未実施の理由を「自動は ON だがまだ回っていない」にする（両タブ）
//! - `no-follow`: 再生位置の追従を OFF にした状態（プレイヤー帯のスイッチの確認）
//! - `far`: 再生位置を一覧の末尾寄りに置く（追従でその行が見えているかの確認。`no-follow`
//!   と組み合わせると先頭のままになる）。**スナップショットは表示後の更新を反映しない**ので、
//!   追従はここで見える初期表示ぶんだけを確認できる
//! - `search` / `no-match`: 一覧を絞り込み中／0 件にする（検索欄・件数・解除の導線の確認）
//! - `not-downloaded`: 退避されていて検索できなかった録音がある状態（#182。件数の行と
//!   0 件の説明に理由が出る。`search` / `no-match` と組み合わせる）
//! - `snapshot <path>`: PNG に書き出す（画面収録の許可が無い環境用）

slint::include_modules!();

#[path = "verification/snapshot.rs"]
mod snapshot;

// **文言は複製せず、本番と同じものを使う**（#160）。複製していたときは実際にずれた
// （#161 で `Waiting to summarize…` と `Waiting to write notes…` に割れているのが見つかった）。
// 目視で確認するのが出荷される文言でなくなると、確認そのものが意味を失う。
// 確認用バイナリは一部の変種しか作らないので、作らないものは「未使用」に見える。
// **本番では全部使う**（`TranscriptPane::message` の網羅 match）ので、ここでは許可する。
#[allow(dead_code)]
#[path = "../src/reading_pane.rs"]
mod reading_pane;

// 一覧の下端と空表示の文も同じ理由で共有する（#182）。ここを複製していたせいで、空表示に
// 「読めなかった録音がある」を足したときに確認用バイナリだけ真っ白になった。
#[allow(dead_code)]
#[path = "../src/library_text.rs"]
mod library_text;

use std::rc::Rc;
use std::time::Duration;

use slint::{ModelRc, VecModel};

/// 引数に含まれるフラグか。
fn flag(name: &str) -> bool {
    std::env::args().any(|arg| arg == name)
}

/// 引数で件数を指定しなかったときのセグメント件数。
const DEFAULT_SEGMENT_COUNT: usize = 30;

/// Transcript タブの空表示を入れる（本番の `main::apply_detail_transcript_status` と同じ
/// 3 つを埋める）。
///
/// **タブごとに関数を分ける**——見出しと理由の setter を引数で受けると、同じ型なので取り違えても
/// 通ってしまう（`docs/rules/coding-conventions.md`）。
fn apply_transcript_pane(win: &LibraryWindow, pane: &reading_pane::TranscriptPane) {
    // **状態行も同じ値から出す**（#175）。`transcript_status_text` を直に呼ぶと、状態 enum が
    // 持てない「最後まで読めていない」を落として、本番では作れない組み合わせを目視することに
    // なる（本番は `main::apply_detail_transcript_status`）。
    win.set_detail_transcript_status(pane.status());
    win.set_detail_transcript_text(pane.status_text().into());
    let message = pane.message();
    win.set_detail_transcript_heading(message.heading.as_str().into());
    win.set_detail_transcript_body(message.body.as_str().into());
    win.set_detail_transcript_actions(ModelRc::from(Rc::new(VecModel::from(message.actions))));
    // 途中結果を伏せるかどうかも**同じ値から**出す（#164。本番の
    // `main::apply_detail_transcript_status` と同じ）。別々に選ぶと、本番では作れない
    // 組み合わせを目視することになる（`docs/rules/testing.md`）。
    win.set_detail_transcript_partial(pane.shows_partial());
}

/// Notes タブの空表示を入れる（`apply_transcript_pane` と同じ理由で分けてある）。
fn apply_summary_pane(win: &LibraryWindow, message: &reading_pane::PaneMessage) {
    win.set_detail_summary_heading(message.heading.as_str().into());
    win.set_detail_summary_body(message.body.as_str().into());
    win.set_detail_summary_actions(ModelRc::from(Rc::new(VecModel::from(
        message.actions.clone(),
    ))));
}

/// Transcript タブに出す状態を引数で選ぶ。**状態そのものを返す**——状態行と空表示を別々に
/// 選ぶと、本番では作れない組み合わせ（「Transcribed」なのに空表示は「Transcribing」）が
/// 出せてしまう。文言は `reading_pane` が組む（#160）。
fn transcript_pane(has_transcript: bool) -> reading_pane::TranscriptPane {
    if flag("transcribing") {
        return reading_pane::TranscriptPane::Transcribing {
            model: "Medium".to_owned(),
            percent: Some(48),
        };
    }
    if flag("stopping") {
        return reading_pane::TranscriptPane::Stopping {
            model: "Medium".to_owned(),
        };
    }
    if flag("not-read-to-the-end") {
        // 走り終わっているが、音源を最後まで読めていない（#175。ディスクの印から分かるので
        // 再起動しても消えない）。
        return reading_pane::TranscriptPane::NotReadToTheEnd;
    }
    if flag("transcript-failed") {
        // ワーカーが返す中でいちばん長い理由（折り返しを見る）。何も残っていないので
        // `Show partial` は出ない。
        return reading_pane::TranscriptPane::Failed {
            reason: reading_pane::TranscribeFailure::Files {
                failed: vec![
                    reading_pane::FailedSource::new("mic.mp3", None),
                    reading_pane::FailedSource::new("system.mp3", None),
                ],
                kept_other_sources: false,
            },
        };
    }
    if flag("transcript-partial") {
        // 途中まで読めて失敗した状態（#164）。セグメントが在っても、開かれるまでは
        // この空表示が出る。
        return reading_pane::TranscriptPane::Failed {
            reason: reading_pane::TranscribeFailure::Files {
                failed: vec![reading_pane::FailedSource::new(
                    "mic.mp3",
                    Some(Duration::from_secs(252)),
                )],
                kept_other_sources: false,
            },
        };
    }
    // `transcript-unreadable`（生成済みなのに中身が読めない）も、状態としては生成済み。
    // 違いは行が 0 件になることで、それは呼び出し側が `set_segments` を省いて作る。
    if has_transcript {
        return reading_pane::TranscriptPane::Done;
    }
    reading_pane::TranscriptPane::NotTranscribed {
        auto_on: flag("auto-on"),
    }
}

/// Notes タブに出す状態（同上）。
///
/// **入力（文字起こし）の様子は、Transcript タブと同じ値から出す**（#165）。フラグで直接
/// 選ぶと、本番では作れない組み合わせ（「文字起こし失敗」なのに Notes は「まだ書けない」）を
/// 目視することになる（`docs/rules/testing.md`）。
fn summary_pane(
    status: SummaryStatus,
    transcript: &reading_pane::TranscriptPane,
    has_transcript: bool,
) -> reading_pane::SummaryPane {
    // ディスクの様子も**同じ値から**出す（#175。本番は `main::LoadedTranscript::stored`）。
    let stored = match (has_transcript, transcript) {
        (false, _) => reading_pane::StoredTranscript::None,
        (true, reading_pane::TranscriptPane::NotReadToTheEnd) => {
            reading_pane::StoredTranscript::NotReadToTheEnd
        }
        (true, _) => reading_pane::StoredTranscript::Complete,
    };
    let input = reading_pane::TranscriptInput::of(transcript, stored);
    if input != reading_pane::TranscriptInput::Ready {
        return input.pane_when_no_notes(flag("auto-on"));
    }
    match status {
        SummaryStatus::Queued => reading_pane::SummaryPane::Queued { position: 2 },
        SummaryStatus::Summarizing => reading_pane::SummaryPane::Summarizing {
            model: "Qwen2.5 3B Instruct".to_owned(),
            started_ago: "40 seconds".to_owned(),
        },
        // いちばん長い理由（2 つ並ぶボタンと一緒に収まるかを見る）。
        SummaryStatus::Failed => reading_pane::SummaryPane::Failed {
            reason: reading_pane::SummarizeFailure::ModelRun,
        },
        SummaryStatus::Done => reading_pane::SummaryPane::Done,
        SummaryStatus::NotSummarized => reading_pane::SummaryPane::NotSummarized {
            auto_on: flag("auto-on"),
        },
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
    let win = LibraryWindow::new()
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
    // 固定**なので、いちばん長い文言でクリップされることも見る。状態の語は本番と同じ
    // `reading_pane::session_transcript_word` が組む（#160）。
    let detail = |sources: &str, status, percent| -> slint::SharedString {
        format!(
            "{sources} · {}",
            reading_pane::session_transcript_word(status, percent)
        )
        .into()
    };
    win.set_sessions(ModelRc::from(Rc::new(VecModel::from(vec![
        SessionRow {
            group_heading: "Today".into(),
            time_text: "14:02".into(),
            date_text: "Aug 10, 2026 · 1:12:40".into(),
            detail_text: detail("Mic + system", TranscriptStatus::Transcribing, Some(48)),
            transcript_status: TranscriptStatus::Transcribing,
        },
        SessionRow {
            group_heading: "".into(),
            time_text: "09:30".into(),
            date_text: "Aug 10, 2026 · 27:05".into(),
            detail_text: detail("Mic only", TranscriptStatus::Done, None),
            transcript_status: TranscriptStatus::Done,
        },
        SessionRow {
            group_heading: "Yesterday".into(),
            time_text: "16:45".into(),
            date_text: "Aug 9, 2026 · 2:41:18".into(),
            detail_text: detail("System only", TranscriptStatus::Failed, None),
            transcript_status: TranscriptStatus::Failed,
        },
        SessionRow {
            group_heading: "".into(),
            time_text: "11:00".into(),
            // 長さが分からない録音（区切りごと出ないことを見る）。
            date_text: "Aug 9, 2026".into(),
            detail_text: detail("Mic + system", TranscriptStatus::NotTranscribed, None),
            transcript_status: TranscriptStatus::NotTranscribed,
        },
        SessionRow {
            group_heading: "Aug 5, 2026".into(),
            time_text: "15:30".into(),
            // デザインの `6:20` に対して、プレイヤーへ揃えたゼロ詰めの形も見る。
            date_text: "Aug 5, 2026 · 06:20".into(),
            detail_text: detail("Mic + system", TranscriptStatus::Done, None),
            transcript_status: TranscriptStatus::Done,
        },
    ]))));
    win.set_selected_index(0);
    win.set_library_summary(library_text::library_summary(5).into());
    // 検索（#161）。`search` は絞り込み中、`no-match` は 0 件（解除の導線を見る）。
    // **件数の文も空表示の文も本番と同じ関数で組む**（#182。ここを複製すると、目視の対象が
    // 出荷される文言でなくなる）。
    let searching = flag("search") || flag("no-match");
    let not_downloaded = if flag("not-downloaded") { 2 } else { 0 };
    let matched = if flag("no-match") { 0 } else { 3 };
    if searching {
        win.set_search_text("recording format".into());
        win.set_search_summary(
            library_text::search_summary_text(matched, 5, not_downloaded).into(),
        );
    }
    let (empty_heading, empty_body) = library_text::empty_list_message(searching, not_downloaded);
    win.set_empty_heading(empty_heading.into());
    win.set_empty_body(empty_body.into());

    win.set_has_selection(true);
    win.set_detail_datetime("Aug 10, 2026 · 14:02".into());
    win.set_detail_sources("Mic + system".into());

    // 文字起こしと議事録は**実アプリで起こりうる組み合わせ**に揃える（要約は文字起こしを
    // 入力にするので、文字起こしが無いセッションには議事録も無い）。状態の文言は本番と同じ
    // `reading_pane` が組む（#160）。
    // 読み込み中の表示（#152）。選んだ直後は中身が空で、その間もウィンドウは操作できる。
    if flag("loading") {
        win.set_loading(true);
    }
    // 0 件の一覧を見るための縮退（`no-match` のときだけ）。
    if flag("no-match") {
        win.set_sessions(ModelRc::from(Rc::new(VecModel::<SessionRow>::default())));
        win.set_has_selection(false);
    }
    let has_transcript = !flag("no-transcript");
    win.set_has_transcript(has_transcript);
    win.set_show_partial_transcript(flag("show-partial"));
    let transcript_pane = transcript_pane(has_transcript);
    // 読む領域の空表示（#154）。**状態を引数で選べる**ようにする——見出し・理由・操作の 3 段が
    // 最長文言で崩れないか、ボタンが 2 つ並んだときに収まるかを目視する。文言は本番と同じ
    // `reading_pane` が組む（#160）。
    apply_transcript_pane(&win, &transcript_pane);
    // `transcript-unreadable` は行を入れない——「生成済みなのに読めない」ときの空表示を見る。
    if has_transcript && !flag("transcript-unreadable") {
        win.set_segments(ModelRc::from(Rc::new(VecModel::from(rows))));
        // 追従（#154）の確認: `far` は再生位置を一覧の末尾寄りに置く。ON なら開いた時点で
        // その行が見えているはず（`no-follow` なら先頭のまま）。
        win.set_current_segment(if flag("far") {
            i32::try_from(count.saturating_sub(3)).unwrap_or(0)
        } else {
            2
        });
    }

    // 議事録の状態を引数で選べるようにする（状態行の色・空表示・取り消しの確認用）。
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
    let summary_pane = summary_pane(summary_status, &transcript_pane, has_transcript);
    win.set_detail_summary_status(summary_pane.status());
    win.set_detail_summary_status_text(
        reading_pane::summary_status_text(summary_pane.status()).into(),
    );
    apply_summary_pane(&win, &summary_pane.message());
    win.set_detail_summary_footer("Written from the transcript · Aug 9, 2026 · 09:14".into());
    // 生成済みのときだけ行を入れる（生成中・失敗は旧議事録が無い状態＝空表示を見る）。
    // **行も空表示も同じ値から出す**（#175）。別に選んだ状態で行を入れると、「未生成なのに
    // 議事録が出ている」という本番では作れない画面になる（`docs/rules/testing.md`）。
    if summary_pane.status() == SummaryStatus::Done {
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
