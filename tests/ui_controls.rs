//! 自作の操作部品（`ActionButton` / `Toggle` / `Stepper` / `Select`）の操作テスト（#146）。
//!
//! 標準ウィジェットを置き換えたぶん、**標準が持っていたものを自分で用意できているか**を
//! ここで固定する: キーボードで操作できること、`enabled` が false なら発火しないこと、
//! 押下中にポインタが外へ出たら発火しないこと。
//!
//! 自作部品は `enabled` を無視して発火しても静かに壊れる（見た目だけ淡くなって押せてしまう）。
//! 見た目は `examples/settings_view.rs` で目視し、ここでは配線だけを見る
//! （`docs/rules/slint.md`「UI 操作の検証は `tests/` のテストバックエンドで」）。

#[path = "ui_support/mod.rs"]
mod ui_support;

use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;

slint::include_modules!();

/// 設定画面を、本文がすべて収まる大きさで開く。
fn open_window() -> AppWindow {
    ui_support::init_backend();
    let window = AppWindow::new().expect("create the settings window");
    ui_support::fit_settings_content(&window);
    window
}

fn find_all(window: &AppWindow, type_name: &str) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, type_name).collect()
}

/// 上から N 番目の部品。並び順は宣言順（`ui/app-window.slint`）。
fn nth(window: &AppWindow, type_name: &str, index: usize) -> ElementHandle {
    find_all(window, type_name)
        .into_iter()
        .nth(index)
        .unwrap_or_else(|| panic!("the settings window has at least {} {type_name}", index + 1))
}

fn press_key(window: &AppWindow, key: slint::platform::Key) {
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: key.into() });
}

fn press_text(window: &AppWindow, text: &str) {
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed { text: text.into() });
}

/// トグルはキーボードだけで切り替えられる（クリックでフォーカスし、Space で反転）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_toggle_flips_from_the_keyboard() {
    let window = open_window();
    window.set_auto_record_app(false);

    let toggle = nth(&window, "Toggle", 0);
    toggle.mock_single_click(slint::platform::PointerEventButton::Left);
    // クリック自体で 1 回反転する（押した先がフォーカスされる）。
    assert!(window.get_auto_record_app());

    press_text(&window, " ");
    assert!(
        !window.get_auto_record_app(),
        "Space should flip the focused toggle"
    );
}

/// `enabled` が false のトグルは、ポインタでもキーボードでも動かない。
///
/// 議事録のトグルは自動文字起こしが OFF のとき無効になる（`transcribe-deps` のゲート）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_disabled_toggle_ignores_pointer_and_keyboard() {
    let window = open_window();
    window.set_auto_transcribe(false); // 議事録トグルを無効にするゲート
    window.set_auto_summarize(false);

    // 宣言順は Record automatically → Transcribe recordings → Generate meeting notes。
    let notes = nth(&window, "Toggle", 2);
    notes.mock_single_click(slint::platform::PointerEventButton::Left);
    assert!(
        !window.get_auto_summarize(),
        "a disabled toggle must not flip on click"
    );

    press_text(&window, " ");
    assert!(
        !window.get_auto_summarize(),
        "a disabled toggle must not flip from the keyboard"
    );
}

/// `enabled` が false の部品は、**支援技術からの操作**でも動かない。
///
/// ポインタとキーボードを塞いでも `accessible-action-*` が素通りすると、無効な部品を
/// 押せてしまう（見た目は淡いまま動くので、目視では気づけない静かな壊れ方）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn disabled_parts_ignore_accessibility_actions() {
    let window = open_window();
    // 自動録音 OFF で「Add app…」とステッパーを、自動文字起こし OFF で議事録トグルと
    // 選択を無効にする。
    window.set_auto_record_app(false);
    window.set_auto_transcribe(false);
    window.set_auto_summarize(false);
    window.set_auto_stop_debounce_secs(4);
    window.set_transcribe_language_index(0);

    let fired = std::rc::Rc::new(std::cell::Cell::new(0));
    let counter = fired.clone();
    window.on_add_app(move || counter.set(counter.get() + 1));

    ElementHandle::find_by_accessible_label(&window, "Add app…")
        .next()
        .expect("the settings window has an add-app button")
        .invoke_accessible_default_action();
    assert_eq!(fired.get(), 0, "a disabled button must not fire");

    nth(&window, "Toggle", 2).invoke_accessible_default_action();
    assert!(
        !window.get_auto_summarize(),
        "a disabled toggle must not flip"
    );

    nth(&window, "Stepper", 0).invoke_accessible_increment_action();
    assert_eq!(
        window.get_auto_stop_debounce_secs(),
        4,
        "a disabled stepper must not change its value"
    );
}

/// ステッパーは上下キーで増減し、範囲を超えない（丸めは部品が持つ）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_stepper_moves_with_arrow_keys_and_stops_at_the_bounds() {
    let window = open_window();
    window.set_auto_record_app(true);
    window.set_auto_stop_debounce_secs(2);

    let stepper = nth(&window, "Stepper", 0);
    stepper.mock_single_click(slint::platform::PointerEventButton::Left);

    press_key(&window, slint::platform::Key::UpArrow);
    assert_eq!(window.get_auto_stop_debounce_secs(), 3);
    press_key(&window, slint::platform::Key::DownArrow);
    press_key(&window, slint::platform::Key::DownArrow);
    assert_eq!(window.get_auto_stop_debounce_secs(), 1);
    // 下限（`ui/app-window.slint` の minimum = 1）で止まる。
    press_key(&window, slint::platform::Key::DownArrow);
    assert_eq!(
        window.get_auto_stop_debounce_secs(),
        1,
        "the stepper must clamp at its minimum"
    );
}

/// `enabled` が false のステッパーは、支援技術からの増減も受け付けない。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_disabled_stepper_ignores_increment() {
    let window = open_window();
    window.set_auto_record_app(false); // ステッパーを無効にするゲート
    window.set_auto_stop_debounce_secs(4);

    let stepper = nth(&window, "Stepper", 0);
    stepper.invoke_accessible_increment_action();
    assert_eq!(
        window.get_auto_stop_debounce_secs(),
        4,
        "a disabled stepper must not change its value"
    );
}

/// ステッパーの ± は、**フォーカスが無い状態の 1 回目のクリック**でも値が動く。
///
/// `FocusScope` を `TouchArea` より後ろに置くと、フォーカスが無いあいだ最初の押下を
/// `FocusScope` が吸ってしまい、1 回目が空振りする（2 回目から効くので気づきにくい）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn the_first_click_on_a_stepper_button_already_moves_the_value() {
    let window = open_window();
    window.set_auto_record_app(true);
    window.set_auto_stop_debounce_secs(3);

    // ステッパーの中の ± は宣言順に − → ＋。
    nth(&window, "StepperButton", 1).mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(
        window.get_auto_stop_debounce_secs(),
        4,
        "the very first click on + must already step the value"
    );
}

/// 選択肢が空でも、上下キーで選択位置が範囲外へ出ない。
///
/// `model.length - 1` は空のとき -1 になるので、丸めの下限と上限が逆転する。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_select_with_no_options_stays_in_range() {
    let window = open_window();
    window.set_auto_transcribe(true);
    window.set_transcribe_languages(std::rc::Rc::new(slint::VecModel::from(vec![])).into());
    window.set_transcribe_language_index(0);

    let chosen = std::rc::Rc::new(std::cell::Cell::new(i32::MIN));
    let seen = chosen.clone();
    window.on_change_transcribe_language(move |index| seen.set(index));

    let select = nth(&window, "Select", 0);
    select.mock_single_click(slint::platform::PointerEventButton::Left);
    press_key(&window, slint::platform::Key::DownArrow);
    press_key(&window, slint::platform::Key::UpArrow);

    assert_eq!(
        window.get_transcribe_language_index(),
        0,
        "an empty select must not move its index out of range"
    );
    assert_eq!(
        chosen.get(),
        i32::MIN,
        "an empty select must not report a selection"
    );
}

/// 選択は上下キーで動き、端で止まる（巡回しない）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_select_moves_with_arrow_keys_and_stops_at_the_ends() {
    let window = open_window();
    window.set_auto_transcribe(true);
    window.set_transcribe_languages(
        std::rc::Rc::new(slint::VecModel::from(vec![
            slint::SharedString::from("English"),
            slint::SharedString::from("Japanese"),
        ]))
        .into(),
    );
    window.set_transcribe_language_index(0);

    let select = nth(&window, "Select", 0);
    select.mock_single_click(slint::platform::PointerEventButton::Left);

    press_key(&window, slint::platform::Key::DownArrow);
    assert_eq!(window.get_transcribe_language_index(), 1);
    // 末尾で止まる。
    press_key(&window, slint::platform::Key::DownArrow);
    assert_eq!(
        window.get_transcribe_language_index(),
        1,
        "the select must not wrap around at the end"
    );
    press_key(&window, slint::platform::Key::UpArrow);
    assert_eq!(window.get_transcribe_language_index(), 0);
    press_key(&window, slint::platform::Key::UpArrow);
    assert_eq!(
        window.get_transcribe_language_index(),
        0,
        "the select must not wrap around at the start"
    );
}

/// ボタンは Enter で押せる。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_button_fires_from_the_keyboard() {
    let window = open_window();
    let fired = std::rc::Rc::new(std::cell::Cell::new(0));
    let counter = fired.clone();
    window.on_choose_folder(move || counter.set(counter.get() + 1));

    // 宣言順の先頭は「Change…」（保存先を選び直す）。
    let button = nth(&window, "ActionButton", 0);
    button.mock_single_click(slint::platform::PointerEventButton::Left);
    assert_eq!(fired.get(), 1);

    press_text(&window, "\n");
    assert_eq!(fired.get(), 2, "Enter should press the focused button");
}

/// `enabled` が false のボタンは、ポインタでもキーボードでも発火しない。
///
/// 「Add app…」は自動録音が OFF のとき無効になる。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn a_disabled_button_never_fires() {
    let window = open_window();
    window.set_auto_record_app(false);
    let fired = std::rc::Rc::new(std::cell::Cell::new(0));
    let counter = fired.clone();
    window.on_add_app(move || counter.set(counter.get() + 1));

    let add = ElementHandle::find_by_accessible_label(&window, "Add app…")
        .next()
        .expect("the settings window has an add-app button");
    add.mock_single_click(slint::platform::PointerEventButton::Left);
    press_text(&window, "\n");
    assert_eq!(
        fired.get(),
        0,
        "a disabled button must not fire from pointer or keyboard"
    );
}
