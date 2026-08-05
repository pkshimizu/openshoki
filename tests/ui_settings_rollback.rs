//! 設定画面の入力ウィジェットが、Rust 側からの書き戻し（保存失敗時の巻き戻し）を
//! **ユーザー操作のあとでも**反映することのテスト。
//!
//! `docs/rules/slint.md` の「in-out プロパティの操作は、保存失敗時に表示を旧値へ戻す」は、
//! ウィジェット側が `checked: root.x` のような**片方向バインディング**だと成立しない
//! （std-widgets は操作時に自分のプロパティへ命令的に代入し、その時点でバインディングが
//! 外れるため、以後 `root.x` を set しても表示が追従しない）。ここではユーザー操作 →
//! Rust から書き戻し → 表示が戻ることを、実際のポインタ相当の操作で固定する。
//!
//! Slint のデバッグ情報が要る（`build.rs` の `slint_debug_info`）。使い方は `docs/rules/slint.md`。

mod ui_support;

use i_slint_backend_testing::ElementHandle;

slint::include_modules!();

/// 設定画面のチェックボックスはラベルを隣の Text に持たせているため、要素型で探して
/// 並び順（宣言順 = 上から。Auto-record → Auto-transcribe → Write meeting minutes）で選ぶ。
fn check_boxes(window: &AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, "CheckBox").collect()
}

/// 同じく ComboBox を上から順に集める（Language → Model → Minutes Model）。
fn combo_boxes(window: &AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, "ComboBox").collect()
}

/// 同じく SpinBox（Auto-stop delay の 1 つだけ）。
fn spin_box(window: &AppWindow) -> ElementHandle {
    ElementHandle::find_by_element_type_name(window, "SpinBox")
        .next()
        .expect("the settings window has a spin box")
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn rust_can_roll_back_a_toggle_after_the_user_flipped_it() {
    ui_support::init_backend();
    let window = AppWindow::new().expect("create the settings window");
    window.set_auto_record_app(false);

    let boxes = check_boxes(&window);
    let first = boxes.first().expect("the settings window has a check box");
    assert_eq!(first.accessible_checked(), Some(false));

    // ユーザー操作で ON にする（Slint 側が先に自分の checked を更新してから
    // toggle-auto-record-app を呼ぶ流儀）。
    first.invoke_accessible_default_action();
    assert_eq!(first.accessible_checked(), Some(true));
    assert!(window.get_auto_record_app());

    // 保存に失敗した Rust 側が、保存済みの値（OFF）へ書き戻す。
    window.set_auto_record_app(false);
    assert_eq!(
        first.accessible_checked(),
        Some(false),
        "the check box should follow the value Rust wrote back"
    );
}

/// SpinBox（自動停止の待ち時間）も同じ契約を持つ。ここは保存**成功**時にも書き戻す
/// （Rust 側が範囲へ丸めた値を反映する）ため、片方向に戻ると丸めの反映も届かなくなる。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn rust_can_write_back_a_delay_after_the_user_edited_it() {
    ui_support::init_backend();
    let window = AppWindow::new().expect("create the settings window");
    // SpinBox は自動録音 ON のときだけ操作できる（`deps` のゲート）。
    window.set_auto_record_app(true);
    window.set_auto_stop_debounce_secs(4);

    let spin = spin_box(&window);
    assert_eq!(spin.accessible_value().as_deref(), Some("4"));

    // ユーザー操作で 1 増やす（SpinBox の increment アクション）。
    spin.invoke_accessible_increment_action();
    assert_eq!(window.get_auto_stop_debounce_secs(), 5);

    // Rust 側が保存済みの値（丸めた値）へ書き戻す。
    window.set_auto_stop_debounce_secs(4);
    assert_eq!(
        spin.accessible_value().as_deref(),
        Some("4"),
        "the spin box should follow the value Rust wrote back"
    );
}

/// 要約 LLM の ComboBox も同じ契約を持つ（#119 で足した選択 UI。whisper・言語の ComboBox も
/// 同じ束縛にしてあるので、代表として一番下のものを見る）。
///
/// 選択の変更はキー操作で行う（ポップアップの項目をクリックする経路はヘッドレスでは不安定）。
/// クリックで `ComboBoxBase` の TouchArea がフォーカスを取ってポップアップを開き、続く矢印キーは
/// ポップアップ側の FocusScope（`popup-key-handler`）が受けて選択を 1 つ動かす。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn rust_can_roll_back_a_model_choice_after_the_user_changed_it() {
    const LIGHT: &str = "Light — 2.0 GB — fast";
    const HEAVY: &str = "Heavy — 4.4 GB — slow";

    ui_support::init_backend();
    let window = AppWindow::new().expect("create the settings window");
    // ComboBox は自動文字起こし ON のときだけ操作できる（`transcribe-deps` のゲート）。
    window.set_auto_transcribe(true);
    window.set_summary_models(
        std::rc::Rc::new(slint::VecModel::from(vec![
            slint::SharedString::from(LIGHT),
            slint::SharedString::from(HEAVY),
        ]))
        .into(),
    );
    window.set_summary_model_index(1);

    let combo = combo_boxes(&window)
        .pop()
        .expect("the settings window has a summary model combo box");
    assert_eq!(combo.accessible_value().as_deref(), Some(HEAVY));

    // ユーザー操作で軽い方へ移す（クリックでフォーカスを与え、↑ で 1 つ上の選択肢へ）。
    combo.mock_single_click(slint::platform::PointerEventButton::Left);
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: slint::platform::Key::UpArrow.into(),
        });
    assert_eq!(window.get_summary_model_index(), 0);

    // 保存に失敗した Rust 側が、保存済みの選択（Heavy）へ書き戻す。表示テキスト（`current-value`）は
    // ComboBoxBase の `changed current-index` ハンドラ経由で追従し、このハンドラはイベントループの
    // 次の回で走るので、set 直後には反映されていない。`mock_elapsed_time` は経過量に関係なく
    // changed ハンドラを流すので、待ち時間としての意味は無い（0 でよい）。
    window.set_summary_model_index(1);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::ZERO);
    assert_eq!(
        combo.accessible_value().as_deref(),
        Some(HEAVY),
        "the combo box should follow the index Rust wrote back"
    );
}
