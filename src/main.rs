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

/// ウィンドウの初期ジオメトリ。イベントループ稼働中に初めて show() すると、位置・サイズが
/// 確定されないまま高さ 0 で表示される。初回表示時にこの値を明示してジオメトリを確定させる。
const WINDOW_WIDTH: f32 = 360.0;
const WINDOW_HEIGHT: f32 = 220.0;
const WINDOW_X: f32 = 240.0;
const WINDOW_Y: f32 = 160.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 常駐アプリとして Dock にアイコンを出さない（macOS）。
    #[cfg(target_os = "macos")]
    hide_dock_icon();

    // ウィンドウは生成するが表示はしない（起動時はトレイのみ）。
    let ui = AppWindow::new()?;

    // Slint バックエンドの初期化後にトレイを常駐させる（macOS の NSApplication 初期化後）。
    let tray = Tray::new()?;

    // ウィンドウを閉じても終了させず、非表示にして常駐を保つ。
    // メニューの表示状態と整合させるため、トグル項目のラベルも戻す。
    let toggle_on_close = tray.toggle_item.clone();
    ui.window().on_close_requested(move || {
        toggle_on_close.set_text("ウィンドウを表示");
        slint::CloseRequestResponse::HideWindow
    });

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
    // クロージャは 'static のため &Tray を借用できない。必要な要素（トグル項目と
    // 各項目の ID）だけを複製して所有する。
    let toggle_item = tray.toggle_item.clone();
    let toggle_id = tray.toggle_item.id().clone();
    let quit_id = tray.quit_item.id().clone();
    let menu_channel = MenuEvent::receiver();
    // 初回表示でジオメトリを確定させたか。2 回目以降は位置・サイズを動かさない。
    let mut geometry_committed = false;

    move || {
        while let Ok(event) = menu_channel.try_recv() {
            if event.id == toggle_id {
                let Some(ui) = ui.upgrade() else { continue };
                let window = ui.window();
                if window.is_visible() {
                    if let Err(err) = window.hide() {
                        eprintln!("ウィンドウの非表示に失敗した: {err}");
                    }
                    toggle_item.set_text("ウィンドウを表示");
                } else {
                    if !geometry_committed {
                        // 初回 show() でジオメトリが確定されず高さ 0 になるのを防ぐため、
                        // 位置とサイズを明示してから表示する。set_position が無いと高さ 0 になる。
                        window.set_position(slint::LogicalPosition::new(WINDOW_X, WINDOW_Y));
                        window.set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
                        geometry_committed = true;
                    }
                    if let Err(err) = window.show() {
                        eprintln!("ウィンドウの表示に失敗した: {err}");
                    }
                    toggle_item.set_text("ウィンドウを隠す");
                }
            } else if event.id == quit_id
                && let Err(err) = slint::quit_event_loop()
            {
                eprintln!("イベントループの終了に失敗した: {err}");
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

    let mtm = MainThreadMarker::new().expect("main は常にメインスレッドで動くため成功する");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}
