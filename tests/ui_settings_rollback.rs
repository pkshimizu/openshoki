//! 入力ウィジェットが、Rust 側からの書き戻し（保存失敗時の巻き戻し）を**ユーザー操作の
//! あとでも**反映することのテスト。
//!
//! #141 で機能の設定が専用ウィンドウへ移ったので、対象も移設先で見る（設定画面に残るのは
//! 録音・自動録音と、機能ウィンドウへの扉だけ）。
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

/// トグルはラベルを隣の Text に持たせているため、要素型で探して並び順（宣言順 = 上から）で選ぶ。
fn toggles(window: &AppWindow) -> Vec<ElementHandle> {
    ElementHandle::find_by_element_type_name(window, "Toggle").collect()
}

/// 同じく Stepper（「Stop recording after the mic is released for」の 1 つだけ）。
fn stepper(window: &AppWindow) -> ElementHandle {
    ElementHandle::find_by_element_type_name(window, "Stepper")
        .next()
        .expect("the settings window has a stepper")
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn rust_can_roll_back_a_toggle_after_the_user_flipped_it() {
    ui_support::init_backend();
    let window = AppWindow::new().expect("create the settings window");
    ui_support::fit_settings_content(&window);
    window.set_auto_record_app(false);

    let boxes = toggles(&window);
    let first = boxes.first().expect("the settings window has a toggle");
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
        "the toggle should follow the value Rust wrote back"
    );
}

/// Stepper（自動停止の待ち時間）も同じ契約を持つ。ここは保存**成功**時にも Rust から書き戻す
/// （範囲へ丸めた値を反映する経路がある）ので、片方向に戻ると失敗時だけでなく丸めも届かなくなる。
/// 見るのは「操作のあとでも Rust の set が届くこと」までで、丸めの計算自体は
/// `config::clamp_debounce_secs` のテストが持つ。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn rust_can_write_back_a_delay_after_the_user_edited_it() {
    ui_support::init_backend();
    let window = AppWindow::new().expect("create the settings window");
    ui_support::fit_settings_content(&window);
    // Stepper は自動録音 ON のときだけ操作できる（`deps` のゲート）。
    window.set_auto_record_app(true);
    window.set_auto_stop_debounce_secs(4);

    let stepper = stepper(&window);
    assert_eq!(stepper.accessible_value().as_deref(), Some("4"));

    // ユーザー操作で 1 増やす（Stepper の increment アクション）。
    stepper.invoke_accessible_increment_action();
    assert_eq!(window.get_auto_stop_debounce_secs(), 5);

    // Rust 側が保存済みの値へ書き戻す。
    window.set_auto_stop_debounce_secs(4);
    assert_eq!(
        stepper.accessible_value().as_deref(),
        Some("4"),
        "the stepper should follow the value Rust wrote back"
    );
}

/// 認識言語の `Select` も同じ契約を持つ（#141 で文字起こしウィンドウへ移設）。
///
/// **モデルの選択 UI はもう無い**——選ぶ場所は一覧の `Use` だけになったので、書き戻しの契約が
/// 要るのは言語だけになった。選択の変更はキー操作で行う（ポップアップの項目をクリックする
/// 経路はヘッドレスでは不安定）。クリックでフォーカスが `Select` へ移り、続く矢印キーを
/// `Select` 自身の FocusScope が受けて選択を 1 つ動かす。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn rust_can_roll_back_a_language_choice_after_the_user_changed_it() {
    ui_support::init_backend();
    let window = TranscriptionWindow::new().expect("create the transcription window");
    window
        .window()
        .set_size(slint::LogicalSize::new(620.0, 780.0));
    window.set_languages(
        std::rc::Rc::new(slint::VecModel::from(vec![
            slint::SharedString::from("English"),
            slint::SharedString::from("Japanese"),
        ]))
        .into(),
    );
    window.set_language_index(1);

    let select = ElementHandle::find_by_element_type_name(&window, "Select")
        .next()
        .expect("the transcription window has a language select");
    assert_eq!(select.accessible_value().as_deref(), Some("Japanese"));

    // ユーザー操作で 1 つ上へ（クリックでフォーカスを与え、↑ で English へ）。
    select.mock_single_click(slint::platform::PointerEventButton::Left);
    window
        .window()
        .dispatch_event(slint::platform::WindowEvent::KeyPressed {
            text: slint::platform::Key::UpArrow.into(),
        });
    assert_eq!(window.get_language_index(), 0);

    // 保存に失敗した Rust 側が、保存済みの選択（Japanese）へ書き戻す。
    window.set_language_index(1);
    assert_eq!(
        select.accessible_value().as_deref(),
        Some("Japanese"),
        "the select should follow the index Rust wrote back"
    );
}
