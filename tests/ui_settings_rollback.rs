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
/// 並び順（宣言順 = 上から）で選ぶ。
fn check_boxes(window: &AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, "CheckBox").collect()
}

/// 同じく ComboBox を上から順に集める（Language → Model → Minutes Model）。
fn combo_boxes(window: &AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, "ComboBox").collect()
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

/// 要約 LLM の ComboBox も同じ契約を持つ（この PR で足した選択 UI。whisper・言語の ComboBox も
/// 同じ束縛にしてあるので、代表として一番下のものを見る）。
///
/// 選択の変更は、閉じた ComboBox がフォーカス中に受ける矢印キー（`ComboBoxBase` の
/// `key-pressed`）で行う。ポップアップを開いて項目をクリックする経路はヘッドレスでは不安定。
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
    // ComboBoxBase の `changed current-index` ハンドラ経由で追従するため、1 フレーム進める
    // （このハンドラはイベントループの次の回で走るので、set 直後には反映されていない）。
    window.set_summary_model_index(1);
    i_slint_backend_testing::mock_elapsed_time(std::time::Duration::from_millis(16));
    assert_eq!(
        combo.accessible_value().as_deref(),
        Some(HEAVY),
        "the combo box should follow the index Rust wrote back"
    );
}
