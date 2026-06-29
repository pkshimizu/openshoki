//! メニューバー／タスクバーに常駐するトレイアイコンとメニューを構築する。
//!
//! Slint 単体にはトレイ常駐の API が無いため、`tray-icon` でアイコンとメニューを担う。
//! メニュー操作のイベントは `tray_icon::menu::MenuEvent` のグローバルチャネルへ流れるので、
//! 呼び出し側（`main`）が Slint のイベントループ上でそれを拾ってウィンドウ操作・録音・終了を行う。

use std::rc::Rc;
use std::time::Duration;

use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 設定画面トグル項目のラベル。可視状態と表示文言が食い違わないよう、文字列を直書きで
/// 散らさず、初期値・更新の双方からこの定数を参照する。OPEN=非表示時に押すと開く、
/// CLOSE=表示時に押すと閉じる（=ウィンドウを隠す）。
pub const SETTINGS_LABEL_OPEN: &str = "設定を開く";
pub const SETTINGS_LABEL_CLOSE: &str = "設定を閉じる";

/// 録音トグル項目のラベル。START=待機中に押すと開始、STOP=録音中に押すと停止。
pub const RECORD_LABEL_START: &str = "録音を開始";
pub const RECORD_LABEL_STOP: &str = "録音を停止";

/// トレイのツールチップ。待機中と録音中で切り替える。
const TOOLTIP_IDLE: &str = "openshoki";
const TOOLTIP_RECORDING: &str = "openshoki — 録音中…";

/// 構築したトレイ一式。`TrayIcon` はドロップするとアイコンが消えるため、
/// アプリが生きている間は保持し続ける必要がある。
pub struct Tray {
    /// トレイアイコン本体。録音状態に応じてアイコン／ツールチップを更新するため、メインスレッド上で
    /// イベントハンドラと共有する（`Rc`）。
    pub icon: Rc<TrayIcon>,
    /// 設定画面（ウィンドウ）の表示/非表示を切り替える項目。表示状態に応じてラベルを更新する。
    pub toggle_item: MenuItem,
    /// 録音の開始/停止を切り替える項目。録音状態に応じてラベルを更新する。
    pub record_item: MenuItem,
    /// アプリを終了する項目。
    pub quit_item: MenuItem,
}

impl Tray {
    /// トレイアイコンとメニューを生成して常駐させる。
    ///
    /// macOS では NSApplication の初期化後（= Slint バックエンド初期化後）に呼ぶ必要がある。
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let record_item = MenuItem::new(RECORD_LABEL_START, true, None);
        let toggle_item = MenuItem::new(SETTINGS_LABEL_OPEN, true, None);
        let quit_item = MenuItem::new("終了", true, None);

        let menu = Menu::new();
        menu.append(&record_item)?;
        menu.append(&toggle_item)?;
        menu.append(&quit_item)?;

        let icon = TrayIconBuilder::new()
            .with_tooltip(TOOLTIP_IDLE)
            .with_menu(Box::new(menu))
            .with_icon(dot_icon(DotColor::Idle))
            .build()?;

        Ok(Self {
            icon: Rc::new(icon),
            toggle_item,
            record_item,
            quit_item,
        })
    }
}

/// 待機中の表示へ戻す。静的なグレーアイコン・経過時間テキストの消去・ツールチップを既定に戻す。
/// `?` を使えない呼び出し元（イベントループのコールバック）から使うため、失敗はログに残す。
pub fn set_idle(icon: &TrayIcon) {
    if let Err(err) = icon.set_icon(Some(dot_icon(DotColor::Idle))) {
        eprintln!("トレイアイコンの更新に失敗した: {err}");
    }
    // set_title は Result を返さない。None で経過時間テキストを消す。
    icon.set_title(None::<&str>);
    if let Err(err) = icon.set_tooltip(Some(TOOLTIP_IDLE)) {
        eprintln!("トレイのツールチップ更新に失敗した: {err}");
    }
}

/// 録音中の表示を更新する。点滅フレーム（`blink_on`）で赤の濃淡を切り替え、メニューバーに
/// 経過時間テキストを出す。呼び出し側が表示の変化時（秒の更新・点滅トグル）にだけ呼ぶ前提。
/// `?` を使えない呼び出し元から使うため、失敗はログに残す。
pub fn render_recording(icon: &TrayIcon, elapsed: Duration, blink_on: bool) {
    let color = if blink_on {
        DotColor::Recording
    } else {
        DotColor::RecordingDim
    };
    if let Err(err) = icon.set_icon(Some(dot_icon(color))) {
        eprintln!("トレイアイコンの更新に失敗した: {err}");
    }
    // set_title は Result を返さない。macOS ではメニューバーにテキスト表示される
    //（Windows/Linux では効き方が異なるが、アイコンの色・点滅を主表示にしているので許容）。
    icon.set_title(Some(format_elapsed(elapsed)));
    if let Err(err) = icon.set_tooltip(Some(TOOLTIP_RECORDING)) {
        eprintln!("トレイのツールチップ更新に失敗した: {err}");
    }
}

/// 経過時間を表示用文字列にする。既定は `mm:ss`、1 時間以上は `h:mm:ss`。
fn format_elapsed(elapsed: Duration) -> String {
    const SECS_PER_MINUTE: u64 = 60;
    const SECS_PER_HOUR: u64 = 60 * SECS_PER_MINUTE;

    let total = elapsed.as_secs();
    let hours = total / SECS_PER_HOUR;
    let minutes = (total % SECS_PER_HOUR) / SECS_PER_MINUTE;
    let seconds = total % SECS_PER_MINUTE;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// ドットアイコンの色。待機中はグレー、録音中は赤（点滅の減光フレームは暗い赤）。
#[derive(Clone, Copy)]
enum DotColor {
    Idle,
    Recording,
    RecordingDim,
}

/// トレイ用のドットアイコンを生成する。録音中は赤（点滅で濃淡）、待機中はグレーで状態を示す。
///
/// 暫定アイコン。macOS のテンプレート画像化など見た目の調整は後続に回す。
fn dot_icon(color: DotColor) -> Icon {
    const SIZE: u32 = 32;
    // 不透明なドット色（RGBA）。録音中は明るい赤と、点滅の減光フレーム用の暗い赤。
    // 透明にはせず減光に留め、点滅で「消えた」ように見えないようにする。
    const RECORDING: [u8; 4] = [0xD0, 0x21, 0x1c, 0xff];
    const RECORDING_DIM: [u8; 4] = [0x6a, 0x14, 0x10, 0xff];
    const IDLE: [u8; 4] = [0x8a, 0x8a, 0x8a, 0xff];
    // ドットの半径はアイコン一辺に対する割合で決める。
    const RADIUS_RATIO: f32 = 0.4;

    let dot = match color {
        DotColor::Idle => IDLE,
        DotColor::Recording => RECORDING,
        DotColor::RecordingDim => RECORDING_DIM,
    };

    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let center = (SIZE as f32 - 1.0) / 2.0;
    let radius = SIZE as f32 * RADIUS_RATIO;
    let radius_sq = radius * radius;

    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            if dx * dx + dy * dy <= radius_sq {
                let offset = ((y * SIZE + x) * 4) as usize;
                rgba[offset..offset + 4].copy_from_slice(&dot);
            }
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).expect("RGBA バッファ長 = SIZE*SIZE*4 を満たすため常に有効")
}

#[cfg(test)]
mod tests {
    use super::format_elapsed;
    use std::time::Duration;

    #[test]
    fn format_elapsed_under_hour_is_mm_ss() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "00:00");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "01:05");
        assert_eq!(format_elapsed(Duration::from_secs(599)), "09:59");
    }

    #[test]
    fn format_elapsed_over_hour_includes_hours() {
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "1:01:01");
        // 分は 2 桁ゼロ詰め、時は詰めない。
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1:00:00");
    }
}
