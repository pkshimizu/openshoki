//! モデル一覧の操作（選択・取得・削除）のテスト（#117 / #138。#141 で機能ウィンドウへ移設）。
//!
//! 一覧は文字起こし・議事録の 2 ウィンドウが**同じ部品**（`ModelList`）で持つので、代表として
//! 文字起こし側で検証する（行の見た目と操作はどちらも同じ部品が出す）。
//!
//! 見た目は `examples/models_view.rs` で目視するが、**クリックが配線されているか**は
//! ビルドでも目視でも分からない（Delete は自作の `DangerButton`＝`TouchArea` を重ねる構造で、
//! `enabled` を無視して発火しても静かに壊れる）。`docs/rules/slint.md` の「操作は tests/ の
//! テストバックエンドで」に従い、実際のポインタイベントで検証する。
//!
//! ここで固定するのは**操作の契約**: どの状態でどのボタンが出るか、押したときに**正しい行の**
//! インデックスが 1 回だけ渡ること、削除は確認モーダルを経ること、Cancel では発火しないこと。
//! 100ms tick に依存する挙動（進捗の追従）はテストバックエンドでは動かないので扱わない。
//!
//! Slint のデバッグ情報が要る（`build.rs` の `slint_debug_info`）。使い方は `docs/rules/slint.md`。

mod ui_support;

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::platform::PointerEventButton;

slint::include_modules!();

/// ウィンドウ寸法（実アプリと同じ。`src/main.rs` の TRANSCRIPTION_WIDTH/HEIGHT）。要素の座標を出すために
/// 必要で、この値自体はアサートに使わない。
const WINDOW_WIDTH: f32 = 620.0;
const WINDOW_HEIGHT: f32 = 780.0;

/// テスト用の行の指定（表示の中身ではなく、**操作の可否を決める軸**だけを置く）。
#[derive(Clone, Copy)]
struct Row {
    status: ModelStatus,
    can_use: bool,
    can_download: bool,
    can_delete: bool,
}

impl Row {
    /// 取得済みで、選べて消せる普通の行。
    fn installed() -> Self {
        Self {
            status: ModelStatus::Installed,
            can_use: true,
            can_download: false,
            can_delete: true,
        }
    }

    /// 状態から素直に導ける可否を持つ行（`main` 側の純粋関数と同じ写像）。
    fn with_status(status: ModelStatus) -> Self {
        Self {
            status,
            can_use: true,
            can_download: matches!(status, ModelStatus::NotDownloaded | ModelStatus::Failed),
            can_delete: status == ModelStatus::Installed,
        }
    }
}

/// 取得済み・未取得・取得中の 3 行を並べたウィンドウ。
fn open_window() -> TranscriptionWindow {
    open_window_with(&[
        Row::installed(),
        Row::with_status(ModelStatus::NotDownloaded),
        Row::with_status(ModelStatus::Downloading),
    ])
}

fn open_window_with(rows: &[Row]) -> TranscriptionWindow {
    ui_support::init_backend();
    let window = TranscriptionWindow::new().expect("create the models window");
    window
        .window()
        .set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
    set_rows(&window, rows);
    window
}

fn set_rows(window: &TranscriptionWindow, rows: &[Row]) {
    let rows: Vec<ModelRow> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| ModelRow {
            is_heading: false,
            name: format!("Model {i}").into(),
            detail: "balanced speed and accuracy".into(),
            size: "1.5 GB".into(),
            status_text: "status".into(),
            tone: StatusTone::Neutral,
            progress: -1.0,
            progress_detail: "".into(),
            badge: "".into(),
            mono: false,
            delete_detail: "detail".into(),
            status: row.status,
            can_use: row.can_use,
            can_download: row.can_download,
            can_delete: row.can_delete,
        })
        .collect();
    window.set_models(Rc::new(slint::VecModel::from(rows)).into());
}

/// 行の Delete ボタン（宣言順＝行の順）。自作の `DangerButton` は accessible-label を持たないので
/// 型名で引く。確認モーダルの確定ボタンだけはラベル（`Delete Model`）を持たせてあるので、
/// 位置ではなくラベルで引ける（下の `confirm_button`）。
fn row_delete_buttons(window: &TranscriptionWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, "DangerButton")
        .filter(|handle| handle.accessible_label().as_deref() != Some(CONFIRM_LABEL))
        .collect()
}

/// 確認モーダルの確定ボタン（ラベルで引く。行の Delete と取り違えないため）。
const CONFIRM_LABEL: &str = "Delete Model";

fn confirm_button(window: &TranscriptionWindow) -> ElementHandle {
    ElementHandle::find_by_accessible_label(window, CONFIRM_LABEL)
        .next()
        .expect("the confirmation should have a Delete Model button")
}

/// 状態ごとに、行に出るはずのボタン（**網羅 match**。Slint 側は `can-download` と
/// `status == installed` で出し分けているので、状態を足したときにここがコンパイルエラーになって
/// 更新漏れに気づけるようにする）。`Row::with_status` が渡す可否と対で読む。
fn expected_buttons(
    status: ModelStatus,
) -> (
    Option<&'static str>, /* 取得 */
    bool,                 /* Delete */
) {
    match status {
        // ディスクに無い＝取得できる／消すものが無い。
        ModelStatus::NotDownloaded => (Some("Download"), false),
        // 失敗からの再取得は**同じ操作だが押す動機が違う**ので、文言を変える（#141 のデザイン）。
        ModelStatus::Failed => (Some("Retry"), false),
        // 受信中は一時ファイルなので、取得も削除も出さない。
        ModelStatus::Downloading => (None, false),
        // 在るものは消せる（押せるかは can-delete）。
        ModelStatus::Installed => (None, true),
    }
}

/// ラベルでボタンを引く（`Use` / `Download` / `Retry`。行の順に返る）。
fn buttons_labelled(window: &TranscriptionWindow, label: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_accessible_label(window, label).collect()
}

fn click(button: &ElementHandle) {
    button.mock_single_click(PointerEventButton::Left);
}

/// 削除は確認モーダルを経る（1 クリックでは消さない）。確定すると、**押した行の**インデックスが
/// 1 回だけ渡る。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn deleting_a_model_goes_through_the_confirmation() {
    let window = open_window_with(&[Row::installed(), Row::installed()]);
    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_delete_model(move |index| recorded.borrow_mut().push(index));

    let buttons = row_delete_buttons(&window);
    assert_eq!(buttons.len(), 2, "each installed row has a Delete button");
    click(&buttons[0]);
    assert!(
        window.get_show_delete_confirm(),
        "the first click should only open the confirmation"
    );
    assert!(
        calls.borrow().is_empty(),
        "nothing should be deleted before the confirmation"
    );

    click(&confirm_button(&window));
    assert_eq!(*calls.borrow(), vec![0], "the pressed row is passed");
    assert!(
        !window.get_show_delete_confirm(),
        "confirming closes the modal"
    );
}

/// 2 行目を押したら 2 行目が渡る（行とインデックスの対応がずれていないこと）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn the_confirmation_targets_the_row_that_was_pressed() {
    let window = open_window_with(&[Row::installed(), Row::installed()]);
    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_delete_model(move |index| recorded.borrow_mut().push(index));

    click(&row_delete_buttons(&window)[1]);
    assert_eq!(window.get_delete_index(), 1);
    click(&confirm_button(&window));
    assert_eq!(*calls.borrow(), vec![1]);
}

/// Cancel は何も削除せずにモーダルを閉じる。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn cancelling_the_confirmation_deletes_nothing() {
    let window = open_window();
    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_delete_model(move |index| recorded.borrow_mut().push(index));

    click(&row_delete_buttons(&window)[0]);
    ElementHandle::find_by_accessible_label(&window, "Cancel")
        .next()
        .expect("the confirmation should have a Cancel button")
        .mock_single_click(PointerEventButton::Left);
    assert!(!window.get_show_delete_confirm(), "Cancel closes the modal");
    assert!(calls.borrow().is_empty(), "Cancel must not delete anything");
}

/// 状態ごとに、出るボタンが意図どおりであること（全バリアント）。Slint 側は状態の比較で
/// 出し分けているので、状態を足したときに静かに「取得も削除もできない行」へ落ちないよう固定する。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn the_status_decides_which_buttons_appear() {
    let window = open_window();
    for status in [
        ModelStatus::NotDownloaded,
        ModelStatus::Downloading,
        ModelStatus::Installed,
        ModelStatus::Failed,
    ] {
        set_rows(&window, &[Row::with_status(status)]);
        window.set_show_delete_confirm(false);
        let (fetch, delete) = expected_buttons(status);
        for label in ["Download", "Retry"] {
            assert_eq!(
                buttons_labelled(&window, label).len(),
                usize::from(fetch == Some(label)),
                "{label} for {status:?}"
            );
        }
        assert_eq!(
            row_delete_buttons(&window).len(),
            usize::from(delete),
            "Delete for {status:?}"
        );
    }
}

/// `can-download` が false の行では Download が出ない（`config.toml` がその種別のモデルパスを
/// 上書きしているとき。落としても使われないので出さない）。状態は「未取得」のままなので、
/// 状態からの導出に戻すとここが落ちる。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_row_that_cannot_be_downloaded_hides_the_button() {
    for status in [ModelStatus::NotDownloaded, ModelStatus::Failed] {
        let window = open_window_with(&[Row {
            status,
            can_use: false,
            can_download: false,
            can_delete: false,
        }]);
        assert!(
            buttons_labelled(&window, "Download").is_empty(),
            "Download must follow can-download, not the status ({status:?})"
        );
    }
}

/// 使用中のジョブがある行では Delete が押せない（`can-delete` が false）。ボタン自体は出す
/// （消せない理由は状態テキストに出るので、消えると理由が分からなくなる）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_busy_row_shows_delete_but_does_not_open_the_confirmation() {
    let window = open_window_with(&[Row {
        status: ModelStatus::Installed,
        can_use: false,
        can_download: false,
        can_delete: false,
    }]);
    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_delete_model(move |index| recorded.borrow_mut().push(index));

    let buttons = row_delete_buttons(&window);
    assert_eq!(
        buttons.len(),
        1,
        "the button stays so the reason is visible"
    );
    click(&buttons[0]);
    assert!(
        !window.get_show_delete_confirm(),
        "a row that cannot be deleted must not open the confirmation"
    );
    assert!(calls.borrow().is_empty());
}

/// 「Use」は `can-use` の行にだけ出て、押すとその行のインデックスが 1 回渡る。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn use_fires_for_the_row_that_can_be_selected() {
    let window = open_window_with(&[
        // 1 行目は選択中（`can-use` が false）、2 行目が選べる行。
        Row {
            status: ModelStatus::Installed,
            can_use: false,
            can_download: false,
            can_delete: true,
        },
        Row::installed(),
    ]);
    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_use_model(move |index| recorded.borrow_mut().push(index));

    let use_buttons = buttons_labelled(&window, "Use");
    assert_eq!(use_buttons.len(), 1, "only the selectable row offers Use");
    click(&use_buttons[0]);
    assert_eq!(*calls.borrow(), vec![1], "the second row is passed");
}

/// 「Download」は未取得・失敗の行に出て、押すとその行のインデックスが 1 回渡る。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn download_fires_for_a_row_without_a_file() {
    let window = open_window_with(&[
        Row::installed(),
        Row::with_status(ModelStatus::NotDownloaded),
    ]);
    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_download_model(move |index| recorded.borrow_mut().push(index));

    let download = buttons_labelled(&window, "Download");
    assert_eq!(download.len(), 1, "only the row without a file offers it");
    click(&download[0]);
    assert_eq!(*calls.borrow(), vec![1]);
}

/// 見出しの行は操作を持たない（ボタンが出ない）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_heading_row_has_no_buttons() {
    let window = open_window();
    let heading = ModelRow {
        is_heading: true,
        name: "Transcription — Whisper".into(),
        ..ModelRow::default()
    };
    window.set_models(Rc::new(slint::VecModel::from(vec![heading])).into());

    assert!(row_delete_buttons(&window).is_empty());
    assert!(buttons_labelled(&window, "Use").is_empty());
    assert!(buttons_labelled(&window, "Download").is_empty());
}
