//! openshoki — メニューバー／タスクバーに常駐する録音アプリ（基盤）。
//!
//! 起動時はウィンドウを表示せずトレイに常駐し、トレイメニューから Slint ウィンドウの
//! 表示/非表示とアプリ終了を行う。録音機能は後続の issue で実装する。

mod tray;

use std::time::Duration;

use tray_icon::menu::MenuEvent;

use crate::tray::Tray;

slint::include_modules!();

/// メニューイベントのポーリング周期。アイドル時の負荷を抑えつつ、操作の体感遅延が
/// 出ない程度の値にする。
const MENU_POLL_INTERVAL: Duration = Duration::from_millis(100);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 常駐アプリとして Dock にアイコンを出さない（macOS）。
    #[cfg(target_os = "macos")]
    hide_dock_icon();

    // ウィンドウは生成するが表示はしない（起動時はトレイのみ）。
    let ui = AppWindow::new()?;

    // Slint バックエンドの初期化後にトレイを常駐させる（macOS の NSApplication 初期化後）。
    let tray = Tray::new()?;

    // トレイのメニューイベントを Slint のイベントループ上でポーリングし、
    // ウィンドウ操作・終了へ橋渡しする。
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        MENU_POLL_INTERVAL,
        menu_event_handler(ui.as_weak(), &tray),
    );

    slint::run_event_loop()?;

    // イベントループ終了後、トレイを明示的に解放してアイコンを残さない。
    drop(timer);
    drop(tray);
    Ok(())
}

/// メニューイベントを処理するクロージャを作る。
///
/// 表示/非表示トグルはウィンドウの現在の可視状態から判断し、別途フラグを持たない
/// （「ありえない状態」を作らないため）。
fn menu_event_handler(ui: slint::Weak<AppWindow>, tray: &Tray) -> impl FnMut() + 'static {
    let toggle_item = tray.toggle_item.clone();
    let toggle_id = tray.toggle_item.id().clone();
    let quit_id = tray.quit_item.id().clone();
    let menu_channel = MenuEvent::receiver();

    move || {
        while let Ok(event) = menu_channel.try_recv() {
            if event.id == toggle_id {
                let Some(ui) = ui.upgrade() else { continue };
                let window = ui.window();
                if window.is_visible() {
                    let _ = window.hide();
                    toggle_item.set_text("ウィンドウを表示");
                } else {
                    let _ = window.show();
                    toggle_item.set_text("ウィンドウを隠す");
                }
            } else if event.id == quit_id {
                let _ = slint::quit_event_loop();
            }
        }
    }
}

/// macOS で Dock アイコンを隠し、メニューバー常駐アプリとして振る舞わせる。
///
/// activation policy を Accessory にすることで Dock とアプリスイッチャーに出なくなる。
/// 配布パッケージでは `Info.plist` の `LSUIElement` 指定が確実だが、それはパッケージング時に扱う。
#[cfg(target_os = "macos")]
fn hide_dock_icon() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let mtm = MainThreadMarker::new().expect("main スレッドで実行する必要がある");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}
