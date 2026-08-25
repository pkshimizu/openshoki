//! Recordings ウィンドウの Transcript / Summary タブと「Summarize」ボタンの操作テスト（#81）。
//!
//! 見た目は `examples/transcript_view.rs` で目視するが、**クリックが配線されているか**は
//! ビルドでも目視でも分からない（自作の `ViewTab` は `TouchArea` を重ねる構造で、レイアウトを
//! 変えると当たり判定だけ死ぬことがある）。`docs/rules/slint.md` の「操作は tests/ の
//! テストバックエンドで」に従い、実際のポインタイベントで検証する。
//!
//! Slint のデバッグ情報が要る（`build.rs` の `slint_debug_info`）。使い方は `docs/rules/slint.md`。

mod ui_support;

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::platform::PointerEventButton;

slint::include_modules!();

/// ウィンドウ寸法（実アプリと同じ。`src/main.rs` の RECORDINGS_WIDTH/HEIGHT）。要素の座標を
/// 出すために必要で、この値自体はアサートに使わない。
const WINDOW_WIDTH: f32 = 720.0;
const WINDOW_HEIGHT: f32 = 540.0;

/// 詳細ペイン（`if root.has-selection` の中）を出した状態のウィンドウ。
fn open_window() -> RecordingsWindow {
    ui_support::init_backend();
    let window = RecordingsWindow::new().expect("create the recordings window");
    window
        .window()
        .set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
    window.set_has_selection(true);
    window
}

/// 表示切替タブ（宣言順に Transcript → Summary）。
fn tabs(window: &RecordingsWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, "ViewTab").collect()
}

/// ラベルで詳細ペインのボタンを引く。対象は std-widgets の `Button` のみ（自作の
/// `DangerButton` は accessible-role/enabled を持たないので `accessible_enabled` が取れない）。
fn button(window: &RecordingsWindow, label: &str) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, label)
        .next()
        .unwrap_or_else(|| panic!("the detail pane should have a {label} button"))
}

/// 議事録のボタン。**文言は状態で変わる**（まだ無ければ `Write notes`、あれば
/// `Regenerate notes`）ので、どちらかを引く——押す動機が違うので語を変えている（#128）。
fn notes_button(window: &RecordingsWindow) -> ElementHandle {
    ["Regenerate notes", "Write notes"]
        .into_iter()
        .find_map(|label| ElementHandle::find_by_accessible_label(window, label).next())
        .expect("the detail pane should have a notes button")
}

/// 文字起こしのボタン（同じ理由で `Transcribe` / `Re-transcribe` のどちらか）。
fn transcribe_button(window: &RecordingsWindow) -> ElementHandle {
    ["Re-transcribe", "Transcribe"]
        .into_iter()
        .find_map(|label| ElementHandle::find_by_accessible_label(window, label).next())
        .expect("the detail pane should have a transcribe button")
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn clicking_the_tabs_switches_the_pane() {
    let window = open_window();
    let tabs = tabs(&window);
    assert_eq!(
        tabs.len(),
        2,
        "there should be a Transcript and a Summary tab"
    );

    // 既定は Transcript。
    assert!(!window.get_showing_summary());

    tabs[1].mock_single_click(PointerEventButton::Left);
    assert!(
        window.get_showing_summary(),
        "clicking the Summary tab should switch the pane"
    );

    tabs[0].mock_single_click(PointerEventButton::Left);
    assert!(
        !window.get_showing_summary(),
        "clicking the Transcript tab should switch back"
    );
}

/// Summarize は有効なときだけ、選択中インデックスを渡して発火する
/// （どの状態で押せるかは `the_summary_state_decides_which_actions_are_offered` が表で見る）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn summarize_reports_the_index_only_while_it_is_enabled() {
    let window = open_window();
    window.set_selected_index(2);
    window.set_has_transcript(true);

    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_summarize_session(move |index| recorded.borrow_mut().push(index));

    let summarize = notes_button(&window);
    assert_eq!(summarize.accessible_enabled(), Some(true));
    summarize.mock_single_click(PointerEventButton::Left);
    assert_eq!(*calls.borrow(), vec![2], "the selected index is passed");

    // 文字起こしが無いセッションでは無効（要約の入力が無い）。
    window.set_has_transcript(false);
    assert_eq!(summarize.accessible_enabled(), Some(false));
    summarize.mock_single_click(PointerEventButton::Left);
    assert_eq!(calls.borrow().len(), 1, "a disabled button must not fire");
}

/// 要約の状態ごとに、ボタン列の活性と取り消しの導線が意図どおりであること。
///
/// 状態が増えたときに**静かに「Summarize が押せる」へ落ちる**のを防ぐため、全バリアントを
/// 表で固定する（対応表は Slint 側の導出プロパティにあり、Rust の網羅 match で守れないため。
/// `docs/rules/slint.md` の「三項連鎖にしない」の代替）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn the_summary_state_decides_which_actions_are_offered() {
    // (状態, Summarize が押せるか, Cancel が出ているか, Delete が押せるか)
    let expected = [
        (SummaryStatus::NotSummarized, true, false, true),
        (SummaryStatus::Queued, false, true, true),
        (SummaryStatus::Summarizing, false, false, false),
        (SummaryStatus::Done, true, false, true),
        (SummaryStatus::Failed, true, false, true),
    ];

    let window = open_window();
    window.set_has_transcript(true);
    for (status, summarize_enabled, cancel_shown, delete_enabled) in expected {
        window.set_detail_summary_status(status);
        // 前の周回で開いた確認モーダルを閉じる（開いたままだと Delete の判定が常に真になる）。
        window.set_show_delete_confirm(false);

        assert_eq!(
            notes_button(&window).accessible_enabled(),
            Some(summarize_enabled),
            "the notes button for {status:?}"
        );
        assert_eq!(
            ElementHandle::find_by_accessible_label(&window, "Cancel notes").count(),
            usize::from(cancel_shown),
            "the Cancel button should only exist while queued ({status:?})"
        );

        // Delete は自作の `DangerButton` で accessible-enabled を出さないので、確認モーダルが
        // 開くかで見る。
        ElementHandle::find_by_element_type_name(&window, "DangerButton")
            .next()
            .expect("the detail pane should have a Delete button")
            .mock_single_click(PointerEventButton::Left);
        assert_eq!(
            window.get_show_delete_confirm(),
            delete_enabled,
            "Delete for {status:?}"
        );
    }
}

/// キュー待ちの取り消しは、状態行の隣に出る専用のボタンから行う
/// （Summarize と差し替えない理由は `ui/recordings-window.slint` の状態行のコメント）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn cancelling_a_queued_summary_reports_the_index() {
    let window = open_window();
    window.set_selected_index(1);
    window.set_has_transcript(true);

    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_cancel_summary(move |index| recorded.borrow_mut().push(index));
    // 取り消しのつもりで生成を投入してしまわないこと（取り違えが致命的な組み合わせ）。
    window.on_summarize_session(|_| panic!("a queued summary must not be re-submitted"));

    window.set_detail_summary_status(SummaryStatus::Queued);
    button(&window, "Cancel notes").mock_single_click(PointerEventButton::Left);
    assert_eq!(*calls.borrow(), vec![1], "the selected index is passed");
}

/// 要約の生成中は Transcribe も無効になる（`detail-jobs-pending`。ワーカーが対象ファイルを
/// 読み書きしている最中に多重投入をさせない）。Delete は別ゲート（`detail-files-in-use`）で、
/// 状態ごとの違いは `the_summary_state_decides_which_actions_are_offered` が見る。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn summarizing_also_disables_transcribe() {
    let window = open_window();
    let transcribe = transcribe_button(&window);
    assert_eq!(transcribe.accessible_enabled(), Some(true));

    window.set_detail_summary_status(SummaryStatus::Summarizing);
    assert_eq!(
        transcribe.accessible_enabled(),
        Some(false),
        "transcribing while summarizing would rewrite the input under the worker"
    );

    // 生成が終われば戻る。
    window.set_detail_summary_status(SummaryStatus::Done);
    assert_eq!(transcribe.accessible_enabled(), Some(true));
}

/// 走っている文字起こしを止める操作が配線されていること（#163）。
///
/// **押す場所は状態行**（読む領域の空表示にも同じ操作があるが、そちらはセグメントが 0 行の
/// ときしか出ない——やり直し中は一覧が出ているので、押す場所がここにしか無い）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn stopping_a_running_transcription_reports_the_index() {
    let window = open_window();
    window.set_selected_index(2);

    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_stop_transcription(move |index| recorded.borrow_mut().push(index));
    // 止めるつもりでやり直しを投入してしまわないこと（取り違えが致命的な組み合わせ）。
    window.on_transcribe_session(|_| panic!("stopping must not re-submit the transcription"));

    window.set_detail_transcript_status(TranscriptStatus::Transcribing);
    button(&window, "Stop transcription").mock_single_click(PointerEventButton::Left);
    assert_eq!(*calls.borrow(), vec![2], "the selected index is passed");
}

/// 止める操作は**走っている間だけ**出す（#163）。伝えた後（`Stopping`）に押せても何も
/// 変わらないので、押せる見た目のまま残さない。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn stop_is_offered_only_while_the_transcription_is_running() {
    let window = open_window();
    let stop_buttons = |window: &RecordingsWindow| {
        ElementHandle::find_by_accessible_label(window, "Stop transcription").count()
    };

    window.set_detail_transcript_status(TranscriptStatus::Transcribing);
    assert_eq!(
        stop_buttons(&window),
        1,
        "a running transcription can be stopped"
    );

    window.set_detail_transcript_status(TranscriptStatus::Stopping);
    assert_eq!(
        stop_buttons(&window),
        0,
        "pressing Stop again would not change anything"
    );

    for status in [
        TranscriptStatus::NotTranscribed,
        TranscriptStatus::Done,
        TranscriptStatus::Failed,
    ] {
        window.set_detail_transcript_status(status);
        assert_eq!(stop_buttons(&window), 0, "{status:?} has nothing to stop");
    }
}

/// 途中結果は、**開かれるまで伏せる**（#164）。先に一覧が出ると、欠けていることに気づかない
/// まま完成品として読んでしまう。
///
/// 逆に、完成した文字起こしが残っているだけの失敗（モデルのロードに失敗した再実行など）で
/// 伏せると、完成品を途中結果として隠すことになる。だから伏せる条件は状態 `Failed` ではなく
/// `detail-transcript-partial`（Rust の `TranscriptPane::shows_partial` が決める）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_partial_transcript_stays_behind_the_failure_until_it_is_asked_for() {
    let window = open_window();
    window.set_segments(slint::ModelRc::from(Rc::new(slint::VecModel::from(vec![
        TranscriptRow {
            speaker: "Mic".into(),
            is_mic: true,
            time: "00:00".into(),
            text: "hello".into(),
        },
    ]))));
    window.set_detail_transcript_status(TranscriptStatus::Failed);
    let lists = |window: &RecordingsWindow| {
        ElementHandle::find_by_element_type_name(window, "TranscriptList").count()
    };

    // 失敗しても、残っているのが完成品なら読める。
    window.set_detail_transcript_partial(false);
    assert_eq!(lists(&window), 1, "a complete transcript is not held back");

    // 途中結果は、押されるまで失敗の理由を出す。
    window.set_detail_transcript_partial(true);
    assert_eq!(
        lists(&window),
        0,
        "a partial transcript waits behind the failure"
    );

    window.set_show_partial_transcript(true);
    assert_eq!(lists(&window), 1, "asking for it shows what was kept");
}

/// 途中結果を開く操作が、空表示のボタンから配線されていること（#164）。**ラベルの一致で
/// 分岐させない**ので、押した結果として届くのは種別。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn asking_for_the_partial_transcript_is_wired_to_the_pane_action() {
    let window = open_window();
    window.set_detail_transcript_status(TranscriptStatus::Failed);
    window.set_detail_transcript_partial(true);
    window.set_detail_transcript_heading("Transcription failed".into());
    window.set_detail_transcript_actions(slint::ModelRc::from(Rc::new(slint::VecModel::from(
        vec![PaneAction {
            label: "Show partial".into(),
            kind: PaneActionKind::ShowPartialTranscript,
            primary: false,
        }],
    ))));

    let calls: Rc<RefCell<Vec<PaneActionKind>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_pane_action(move |kind| recorded.borrow_mut().push(kind));

    button(&window, "Show partial").mock_single_click(PointerEventButton::Left);
    assert_eq!(*calls.borrow(), vec![PaneActionKind::ShowPartialTranscript]);
}
