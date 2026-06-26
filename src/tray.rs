//! メニューバー／タスクバーに常駐するトレイアイコンとメニューを構築する。
//!
//! Slint 単体にはトレイ常駐の API が無いため、`tray-icon` でアイコンとメニューを担う。
//! メニュー操作のイベントは `tray_icon::menu::MenuEvent` のグローバルチャネルへ流れるので、
//! 呼び出し側（`main`）が Slint のイベントループ上でそれを拾ってウィンドウ操作・終了を行う。

use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 構築したトレイ一式。`TrayIcon` はドロップするとアイコンが消えるため、
/// アプリが生きている間は保持し続ける必要がある。
pub struct Tray {
    // 保持専用。明示的に参照しないが、ドロップさせないために持っておく。
    _icon: TrayIcon,
    /// ウィンドウの表示/非表示を切り替える項目。表示状態に応じてラベルを更新する。
    pub toggle_item: MenuItem,
    /// アプリを終了する項目。
    pub quit_item: MenuItem,
}

impl Tray {
    /// トレイアイコンとメニューを生成して常駐させる。
    ///
    /// macOS では NSApplication の初期化後（= Slint バックエンド初期化後）に呼ぶ必要がある。
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let toggle_item = MenuItem::new("ウィンドウを表示", true, None);
        let quit_item = MenuItem::new("終了", true, None);

        let menu = Menu::new();
        menu.append(&toggle_item)?;
        menu.append(&quit_item)?;

        let icon = TrayIconBuilder::new()
            .with_tooltip("openshoki")
            .with_menu(Box::new(menu))
            .with_icon(record_icon())
            .build()?;

        Ok(Self {
            _icon: icon,
            toggle_item,
            quit_item,
        })
    }
}

/// トレイ用のアイコンを生成する。録音アプリらしく赤い録音ドットを描く。
///
/// 暫定アイコン。macOS のテンプレート画像化など見た目の調整は後続に回す。
fn record_icon() -> Icon {
    const SIZE: u32 = 32;
    // 赤い録音ドットの色（不透明）。
    const DOT: [u8; 4] = [0xD0, 0x21, 0x1c, 0xff];
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
                rgba[offset..offset + 4].copy_from_slice(&DOT);
            }
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).expect("RGBA バッファ長 = SIZE*SIZE*4 を満たすため常に有効")
}
