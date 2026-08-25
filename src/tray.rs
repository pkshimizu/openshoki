//! メニューバー／タスクバーに常駐するトレイアイコンとメニューを構築する。
//!
//! Slint 単体にはトレイ常駐の API が無いため、`tray-icon` でアイコンとメニューを担う。
//! メニュー操作のイベントは `tray_icon::menu::MenuEvent` のグローバルチャネルへ流れるので、
//! 呼び出し側（`main`）が Slint のイベントループ上でそれを拾ってウィンドウ操作・録音・終了を行う。

use std::rc::Rc;
use std::sync::OnceLock;
use std::time::Duration;

use tray_icon::menu::{Icon as MenuIcon, IconMenuItem, Menu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

/// 設定画面（ウィンドウ）を開くメニュー項目のラベル。押すとウィンドウを表示する。
/// 閉じるのはウィンドウ自身の閉じるボタンに任せる（メニューからは閉じない）ため、
/// ラベルは固定で切り替えない。
pub const SETTINGS_LABEL: &str = "Settings";

/// 録音一覧ウィンドウを開く項目のラベル。押すと Recordings ウィンドウを表示する（固定）。
pub const RECORDINGS_LABEL: &str = "Recordings…";

/// 録音トグル項目のラベル。START=待機中に押すと開始、STOP=録音中に押すと停止。
pub const RECORD_LABEL_START: &str = "Start Recording";
pub const RECORD_LABEL_STOP: &str = "Stop Recording";

/// トレイのツールチップ。待機中と録音中で切り替える。
const TOOLTIP_IDLE: &str = "shoki";
const TOOLTIP_RECORDING: &str = "shoki — Recording…";

/// メニュー項目アイコンの PNG 素材（ビルド時に埋め込む）。`assets/menu/` に置いた 32x32・8bit RGBA。
/// 実行時のファイル読み込み（`.app` の Resources パス解決）に依存させないため埋め込む
/// （`docs/CONTEXT.md`）。録音項目は状態で `record`（開始）↔`stop`（停止）を切り替える。
const RECORD_ICON_PNG: &[u8] = include_bytes!("../assets/menu/record.png");
const STOP_ICON_PNG: &[u8] = include_bytes!("../assets/menu/stop.png");
const SETTINGS_ICON_PNG: &[u8] = include_bytes!("../assets/menu/settings.png");
const RECORDINGS_ICON_PNG: &[u8] = include_bytes!("../assets/menu/recordings.png");
const QUIT_ICON_PNG: &[u8] = include_bytes!("../assets/menu/quit.png");

/// メニューバー常駐アイコンの PNG 素材（36x36・8bit RGBA。`scripts/generate-icons.sh` が
/// アプリアイコンと同じ SVG から生成する）。待機中は template 画像として表示し、ライト/ダークへの
/// 追従を OS に任せる。録音中は同じグリフを赤く塗って非 template で表示する。
const TRAY_ICON_PNG: &[u8] = include_bytes!("../assets/icon/tray.png");

/// 構築したトレイ一式。`TrayIcon` はドロップするとアイコンが消えるため、
/// アプリが生きている間は保持し続ける必要がある。
pub struct Tray {
    /// トレイアイコン本体。録音状態に応じてアイコン／ツールチップを更新するため、メインスレッド上で
    /// イベントハンドラと共有する（`Rc`）。
    pub icon: Rc<TrayIcon>,
    /// 設定画面（ウィンドウ）を開く項目。ラベル・アイコンは固定（歯車）。
    pub toggle_item: IconMenuItem,
    /// 録音一覧（Recordings）ウィンドウを開く項目。ラベル・アイコンは固定。
    pub recordings_item: IconMenuItem,
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
        let recordings_item = IconMenuItem::new(
            RECORDINGS_LABEL,
            true,
            load_menu_icon(RECORDINGS_ICON_PNG),
            None,
        );
        let quit_item = IconMenuItem::new("Quit", true, load_menu_icon(QUIT_ICON_PNG), None);

        let menu = Menu::new();
        menu.append(&record_item)?;
        menu.append(&toggle_item)?;
        menu.append(&recordings_item)?;
        menu.append(&quit_item)?;

        // 待機中は template 画像（アルファだけを使うモノクロ）。メニューバーの明暗・Reduce
        // Transparency への追従は OS に任せる。
        let mut builder = TrayIconBuilder::new()
            .with_tooltip(TOOLTIP_IDLE)
            .with_menu(Box::new(menu))
            .with_icon_as_template(true);
        match tray_icon(idle_tint()) {
            Some(icon) => builder = builder.with_icon(icon),
            // アイコンが無いとステータス項目が幅ゼロになり、メニュー（＝このアプリ唯一の操作口）を
            // 開けなくなる。製品名をテキストで出してクリックできる状態を必ず残す
            // （Windows はタイトル非対応なので、そこでは縮退しきれない）。
            None => builder = builder.with_title(TOOLTIP_IDLE),
        }
        let icon = builder.build()?;

        Ok(Self {
            icon: Rc::new(icon),
            toggle_item,
            recordings_item,
            record_item,
            quit_item,
        })
    }
}

/// 待機中の表示へ戻す。モノクロの glyph（macOS は template 表示）・経過時間テキストの消去・
/// ツールチップを既定に戻す。
/// `?` を使えない呼び出し元（イベントループのコールバック）から使うため、失敗はログに残す。
pub fn set_idle(icon: &TrayIcon) {
    set_tray_glyph(icon, idle_tint(), true);
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
    // 録音中は色を見せたいので template を外す（template はアルファだけを使い、赤が黒になる）。
    set_tray_glyph(icon, Some(recording_color(level)), false);
    if update_title {
        // set_title は Result を返さない。macOS ではメニューバーにテキスト表示される
        //（Windows/Linux では効き方が異なるが、アイコンの色・明滅を主表示にしているので許容）。
        icon.set_title(Some(format_elapsed(elapsed)));
        if let Err(err) = icon.set_tooltip(Some(TOOLTIP_RECORDING)) {
            eprintln!("Failed to update the tray tooltip: {err}");
        }
    }
}

/// 録音中グリフの色（RGB）を明度レベル（0.0=暗い赤, 1.0=明るい赤）で線形補間する。透明度は
/// 使わず赤の濃淡だけで表すため、明滅しても「消えた」ようには見えない。
fn recording_color(level: f32) -> [u8; 3] {
    // 明滅の両端の赤。DIM を明るくしすぎない範囲で濃淡差を付ける（実機の見え方で微調整可）。
    const RECORDING_BRIGHT: [u8; 3] = [0xD0, 0x21, 0x1c];
    const RECORDING_DIM: [u8; 3] = [0x6a, 0x14, 0x10];

    let level = level.clamp(0.0, 1.0);
    let lerp =
        |dim: u8, bright: u8| (dim as f32 + (bright as f32 - dim as f32) * level).round() as u8;
    [
        lerp(RECORDING_DIM[0], RECORDING_BRIGHT[0]),
        lerp(RECORDING_DIM[1], RECORDING_BRIGHT[1]),
        lerp(RECORDING_DIM[2], RECORDING_BRIGHT[2]),
    ]
}

/// 経過時間を表示用文字列にする（`mm:ss` / 1 時間以上は `h:mm:ss`）。録音中のメニューバー
/// 表示と Recordings の再生時間表示で共用する。
///
/// **実装は `crate::reading_pane::format_elapsed`**。読む領域も同じ表記を使うので（#164 の
/// 「どこまで読めたか」）、あちらへ寄せてある——理由はそちらの doc。ここは呼び名を変えない
/// ための再エクスポート。
pub use crate::reading_pane::format_elapsed;

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

/// 埋め込み PNG を RGBA へデコードして muda の `Icon`（メニュー項目用）を作る。
/// デコード失敗・想定外フォーマットは `None` を返し、呼び出し側はアイコン無しで続行する
/// （アイコンのために機能を止めない。`docs/rules/error-handling.md`）。
fn load_menu_icon(png_bytes: &[u8]) -> Option<MenuIcon> {
    let image = decode_rgba_png(png_bytes, "a menu icon")?;
    match MenuIcon::from_rgba(image.pixels, image.width, image.height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            eprintln!("Skipping a menu icon because building it from RGBA failed: {err}");
            None
        }
    }
}

/// メニューバーのグリフを差し替える。`tint` はグリフの塗り（`None` は素材のまま）、
/// `as_template` は macOS の template 指定。
///
/// **順序が重要**: `set_icon` は macOS で NSImage を作り直し、その際 template を必ず false に
/// する（tray-icon 0.24 の実装）。先に `set_icon_as_template` を呼んでも捨てられるため、
/// 差し替えた**後**に指定する。`set_icon_as_template` は macOS 以外では no-op。
/// 一発で両方を設定する `set_icon_with_as_template` は macOS 以外で**アイコンごと無視される**
/// no-op なので使わない（Windows / Linux でアイコンが更新されなくなる）。
fn set_tray_glyph(icon: &TrayIcon, tint: Option<[u8; 3]>, as_template: bool) {
    if let Some(glyph) = tray_icon(tint)
        && let Err(err) = icon.set_icon(Some(glyph))
    {
        eprintln!("Failed to update the tray icon: {err}");
        return;
    }
    icon.set_icon_as_template(as_template);
}

/// 待機中のグリフの塗り。macOS は template 表示（アルファだけを使い、色は OS が決める）なので
/// 塗らない。template の無いプラットフォームでは素材の黒のままだと暗いタスクバーで見えないため、
/// 明暗どちらでも見える中間グレーで塗る。
fn idle_tint() -> Option<[u8; 3]> {
    #[cfg(target_os = "macos")]
    {
        None
    }
    #[cfg(not(target_os = "macos"))]
    {
        Some([0x8a, 0x8a, 0x8a])
    }
}

/// メニューバー常駐アイコンを作る。`tint` が `None` なら素材そのまま（待機中の template 用。
/// macOS はアルファだけを見る）、`Some(color)` ならグリフをその色で塗る（録音中の赤）。
/// 素材のデコードに失敗している場合は `None`（アイコン無しで続行）。
fn tray_icon(tint: Option<[u8; 3]>) -> Option<Icon> {
    let glyph = tray_glyph()?;
    let pixels = match tint {
        Some(color) => tinted_glyph(&glyph.pixels, color),
        None => glyph.pixels.clone(),
    };
    match Icon::from_rgba(pixels, glyph.width, glyph.height) {
        Ok(icon) => Some(icon),
        Err(err) => {
            eprintln!("Skipping the tray icon because building it from RGBA failed: {err}");
            None
        }
    }
}

/// トレイアイコンの素材（デコード済み RGBA）。録音中は毎ティック塗り替えるため、デコードは
/// 起動時 1 回だけ行って使い回す（デコード失敗も一度きり記録し、以後はアイコン無しで続ける）。
fn tray_glyph() -> Option<&'static RgbaImage> {
    static GLYPH: OnceLock<Option<RgbaImage>> = OnceLock::new();
    GLYPH
        .get_or_init(|| decode_rgba_png(TRAY_ICON_PNG, "the tray icon"))
        .as_ref()
}

/// グリフを 1 色（RGB）で塗り直す。**アルファは素材のまま残す**ので、縁のアンチエイリアスが
/// 保たれ、透明な画素は透明のままになる（塗りにアルファを持たせない＝消えた表示を作れない）。
fn tinted_glyph(pixels: &[u8], color: [u8; 3]) -> Vec<u8> {
    // RGBA の 4 バイトずつ見る（端数は捨てる。`.0` だけを使う）。
    pixels
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|pixel| {
            let alpha = pixel[3];
            if alpha == 0 {
                [0, 0, 0, 0]
            } else {
                [color[0], color[1], color[2], alpha]
            }
        })
        .collect()
}

/// デコード済みの RGBA 画像。
struct RgbaImage {
    pixels: Vec<u8>,
    width: u32,
    height: u32,
}

/// 埋め込み PNG を 8bit RGBA へデコードする。素材は 8bit RGBA 固定
/// （`assets/menu/` と `scripts/generate-icons.sh` の生成時に保証）。失敗と想定外フォーマットは
/// `what`（何の素材か）を添えてログし、`None` を返す。
fn decode_rgba_png(png_bytes: &[u8], what: &str) -> Option<RgbaImage> {
    // png 0.18 の Decoder は BufRead + Seek を要求する。埋め込みバイト列を Cursor で包んで渡す。
    let mut reader = match png::Decoder::new(std::io::Cursor::new(png_bytes)).read_info() {
        Ok(reader) => reader,
        Err(err) => {
            eprintln!("Skipping {what} because decoding its header failed: {err}");
            return None;
        }
    };
    let Some(size) = reader.output_buffer_size() else {
        eprintln!("Skipping {what} because its output buffer size is unavailable.");
        return None;
    };
    let mut pixels = vec![0u8; size];
    let info = match reader.next_frame(&mut pixels) {
        Ok(info) => info,
        Err(err) => {
            eprintln!("Skipping {what} because decoding its pixels failed: {err}");
            return None;
        }
    };
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        eprintln!("Skipping {what} because it is not 8-bit RGBA.");
        return None;
    }
    pixels.truncate(info.buffer_size());
    Some(RgbaImage {
        pixels,
        width: info.width,
        height: info.height,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        QUIT_ICON_PNG, RECORD_ICON_PNG, SETTINGS_ICON_PNG, STOP_ICON_PNG, TRAY_ICON_PNG,
        decode_rgba_png, load_menu_icon, recording_color, tinted_glyph,
    };

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

    /// メニューバー用の素材が 8bit RGBA でデコードでき、`tray-icon` が 18pt 表示に使う 2x
    /// （36x36）であること。サイズが変わるとメニューバーで拡大・縮小されてぼやける。
    #[test]
    fn tray_icon_asset_is_36px_rgba() {
        let glyph = decode_rgba_png(TRAY_ICON_PNG, "the tray icon in test")
            .expect("the embedded tray icon should decode as 8-bit RGBA");
        assert_eq!(
            (glyph.width, glyph.height),
            (36, 36),
            "the tray glyph must stay 36x36 (2x of the 18pt menu bar height)"
        );
        // グリフが実際に描かれている（不透明画素がある）こと。空の素材を埋め込む事故を防ぐ。
        assert!(
            glyph
                .pixels
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] > 0),
            "the tray glyph must contain visible pixels"
        );
    }

    /// 塗り替えの基本: 不透明画素は指定色になり、透明画素は透明のまま。半透明の縁は
    /// アルファを保って色だけ変わる（アンチエイリアスを潰さない）。
    #[test]
    fn tinted_glyph_recolors_only_visible_pixels() {
        // 4 画素: 不透明の黒 / 半透明の黒 / 完全な透明 / 不透明の白。
        let pixels = [
            0x00, 0x00, 0x00, 0xff, //
            0x00, 0x00, 0x00, 0x80, //
            0x00, 0x00, 0x00, 0x00, //
            0xff, 0xff, 0xff, 0xff,
        ];
        let red = [0xD0, 0x21, 0x1c];
        assert_eq!(
            tinted_glyph(&pixels, red),
            vec![
                0xD0, 0x21, 0x1c, 0xff, // 不透明はそのまま赤に
                0xD0, 0x21, 0x1c, 0x80, // 半透明はアルファを保って赤に
                0x00, 0x00, 0x00, 0x00, // 透明は触らない
                0xD0, 0x21, 0x1c, 0xff, // 元の色に関係なく赤にする
            ]
        );
    }

    /// 実素材を塗ると、見えている画素はすべて指定色になり、形（アルファ）は変わらない。
    #[test]
    fn tinted_glyph_recolors_the_real_asset_without_changing_its_shape() {
        let glyph = decode_rgba_png(TRAY_ICON_PNG, "the tray icon in test")
            .expect("the embedded tray icon should decode as 8-bit RGBA");
        let red = recording_color(1.0);
        let tinted = tinted_glyph(&glyph.pixels, red);
        assert_eq!(tinted.len(), glyph.pixels.len());

        let mut recolored = 0usize;
        for (source, painted) in glyph
            .pixels
            .as_chunks::<4>()
            .0
            .iter()
            .zip(tinted.as_chunks::<4>().0.iter())
        {
            assert_eq!(painted[3], source[3], "the alpha channel must be preserved");
            if source[3] == 0 {
                assert_eq!(*painted, [0, 0, 0, 0], "transparent pixels must stay empty");
            } else {
                assert_eq!(
                    &painted[..3],
                    &red,
                    "every visible pixel must take the recording color"
                );
                recolored += 1;
            }
        }
        // 素材が空だったり、塗り替えが恒等関数になっていたら気づけるようにする。
        assert!(
            recolored > 0,
            "the asset must have visible pixels to recolor"
        );
        assert_ne!(
            tinted, glyph.pixels,
            "tinting must actually change the pixels"
        );
    }

    #[test]
    fn recording_color_interpolates_by_level() {
        // level 0.0=暗い赤、1.0=明るい赤、その間は線形補間（アルファの保持は tinted_glyph の担当）。
        assert_eq!(recording_color(0.0), [0x6a, 0x14, 0x10]);
        assert_eq!(recording_color(1.0), [0xD0, 0x21, 0x1c]);
        // 中点は両端の平均（四捨五入）。
        assert_eq!(recording_color(0.5), [0x9d, 0x1b, 0x16]);
        // 範囲外はクランプされる（色だけで表すのでアルファは持たない）。
        assert_eq!(recording_color(-1.0), [0x6a, 0x14, 0x10]);
        assert_eq!(recording_color(2.0), [0xD0, 0x21, 0x1c]);
    }
}
