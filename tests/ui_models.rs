//! モデル一覧ウィンドウの削除操作のテスト（#117）。
//!
//! 見た目は `examples/models_view.rs` で目視するが、**クリックが配線されているか**は
//! ビルドでも目視でも分からない（Delete は自作の `DangerButton`＝`TouchArea` を重ねる構造で、
//! `enabled` を無視して発火しても静かに壊れる）。`docs/rules/slint.md` の「操作は tests/ の
//! テストバックエンドで」に従い、実際のポインタイベントで検証する。
//!
//! ここで固定するのは**削除の契約**だけ: 確認モーダルを経ること、確定で正しい行が渡ること、
//! Cancel では発火しないこと、削除できない状態では発火しないこと。100ms tick に依存する挙動
//! （一覧の作り直し）はテストバックエンドでは動かないので扱わない。
//!
//! Slint のデバッグ情報が要る（`build.rs` の `slint_debug_info`）。使い方は `docs/rules/slint.md`。

mod ui_support;

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::platform::PointerEventButton;

slint::include_modules!();

/// ウィンドウ寸法（実アプリと同じ。`src/main.rs` の MODELS_WIDTH/HEIGHT）。要素の座標を出すために
/// 必要で、この値自体はアサートに使わない。
const WINDOW_WIDTH: f32 = 460.0;
const WINDOW_HEIGHT: f32 = 420.0;

/// 行を 3 つ（取得済み・使用中・取得中）並べたウィンドウ。文言は Rust 側が組むので、ここでは
/// 表示の中身ではなく**状態**を意図どおりに置くことだけを目的にする。
fn open_window() -> ModelsWindow {
    ui_support::init_backend();
    let window = ModelsWindow::new().expect("create the models window");
    window
        .window()
        .set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
    set_rows(
        &window,
        &[
            ("Small", ModelRowState::Installed),
            ("Medium", ModelRowState::InUse),
            ("Large", ModelRowState::Downloading),
        ],
    );
    window
}

fn set_rows(window: &ModelsWindow, rows: &[(&str, ModelRowState)]) {
    let rows: Vec<ModelRow> = rows
        .iter()
        .map(|(name, state)| ModelRow {
            name: (*name).into(),
            detail: "Whisper speech · model.bin".into(),
            size: "1.5 GB".into(),
            state_text: "state".into(),
            delete_detail: "detail".into(),
            state: *state,
        })
        .collect();
    window.set_models(Rc::new(slint::VecModel::from(rows)).into());
}

/// 行の Delete ボタン（宣言順＝行の順）。自作の `DangerButton` は accessible-label を持たないので
/// 型名で引く。確認モーダルの確定ボタンも `DangerButton` なので、モーダルを閉じた状態で数える。
fn row_delete_buttons(window: &ModelsWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, "DangerButton").collect()
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
    let window = open_window();
    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_delete_model(move |index| recorded.borrow_mut().push(index));

    let buttons = row_delete_buttons(&window);
    assert_eq!(buttons.len(), 3, "every row should have a Delete button");
    click(&buttons[0]);
    assert!(
        window.get_show_delete_confirm(),
        "the first click should only open the confirmation"
    );
    assert!(
        calls.borrow().is_empty(),
        "nothing should be deleted before the confirmation"
    );

    // モーダルが開いている間は確定ボタンも `DangerButton` なので、引き直して末尾を押す。
    let confirm = row_delete_buttons(&window);
    assert_eq!(
        confirm.len(),
        4,
        "the confirmation adds one more DangerButton"
    );
    click(&confirm[3]);
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
    let window = open_window();
    set_rows(
        &window,
        &[
            ("Small", ModelRowState::Installed),
            ("Medium", ModelRowState::Selected),
        ],
    );
    let calls: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let recorded = Rc::clone(&calls);
    window.on_delete_model(move |index| recorded.borrow_mut().push(index));

    click(&row_delete_buttons(&window)[1]);
    assert_eq!(window.get_delete_index(), 1);
    let confirm = row_delete_buttons(&window);
    click(&confirm[confirm.len() - 1]);
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

/// 削除できない状態（使用中・取得中）の Delete は押しても何も起きない。無効化は状態 enum から
/// 導出しているので、状態を足したときにここが「押せる」へ静かに落ちないよう全バリアントを見る。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn the_row_state_decides_whether_delete_can_be_pressed() {
    // (状態, 確認モーダルが開くか)
    let expected = [
        (ModelRowState::Installed, true),
        (ModelRowState::Selected, true),
        (ModelRowState::InUse, false),
        (ModelRowState::Downloading, false),
        (ModelRowState::Unknown, true),
    ];

    let window = open_window();
    for (state, opens) in expected {
        set_rows(&window, &[("Model", state)]);
        window.set_show_delete_confirm(false);
        click(&row_delete_buttons(&window)[0]);
        assert_eq!(
            window.get_show_delete_confirm(),
            opens,
            "Delete for {state:?}"
        );
    }
}
