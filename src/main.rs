//! openshoki — メニューバー／タスクバーに常駐する録音アプリ（基盤）。
//!
//! 起動時はウィンドウを表示せずトレイに常駐し、トレイメニューから Slint ウィンドウの
//! 表示/非表示とアプリ終了を行う。録音機能は後続の issue で実装する。

mod config;
#[cfg(target_os = "macos")]
mod mic_monitor;
mod recorder;
#[cfg(target_os = "macos")]
mod system_audio;
mod tray;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tray_icon::menu::{MenuEvent, MenuItem};

use crate::config::Config;
use crate::recorder::Recorder;
use crate::tray::{
    RECORD_LABEL_START, RECORD_LABEL_STOP, SETTINGS_LABEL_CLOSE, SETTINGS_LABEL_OPEN, Tray,
};

slint::include_modules!();

/// メニューイベントのポーリング周期。アイドル時の負荷を抑えつつ、操作の体感遅延が
/// 出ない程度の値にする。録音中のメニューバー表示更新（経過時間・点滅）もこの周期に相乗りする。
const MENU_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 録音中アイコンの明滅（breathing）の 1 サイクル（明→暗→明）の秒数。サイン波でゆったり
/// 変化させる。実機の見え方で微調整しやすいよう定数化する。
const BLINK_CYCLE_SECS: f32 = 2.0;

/// ウィンドウの初期ジオメトリ。イベントループ稼働中に初めて show() すると、位置・サイズが
/// 確定されないまま高さ 0 で表示される。初回表示時にこの値を明示してジオメトリを確定させる。
/// 幅・高さは `ui/app-window.slint` の min/preferred と一致させること（片方だけ変えない）。
const WINDOW_WIDTH: f32 = 420.0;
const WINDOW_HEIGHT: f32 = 300.0;
/// 初回表示位置（画面左上からの暫定値）。中央寄せ等の調整は後続に回す。
const WINDOW_X: f32 = 240.0;
const WINDOW_Y: f32 = 160.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 常駐アプリとして Dock にアイコンを出さない（macOS）。
    #[cfg(target_os = "macos")]
    hide_dock_icon();

    // ウィンドウは生成するが表示はしない（起動時はトレイのみ）。
    let ui = AppWindow::new()?;

    // 設定を読み込み、現在の保存先・自動録音トグルを画面へ反映する。失敗時は load() がデフォルトを返す。
    let config = Rc::new(RefCell::new(Config::load()));
    ui.set_recording_dir(recording_dir_text(&config.borrow().recording_dir));
    ui.set_auto_record(config.borrow().auto_record_on_mic_active);

    // 「フォルダを選択」: ネイティブのフォルダ選択ダイアログで保存先を選び直し、保存・表示更新する。
    // コールバックはメインスレッド（Slint イベントループ）上で動くため、同期 API を使う。
    let config_for_pick = Rc::clone(&config);
    let ui_for_pick = ui.as_weak();
    ui.on_choose_folder(move || {
        let Some(ui) = ui_for_pick.upgrade() else {
            return;
        };
        // 現在の設定を複製し、選択結果を反映した候補を作る。
        let mut candidate = config_for_pick.borrow().clone();
        let mut dialog = rfd::FileDialog::new();
        if candidate.recording_dir.is_dir() {
            dialog = dialog.set_directory(&candidate.recording_dir);
        }
        let Some(folder) = dialog.pick_folder() else {
            return; // キャンセル時は何もしない。
        };
        candidate.recording_dir = folder;
        // 永続化に成功してからメモリ上の設定と画面表示を更新する。
        // 先に更新すると、保存失敗時に「表示は変わったのに保存されていない」不整合になる。
        if let Err(err) = candidate.save() {
            eprintln!("設定の保存に失敗したため、保存先は変更しない: {err}");
            return;
        }
        ui.set_recording_dir(recording_dir_text(&candidate.recording_dir));
        *config_for_pick.borrow_mut() = candidate;
    });

    // 「マイク使用時に自動録音」トグル: 変更を設定へ永続化する。永続化に成功してから
    // メモリ上の設定を更新する（先に更新すると保存失敗時に表示・メモリとディスクが食い違う）。
    // UI 側のチェック状態は Slint が保持するため、ここでは Config への反映のみ行う。
    let config_for_auto = Rc::clone(&config);
    ui.on_toggle_auto_record(move |enabled| {
        let mut candidate = config_for_auto.borrow().clone();
        candidate.auto_record_on_mic_active = enabled;
        if let Err(err) = candidate.save() {
            eprintln!("設定の保存に失敗したため、自動録音の設定は変更しない: {err}");
            return;
        }
        *config_for_auto.borrow_mut() = candidate;
    });

    // Slint バックエンドの初期化後にトレイを常駐させる（macOS の NSApplication 初期化後）。
    let tray = Tray::new()?;

    // マイク使用の監視を開始する（macOS のみ）。他アプリが既定入力デバイスを使い始めたことを
    // 検知し、設定 ON かつ未録音なら自動録音を開始する。連携失敗時はモニタ無しで常駐を続ける。
    #[cfg(target_os = "macos")]
    let mic_monitor = match mic_monitor::MicMonitor::start() {
        Ok(monitor) => Some(monitor),
        Err(err) => {
            eprintln!("マイク使用の監視を開始できないため、自動録音は無効で続行する: {err}");
            None
        }
    };

    // ウィンドウを閉じても終了させず、非表示にして常駐を保つ。
    // メニューの表示状態と整合させるため、トグル項目のラベルも戻す。
    let toggle_on_close = tray.toggle_item.clone();
    ui.window().on_close_requested(move || {
        toggle_on_close.set_text(SETTINGS_LABEL_OPEN);
        slint::CloseRequestResponse::HideWindow
    });

    // トレイのメニューイベントを Slint のイベントループ上でポーリングし、
    // ウィンドウ操作・終了へ橋渡しする。
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        MENU_POLL_INTERVAL,
        build_menu_event_handler(
            ui.as_weak(),
            &tray,
            Rc::clone(&config),
            #[cfg(target_os = "macos")]
            mic_monitor,
        ),
    );

    // run_event_loop() は「最後のウィンドウが閉じ、かつ最後の Slint の SystemTrayIcon が
    // 隠れた」時点で return する。本アプリのトレイは tray-icon クレート製で Slint からは
    // 見えないため、ウィンドウを隠すと「表示物ゼロ」と判定されてループが終了し、プロセスが
    // 落ちてしまう。常駐を保つため until_quit 版を使い、終了は quit_event_loop() だけに限る。
    slint::run_event_loop_until_quit()?;

    // イベントループ終了後、トレイを明示的に解放してアイコンを残さない。
    drop(timer);
    drop(tray);
    Ok(())
}

/// メニューイベントを処理するクロージャを作る。
///
/// 表示/非表示トグルや録音トグルは現在の状態（ウィンドウの可視状態・録音セッションの有無）から
/// 判断し、別途フラグを持たない（「ありえない状態」を作らないため）。
///
/// 録音セッション（`Option<Recorder>`）と `cpal::Stream`(`!Send`) はこのクロージャ内で所有する。
/// クロージャはメインスレッド（Slint イベントループ）上でのみ実行されるため問題ない。
fn build_menu_event_handler(
    ui: slint::Weak<AppWindow>,
    tray: &Tray,
    config: Rc<RefCell<Config>>,
    #[cfg(target_os = "macos")] mic_monitor: Option<mic_monitor::MicMonitor>,
) -> impl FnMut() + 'static {
    // クロージャは 'static のため &Tray を借用できない。必要な要素（各項目・ID・アイコン）
    // だけを複製して所有する。
    let toggle_item = tray.toggle_item.clone();
    let toggle_id = tray.toggle_item.id().clone();
    let record_item = tray.record_item.clone();
    let record_id = tray.record_item.id().clone();
    let quit_id = tray.quit_item.id().clone();
    let tray_icon = Rc::clone(&tray.icon);
    let menu_channel = MenuEvent::receiver();
    // 初回表示でジオメトリを確定させたか。2 回目以降は位置・サイズを動かさない。
    let mut geometry_committed = false;
    // 実行中の録音セッション。None=待機中、Some=録音中。
    let mut recorder: Option<Recorder> = None;
    // 録音中の経過時間テキストを、秒が変わったときだけ更新するための前回値。
    // アイコンの明滅は毎ティック更新するのでここでは持たない。
    let mut last_rendered_secs: Option<u64> = None;
    // 直前 tick で録音中だったか。録音中→待機の遷移を 1 度だけ拾って待機表示へ戻すのに使う。
    let mut was_recording = false;

    move || {
        while let Ok(event) = menu_channel.try_recv() {
            if event.id == toggle_id {
                let Some(ui) = ui.upgrade() else { continue };
                let window = ui.window();
                if window.is_visible() {
                    hide_window(window, &toggle_item);
                } else {
                    show_window(window, &toggle_item, &mut geometry_committed);
                }
            } else if event.id == record_id {
                toggle_recording(&mut recorder, &record_item, &config);
            } else if event.id == quit_id
                && let Err(err) = slint::quit_event_loop()
            {
                eprintln!("イベントループの終了に失敗した: {err}");
            }
        }

        // マイク使用の立ち上がり検知を毎ティック取り出し、設定 ON かつ未録音のときだけ自動開始する
        // （macOS のみ）。フラグは検知の有無にかかわらず取り出してクリアし、設定 OFF 中や録音中に
        // 起きた立ち上がりを溜め込んで後から誤発火させない（立ち上がりエッジだけを拾う）。
        #[cfg(target_os = "macos")]
        {
            let activated = mic_monitor.as_ref().is_some_and(|m| m.take_activated());
            if activated && recorder.is_none() && config.borrow().auto_record_on_mic_active {
                start_recording(&mut recorder, &record_item, &config);
            }
        }

        // 録音中はメニューバーへ経過時間と明滅を反映する。100ms ポーリング（≈10fps）に相乗りし、
        // アイコンは毎ティック明度レベルを更新して滑らかに明滅させる。経過時間テキストは
        // 秒が変わったときだけ更新して無駄な再設定を避ける。
        if let Some(session) = recorder.as_ref() {
            let elapsed = session.elapsed();
            let level = breathing_level(elapsed, BLINK_CYCLE_SECS);
            let secs = elapsed.as_secs();
            let update_title = last_rendered_secs != Some(secs);
            tray::render_recording(&tray_icon, elapsed, level, update_title);
            last_rendered_secs = Some(secs);
            was_recording = true;
        } else if was_recording {
            // 録音中→待機へ移った最初の tick。待機表示へ戻し、表示状態をリセットする。
            tray::set_idle(&tray_icon);
            last_rendered_secs = None;
            was_recording = false;
        }
    }
}

/// 録音セッションの有無に応じて、録音の開始／停止を切り替える。録音セッションの開始・停止と
/// メニュー項目のラベル切替に専念する。トレイアイコン／経過時間の表示はタイマー closure が
/// 録音状態（`Option<Recorder>`）を見て駆動するため、ここでは触らない。
///
/// 失敗してもアプリ（常駐）は落とさず、状態は変えずにログを残す。
fn toggle_recording(
    recorder: &mut Option<Recorder>,
    record_item: &MenuItem,
    config: &Rc<RefCell<Config>>,
) {
    if recorder.is_none() {
        start_recording(recorder, record_item, config);
    } else if let Some(session) = recorder.take() {
        // 停止。stop() が各音源のストリーム停止→flush→ファイル確定まで行い、保存できたパスを返す。
        let saved = session.stop();
        if saved.is_empty() {
            eprintln!("録音の停止・保存に失敗した（保存できたファイルが無い）");
        } else {
            // 保存先のフルパスは機微情報（録音データの所在・フォルダ構造がプライバシーに関わる）
            // なので出さない。完了が分かるように、保存できたファイル数だけを知らせる。
            println!("録音を保存した（{} ファイル）", saved.len());
        }
        record_item.set_text(RECORD_LABEL_START);
    }
}

/// 録音セッションを開始する。手動トグルと自動開始（マイク使用検知）で共用する。
///
/// 保存先は設定の現在値を使う。セッションごとに `<保存先>/<日時>` のディレクトリを作り、その中に
/// 音源（将来は文字起こしも）をまとめる。日時はローカル時刻で衝突を避ける。既に録音中なら何もしない
/// （多重開始を防ぐ）。失敗してもアプリ（常駐）は落とさず、状態は変えずにログを残す。
/// トレイアイコン／経過時間の表示はタイマー closure が録音状態を見て駆動するため、ここでは触らない。
fn start_recording(
    recorder: &mut Option<Recorder>,
    record_item: &MenuItem,
    config: &Rc<RefCell<Config>>,
) {
    if recorder.is_some() {
        return;
    }
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let session_dir = config.borrow().recording_dir.join(&timestamp);
    match Recorder::start(&session_dir) {
        Ok(session) => {
            *recorder = Some(session);
            record_item.set_text(RECORD_LABEL_STOP);
        }
        Err(err) => eprintln!("録音の開始に失敗した: {err}"),
    }
}

/// ウィンドウを表示し、トグル項目のラベルを「隠す」に切り替える。
///
/// 初回表示時のみジオメトリを明示する（`geometry_committed` で一度きりに保つ）。
/// 詳細は `WINDOW_WIDTH` などの定義コメントを参照。
fn show_window(window: &slint::Window, toggle_item: &MenuItem, geometry_committed: &mut bool) {
    if !*geometry_committed {
        window.set_position(slint::LogicalPosition::new(WINDOW_X, WINDOW_Y));
        window.set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
        *geometry_committed = true;
    }
    if let Err(err) = window.show() {
        eprintln!("ウィンドウの表示に失敗した: {err}");
    }
    toggle_item.set_text(SETTINGS_LABEL_CLOSE);
}

/// 録音中アイコンの明滅レベルを、録音経過時間からサイン波で算出する純粋関数。
///
/// `0.0`（最も暗い赤）〜`1.0`（最も明るい赤）を返す。位相はティック数ではなく経過時間
/// （`Recorder::elapsed()`）基準なので、ポーリング tick の揺れに依存せず一定周期で明滅する。
/// `cycle_secs` は 1 サイクル（明→暗→明）の秒数。位相 0 は中間（0.5）から始まる。
fn breathing_level(elapsed: std::time::Duration, cycle_secs: f32) -> f32 {
    use std::f32::consts::PI;
    let t = elapsed.as_secs_f32();
    ((2.0 * PI * t / cycle_secs).sin() + 1.0) / 2.0
}

/// 保存先パスを画面表示用の文字列に変換する。
fn recording_dir_text(dir: &std::path::Path) -> slint::SharedString {
    dir.display().to_string().into()
}

/// ウィンドウを非表示にし、トグル項目のラベルを「表示」に戻す。
fn hide_window(window: &slint::Window, toggle_item: &MenuItem) {
    if let Err(err) = window.hide() {
        eprintln!("ウィンドウの非表示に失敗した: {err}");
    }
    toggle_item.set_text(SETTINGS_LABEL_OPEN);
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

#[cfg(test)]
mod tests {
    use super::breathing_level;
    use std::time::Duration;

    /// サイン波の代表的な位相で、期待どおりの明度レベルになることを確認する。
    /// 2 秒周期なら 0s→0.5, 0.5s(1/4)→1.0, 1.0s(1/2)→0.5, 1.5s(3/4)→0.0, 2.0s(1周)→0.5。
    #[test]
    fn breathing_level_matches_sine_phases() {
        const CYCLE: f32 = 2.0;
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;

        assert!(approx(
            breathing_level(Duration::from_secs_f32(0.0), CYCLE),
            0.5
        ));
        assert!(approx(
            breathing_level(Duration::from_secs_f32(0.5), CYCLE),
            1.0
        ));
        assert!(approx(
            breathing_level(Duration::from_secs_f32(1.0), CYCLE),
            0.5
        ));
        assert!(approx(
            breathing_level(Duration::from_secs_f32(1.5), CYCLE),
            0.0
        ));
        // 1 周期後は位相が戻り、開始と同じ 0.5。
        assert!(approx(
            breathing_level(Duration::from_secs_f32(2.0), CYCLE),
            0.5
        ));
    }

    /// 返り値は常に 0.0〜1.0 の範囲に収まる（アルファ 0 に落ちる＝消えたようには見せない前提）。
    #[test]
    fn breathing_level_stays_within_unit_range() {
        const CYCLE: f32 = 2.0;
        for i in 0..=40 {
            let t = i as f32 * 0.05; // 0.00〜2.00 秒を 0.05 刻みで
            let level = breathing_level(Duration::from_secs_f32(t), CYCLE);
            assert!(
                (0.0..=1.0).contains(&level),
                "level {level} が範囲外 (t={t})"
            );
        }
    }
}
