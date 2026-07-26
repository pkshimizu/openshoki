//! 設定画面の「Trigger apps」一覧が、自動録音で検知できないアプリの注記を出すことのテスト。
//!
//! 自動録音は登録アプリが対象外（Safari 等の WebKit 系）でも**黙って発火しない**だけなので、
//! 一覧でその旨が見えることが機能の一部になる（#107。理由は
//! `app_audio_monitor::auto_record_limitation` の doc を参照）。
//!
//! Slint のデバッグ情報が要る（`build.rs` の `slint_debug_info`）。使い方は `docs/rules/slint.md`。

mod ui_support;

use i_slint_backend_testing::ElementHandle;
use slint::{Model, SharedString, VecModel};

slint::include_modules!();

/// 一覧に出ているテキストを全部集める（要素の種類を問わず `text` を持つものを拾う）。
fn visible_texts(window: &AppWindow) -> Vec<String> {
    i_slint_backend_testing::ElementQuery::from_root(window)
        .match_predicate(|element| element.accessible_label().is_some())
        .find_all()
        .into_iter()
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

fn app(name: &str, limitation: &str) -> TriggerApp {
    TriggerApp {
        name: SharedString::from(name),
        limitation: SharedString::from(limitation),
    }
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn unsupported_app_shows_the_limitation() {
    let limitation = "Auto-record cannot detect this app because WebKit opens the mic in a shared system process.";
    let window = open_window(vec![app("Safari", limitation)]);

    let texts = visible_texts(&window);
    assert!(
        texts.iter().any(|text| text == "Safari"),
        "the app name should be listed: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text == limitation),
        "the limitation should be shown next to it: {texts:?}"
    );
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn supported_app_shows_no_note() {
    let limitation = "Auto-record cannot detect this app because WebKit opens the mic in a shared system process.";
    let window = open_window(vec![app("Google Chrome", ""), app("Safari", limitation)]);

    let texts = visible_texts(&window);
    // 対応しているアプリには注記を出さない（1 件だけ = Safari のぶん）。
    assert_eq!(
        texts.iter().filter(|text| *text == limitation).count(),
        1,
        "only the unsupported app should carry a note: {texts:?}"
    );
    assert!(
        texts.iter().any(|text| text == "Google Chrome"),
        "the supported app should still be listed: {texts:?}"
    );
}

#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn removing_reports_the_index() {
    let window = open_window(vec![app("Google Chrome", ""), app("Safari", "note")]);
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

/// `app_list` が空でも一覧が壊れないこと（枠だけ残る想定）。
#[test]
#[cfg_attr(
    not(slint_debug_info),
    ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
)]
fn empty_list_has_no_rows() {
    let window = open_window(Vec::new());
    assert_eq!(window.get_app_list().row_count(), 0);
    assert_eq!(
        ElementHandle::find_by_accessible_label(&window, "Remove").count(),
        0
    );
}
