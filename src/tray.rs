//! メニューバー／タスクバーに常駐するトレイアイコンとメニューを構築する。
//!
//! Slint 単体にはトレイ常駐の API が無いため、`tray-icon` でアイコンとメニューを担う。
//! メニュー操作のイベントは `tray_icon::menu::MenuEvent` のグローバルチャネルへ流れるので、
//! 呼び出し側（`main`）が Slint のイベントループ上でそれを拾ってウィンドウ操作・録音・終了を行う。

use std::rc::Rc;
use std::time::Duration;

use tray_icon::menu::{Icon as MenuIcon, IconMenuItem, Menu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 設定画面（ウィンドウ）を開くメニュー項目のラベル。押すとウィンドウを表示する。
/// 閉じるのはウィンドウ自身の閉じるボタンに任せる（メニューからは閉じない）ため、
/// ラベルは固定で切り替えない。
pub const SETTINGS_LABEL: &str = "Settings";

/// 録音トグル項目のラベル。START=待機中に押すと開始、STOP=録音中に押すと停止。
pub const RECORD_LABEL_START: &str = "Start Recording";
pub const RECORD_LABEL_STOP: &str = "Stop Recording";

/// トレイのツールチップ。待機中と録音中で切り替える。
const TOOLTIP_IDLE: &str = "openshoki";
const TOOLTIP_RECORDING: &str = "openshoki — Recording…";

/// メニュー項目アイコンの PNG 素材（ビルド時に埋め込む）。`assets/menu/` に置いた 32x32・8bit RGBA。
/// 実行時のファイル読み込み（`.app` の Resources パス解決）に依存させないため埋め込む
/// （`docs/CONTEXT.md`）。録音項目は状態で `record`（開始）↔`stop`（停止）を切り替える。
const RECORD_ICON_PNG: &[u8] = include_bytes!("../assets/menu/record.png");
const STOP_ICON_PNG: &[u8] = include_bytes!("../assets/menu/stop.png");
const SETTINGS_ICON_PNG: &[u8] = include_bytes!("../assets/menu/settings.png");
const QUIT_ICON_PNG: &[u8] = include_bytes!("../assets/menu/quit.png");

/// 構築したトレイ一式。`TrayIcon` はドロップするとアイコンが消えるため、
/// アプリが生きている間は保持し続ける必要がある。
pub struct Tray {
    /// トレイアイコン本体。録音状態に応じてアイコン／ツールチップを更新するため、メインスレッド上で
    /// イベントハンドラと共有する（`Rc`）。
    pub icon: Rc<TrayIcon>,
    /// 設定画面（ウィンドウ）を開く項目。ラベル・アイコンは固定（歯車）。
    pub toggle_item: IconMenuItem,
    /// 録音の開始/停止を切り替える項目。録音状態に応じてラベルとアイコンを更新する。
    pub record_item: IconMenuItem,
    /// アプリを終了する項目。
    pub quit_item: IconMenuItem,
}

impl Tray {
    /// トレイアイコンとメニューを生成して常駐させる。
    ///
    /// macOS では NSApplication の初期化後（= Slint バックエンド初期化後）に呼ぶ必要がある。
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // 各項目のアイコンは load_menu_icon で読み込む（デコード失敗時の扱いは同 doc 参照）。
        // 録音項目の待機表示（ラベル＋アイコン）は set_record_item_idle に集約しているため、
        // 初期状態もそれを通して設定し、対応の定義を 1 箇所に保つ。
        let record_item = IconMenuItem::new(RECORD_LABEL_START, true, None, None);
        set_record_item_idle(&record_item);
        let toggle_item = IconMenuItem::new(
            SETTINGS_LABEL,
            true,
            load_menu_icon(SETTINGS_ICON_PNG),
            None,
        );
        let quit_item = IconMenuItem::new("Quit", true, load_menu_icon(QUIT_ICON_PNG), None);

        let menu = Menu::new();
        menu.append(&record_item)?;
        menu.append(&toggle_item)?;
        menu.append(&quit_item)?;

        let icon = TrayIconBuilder::new()
            .with_tooltip(TOOLTIP_IDLE)
            .with_menu(Box::new(menu))
            .with_icon(dot_icon(IDLE_COLOR))
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
    if let Err(err) = icon.set_icon(Some(dot_icon(IDLE_COLOR))) {
        eprintln!("Failed to update the tray icon: {err}");
    }
    // set_title は Result を返さない。tray-icon 0.24 の macOS 実装では set_title(None) は
    // 既存タイトルを消さない no-op（button.setTitle を呼ぶ分岐をスキップする）ため、
    // 空文字を渡して NSStatusItem ボタンの経過時間テキストを確実に消す。
    icon.set_title(Some(""));
    if let Err(err) = icon.set_tooltip(Some(TOOLTIP_IDLE)) {
        eprintln!("Failed to update the tray tooltip: {err}");
    }
}

/// 録音中の表示を更新する。アイコンは明度レベル（`level`, 0.0=暗〜1.0=明）で赤の濃淡を
/// 補間し、滑らかな明滅（breathing）を表す。アイコンは滑らかさのため毎ティック更新する前提。
/// 経過時間テキストとツールチップは毎ティック再設定すると無駄なので、`update_title` が真の
/// ときだけ（＝呼び出し側で秒が変わったとき）更新する。
/// `?` を使えない呼び出し元から使うため、失敗はログに残す。
pub fn render_recording(icon: &TrayIcon, elapsed: Duration, level: f32, update_title: bool) {
    if let Err(err) = icon.set_icon(Some(dot_icon(recording_color(level)))) {
        eprintln!("Failed to update the tray icon: {err}");
    }
    if update_title {
        // set_title は Result を返さない。macOS ではメニューバーにテキスト表示される
        //（Windows/Linux では効き方が異なるが、アイコンの色・明滅を主表示にしているので許容）。
        icon.set_title(Some(format_elapsed(elapsed)));
        if let Err(err) = icon.set_tooltip(Some(TOOLTIP_RECORDING)) {
            eprintln!("Failed to update the tray tooltip: {err}");
        }
    }
}

/// 録音中ドットの色を明度レベル（0.0=暗い赤, 1.0=明るい赤）で線形補間する。透明度（アルファ）は
/// 使わず赤の濃淡だけで表すため、明滅しても「消えた」ようには見えない。
fn recording_color(level: f32) -> [u8; 4] {
    // 明滅の両端の赤。DIM を明るくしすぎない範囲で濃淡差を付ける（実機の見え方で微調整可）。
    const RECORDING_BRIGHT: [u8; 4] = [0xD0, 0x21, 0x1c, 0xff];
    const RECORDING_DIM: [u8; 4] = [0x6a, 0x14, 0x10, 0xff];

    let level = level.clamp(0.0, 1.0);
    let lerp =
        |dim: u8, bright: u8| (dim as f32 + (bright as f32 - dim as f32) * level).round() as u8;
    [
        lerp(RECORDING_DIM[0], RECORDING_BRIGHT[0]),
        lerp(RECORDING_DIM[1], RECORDING_BRIGHT[1]),
        lerp(RECORDING_DIM[2], RECORDING_BRIGHT[2]),
        0xff,
    ]
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

/// 録音項目を待機中（押すと開始）の表示にする。テキストとアイコンを対で切り替え、
/// 表示状態とラベル/アイコンの対応を 1 箇所で保証する（`docs/rules/coding-conventions.md`）。
pub fn set_record_item_idle(item: &IconMenuItem) {
    item.set_text(RECORD_LABEL_START);
    item.set_icon(load_menu_icon(RECORD_ICON_PNG));
}

/// 録音項目を録音中（押すと停止）の表示にする。`set_record_item_idle` と対。
pub fn set_record_item_recording(item: &IconMenuItem) {
    item.set_text(RECORD_LABEL_STOP);
    item.set_icon(load_menu_icon(STOP_ICON_PNG));
}

/// 埋め込み PNG を RGBA へデコードして muda の `Icon` を作る。素材は 8bit RGBA 固定
/// （`assets/menu/` 生成時に保証）。デコード失敗・想定外フォーマットは `None` を返し、呼び出し側は
/// アイコン無しで続行する（アイコンのために機能を止めない。`docs/rules/error-handling.md`）。
fn load_menu_icon(png_bytes: &[u8]) -> Option<MenuIcon> {
    // png 0.18 の Decoder は BufRead + Seek を要求する。埋め込みバイト列を Cursor で包んで渡す。
    let mut reader = match png::Decoder::new(std::io::Cursor::new(png_bytes)).read_info() {
        Ok(reader) => reader,
        Err(err) => {
            eprintln!("Skipping a menu icon because decoding its header failed: {err}");
            return None;
        }
    };
    let Some(size) = reader.output_buffer_size() else {
        eprintln!("Skipping a menu icon because its output buffer size is unavailable.");
        return None;
    };
    let mut buf = vec![0u8; size];
    let info = match reader.next_frame(&mut buf) {
        Ok(info) => info,
        Err(err) => {
            eprintln!("Skipping a menu icon because decoding its pixels failed: {err}");
            return None;
        }
    };
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        eprintln!("Skipping a menu icon because it is not 8-bit RGBA.");
        return None;
    }
    buf.truncate(info.buffer_size());
    match MenuIcon::from_rgba(buf, info.width, info.height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            eprintln!("Skipping a menu icon because building it from RGBA failed: {err}");
            None
        }
    }
}

/// 待機中ドットのグレー（不透明）。
const IDLE_COLOR: [u8; 4] = [0x8a, 0x8a, 0x8a, 0xff];

/// トレイ用のドットアイコンを、指定した RGBA 色で生成する。色（待機のグレー／録音中の赤の濃淡）は
/// 呼び出し側が決め、ここは描画に専念して共通化する。
///
/// 暫定アイコン。macOS のテンプレート画像化など見た目の調整は後続に回す。
fn dot_icon(dot: [u8; 4]) -> Icon {
    const SIZE: u32 = 32;
    // ドットの半径はアイコン一辺に対する割合で決める。
    const RADIUS_RATIO: f32 = 0.4;

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

    Icon::from_rgba(rgba, SIZE, SIZE)
        .expect("RGBA buffer length always satisfies SIZE*SIZE*4, so this is always valid")
}

#[cfg(test)]
mod tests {
    use super::{
        QUIT_ICON_PNG, RECORD_ICON_PNG, SETTINGS_ICON_PNG, STOP_ICON_PNG, format_elapsed,
        load_menu_icon, recording_color,
    };
    use std::time::Duration;

    #[test]
    fn load_menu_icon_decodes_embedded_assets() {
        // 埋め込み素材はすべて 8bit RGBA でデコードでき、Icon を作れる（素材差し替えの回帰検知）。
        for png in [
            RECORD_ICON_PNG,
            STOP_ICON_PNG,
            SETTINGS_ICON_PNG,
            QUIT_ICON_PNG,
        ] {
            assert!(
                load_menu_icon(png).is_some(),
                "embedded menu icons should decode to an icon"
            );
        }
    }

    #[test]
    fn load_menu_icon_returns_none_for_invalid_bytes() {
        // PNG として不正なバイト列はデコードに失敗し、アイコン無し（None）へ縮退する。
        assert!(load_menu_icon(&[0, 1, 2, 3]).is_none());
    }

    #[test]
    fn load_menu_icon_returns_none_for_non_rgba_png() {
        // 8bit RGB（RGBA でない）PNG を生成し、フォーマット判定で弾かれて None になることを確認する。
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, 2, 2);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder
                .write_header()
                .expect("writing the PNG header succeeds in test");
            writer
                .write_image_data(&[0u8; 2 * 2 * 3])
                .expect("writing the PNG data succeeds in test");
        }
        assert!(load_menu_icon(&bytes).is_none());
    }

    #[test]
    fn recording_color_interpolates_by_level() {
        // level 0.0=暗い赤、1.0=明るい赤、その間は線形補間。アルファは常に不透明。
        assert_eq!(recording_color(0.0), [0x6a, 0x14, 0x10, 0xff]);
        assert_eq!(recording_color(1.0), [0xD0, 0x21, 0x1c, 0xff]);
        // 中点は両端の平均（四捨五入）。
        assert_eq!(recording_color(0.5), [0x9d, 0x1b, 0x16, 0xff]);
        // 範囲外はクランプされ、消えた（アルファ 0）ようにはならない。
        assert_eq!(recording_color(-1.0), [0x6a, 0x14, 0x10, 0xff]);
        assert_eq!(recording_color(2.0), [0xD0, 0x21, 0x1c, 0xff]);
    }

    #[test]
    fn format_elapsed_under_hour_is_mm_ss() {
        assert_eq!(format_elapsed(Duration::from_secs(0)), "00:00");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "01:05");
        assert_eq!(format_elapsed(Duration::from_secs(599)), "09:59");
        // 1 時間未満の上限。ここまでは時を出さず mm:ss のまま（分は 60 以上になりうる）。
        assert_eq!(format_elapsed(Duration::from_secs(3599)), "59:59");
    }

    #[test]
    fn format_elapsed_over_hour_includes_hours() {
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "1:01:01");
        // 分は 2 桁ゼロ詰め、時は詰めない。
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1:00:00");
    }
}
