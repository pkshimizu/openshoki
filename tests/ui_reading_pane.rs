//! 読む領域の空表示（#154）と、再生位置の追従スイッチの操作を、実際のポインタイベントで
//! 検証する。
//!
//! 見た目（3 段の並び・折り返し）の確認は `examples/transcript_view.rs` ＋ screencapture
//! （`docs/rules/slint.md`）。こちらは「押したら何が起きるか」を担当する。
//!
//! 要素を探す `ElementHandle` は生成コードのデバッグ情報を要求する。有無で各テストを
//! ignore へ切り替える（条件と有効化方法は `docs/rules/slint.md`）。

slint::include_modules!();

mod ui_support;

use std::cell::RefCell;
use std::rc::Rc;

use i_slint_backend_testing::ElementHandle;
use slint::{ComponentHandle, ModelRc, VecModel};

/// テスト用のウィンドウサイズ。空表示のボタンまで含めて収まる大きさにする（`ScrollView` の
/// 外にある要素は探索で見つからない。`docs/rules/slint.md`）。
const WINDOW_WIDTH: f32 = 1100.0;
const WINDOW_HEIGHT: f32 = 720.0;

/// 選択済み・中身なしの Library ウィンドウ（空表示が出ている状態）。
fn window_with_empty_pane(actions: Vec<PaneAction>) -> LibraryWindow {
    ui_support::init_backend();
    let window = LibraryWindow::new().expect("creating the window should succeed");
    window
        .window()
        .set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
    // 空表示は詳細ペイン（`has-selection` の内側）にあるので、選択済みの状態にする。
    window.set_has_selection(true);
    window.set_selected_index(3);
    window.set_detail_transcript_heading("No transcript yet".into());
    window.set_detail_transcript_body("Automatic transcription is off.".into());
    window.set_detail_transcript_actions(ModelRc::from(Rc::new(VecModel::from(actions))));
    window
}

fn action(label: &str, kind: PaneActionKind) -> PaneAction {
    PaneAction {
        label: label.into(),
        kind,
        primary: true,
    }
}

/// 空表示のボタンは、**押された操作をそのまま返す**（どこへ繋ぐかは Rust が決める）。
/// あわせて、対象は選択中のセッションであることを `selected-index` で確かめる。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn pane_action_reports_the_kind_that_was_pressed() {
    let window = window_with_empty_pane(vec![
        action("Transcribe now", PaneActionKind::Transcribe),
        action("Open transcription", PaneActionKind::OpenTranscription),
    ]);
    let pressed: Rc<RefCell<Vec<PaneActionKind>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let pressed = Rc::clone(&pressed);
        window.on_pane_action(move |kind| pressed.borrow_mut().push(kind));
    }

    let buttons: Vec<ElementHandle> =
        ElementHandle::find_by_element_type_name(&window, "ActionButton").collect();
    // 空表示のボタン以外（Play / Stop / 詳細ヘッダ）も同じ型なので、ラベルで絞る。
    let by_label = |label: &str| {
        buttons
            .iter()
            .find(|button| button.accessible_label().as_deref() == Some(label))
            .unwrap_or_else(|| panic!("no ActionButton labelled {label} was found"))
    };

    by_label("Open transcription").invoke_accessible_default_action();
    by_label("Transcribe now").invoke_accessible_default_action();

    assert_eq!(
        *pressed.borrow(),
        vec![
            PaneActionKind::OpenTranscription,
            PaneActionKind::Transcribe
        ],
        "each button must report its own action"
    );
}

/// 追従スイッチは**押せば状態が変わる**（押しても何も起きないスイッチを置かない）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn follow_transcript_switch_toggles() {
    let window = window_with_empty_pane(Vec::new());
    assert!(
        window.get_follow_transcript(),
        "following is on by default (the design shows the played line followed)"
    );

    let toggle = ElementHandle::find_by_accessible_label(&window, "Follow transcript")
        .next()
        .expect("no Follow transcript switch was found in the player strip");
    toggle.invoke_accessible_default_action();
    assert!(
        !window.get_follow_transcript(),
        "pressing it turns following off"
    );
    toggle.invoke_accessible_default_action();
    assert!(
        window.get_follow_transcript(),
        "pressing it again turns following back on"
    );
}

/// 検索欄（#161）は、**押した操作をそのまま返す**（絞り込みは Rust が決める）。
/// `✕` は中身を空にするだけでなく、一覧を戻す `clear-search` を通す必要がある。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (see docs/rules/slint.md)"
)]
fn clearing_the_search_asks_for_the_list_to_be_restored() {
    let window = window_with_empty_pane(Vec::new());
    window.set_search_text("recording format".into());

    let cleared: Rc<RefCell<u32>> = Rc::new(RefCell::new(0));
    {
        let cleared = Rc::clone(&cleared);
        window.on_clear_search(move || *cleared.borrow_mut() += 1);
    }

    ElementHandle::find_by_accessible_label(&window, "Clear search")
        .next()
        .expect("no Clear search control was found in the search field")
        .invoke_accessible_default_action();

    assert_eq!(
        *cleared.borrow(),
        1,
        "pressing it must ask for the list to be restored, not just blank the field"
    );
    assert_eq!(
        window.get_search_text(),
        "recording format",
        "the field is blanked by the Rust side, together with rebuilding the list"
    );
}
