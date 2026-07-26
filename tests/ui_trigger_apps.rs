//! 設定画面の「Trigger apps」一覧が、自動録音で検知できないアプリの注記を出すことのテスト。
//!
//! 自動録音は登録アプリが対象外（Safari 等の WebKit 系）でも**黙って発火しない**だけなので、
//! 一覧でその旨が見えることが機能の一部になる（#107。理由は
//! `app_audio_monitor::auto_record_limitation` の doc を参照）。
//!
//! Slint のデバッグ情報が要る（`build.rs` の `slint_debug_info`）。使い方は `docs/rules/slint.md`。

mod ui_support;

use i_slint_backend_testing::ElementHandle;
use slint::{SharedString, VecModel};

slint::include_modules!();

/// 「Trigger apps」一覧の行が持つアクセシブルラベルを、上から順に集める。
///
/// 一覧の外（セクション見出しやチェックボックス等）を拾わないよう、行の部品
/// `TriggerAppRow` の配下だけを見る。注記の**不在**を等値でアサートしたいので、
/// 空ラベルも落とさずそのまま入れる。
fn row_labels(window: &AppWindow) -> Vec<String> {
    ElementHandle::find_by_element_type_name(window, "TriggerAppRow")
        .flat_map(|row| row.query_descendants().find_all())
        .filter_map(|element| element.accessible_label().map(|label| label.to_string()))
        .collect()
}

fn open_window(apps: Vec<TriggerApp>) -> AppWindow {
    ui_support::init_backend();
    let window = AppWindow::new().expect("create the settings window");
    // 一覧は Auto-record が ON のときだけ操作できるが、表示自体は OFF でも行われる。
    // 注記の有無を見るテストなので ON にして通常の見え方に揃える。
    window.set_auto_record_app(true);
    window.set_app_list(std::rc::Rc::new(VecModel::from(apps)).into());
    window
}

fn trigger_app(name: &str, limitation_note: &str) -> TriggerApp {
    TriggerApp {
        name: SharedString::from(name),
        limitation_note: SharedString::from(limitation_note),
    }
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn unsupported_app_shows_the_note() {
    let note = "Not detected — record manually";
    let window = open_window(vec![trigger_app("Safari", note)]);

    // 行に出るのは「名前・注記・Remove」の 3 つだけ。等値で見て、余計なものが出ないことも固定する。
    assert_eq!(
        row_labels(&window),
        vec!["Safari".to_owned(), note.to_owned(), "Remove".to_owned()]
    );
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn supported_app_shows_no_note() {
    let window = open_window(vec![trigger_app("Google Chrome", "")]);

    // 注記が空のときは Text 自体を出さない（空ラベルの要素も残さない）。件数で見ると、
    // 条件表示（`if`）を外して空文字を描画する実装でも通ってしまうため、等値で固定する。
    assert_eq!(
        row_labels(&window),
        vec!["Google Chrome".to_owned(), "Remove".to_owned()]
    );
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn removing_reports_the_index() {
    let window = open_window(vec![
        trigger_app("Google Chrome", ""),
        trigger_app("Safari", "note"),
    ]);
    let removed = std::rc::Rc::new(std::cell::Cell::new(None));
    let sink = std::rc::Rc::clone(&removed);
    window.on_remove_app(move |index| sink.set(Some(index)));

    // 注記の有無で行の構造が変わるため、2 行目（注記あり）の Remove が正しい添字を返すかを見る。
    let buttons: Vec<ElementHandle> =
        ElementHandle::find_by_accessible_label(&window, "Remove").collect::<Vec<_>>();
    assert_eq!(buttons.len(), 2, "one Remove button per row");
    buttons[1].invoke_accessible_default_action();
    assert_eq!(removed.get(), Some(1));
}

/// `app_list` が空なら行を 1 つも作らないこと（枠だけ残る想定）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn empty_list_has_no_rows() {
    let window = open_window(Vec::new());
    assert!(row_labels(&window).is_empty());
    assert_eq!(
        ElementHandle::find_by_accessible_label(&window, "Remove").count(),
        0
    );
}
