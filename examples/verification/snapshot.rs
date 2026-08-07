//! 確認用バイナリ（`examples/*.rs`）が見た目を PNG に書き出すための共通処理。
//!
//! 画面収録（TCC）の許可が無い環境では `screencapture` が使えないため、イベントループ開始後に
//! `Window::take_snapshot()` して PNG を書く経路を持たせる（`docs/rules/slint.md` の検証手順）。
//! **ループ開始前に撮ると中身が空になる**ので、`Timer` で 1 フレーム後に撮ること——この待ちと
//! PNG のエンコード設定が各 example に散らないよう、ここへ集約している。
//!
//! 各 example から `#[path = "verification/snapshot.rs"] mod snapshot;` で読み込む
//! （`examples/` 直下に置くと example として個別にビルドされてしまう）。

use std::path::{Path, PathBuf};

/// 最初のフレームが描かれるまでの待ち（`take_snapshot` が空を返さないようにする）。
const FIRST_FRAME_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

/// 引数から `snapshot <path>` を読む。指定が無ければ `None`（通常の表示のみ）。
pub fn requested_path() -> Option<PathBuf> {
    std::env::args()
        .skip_while(|arg| arg != "snapshot")
        .nth(1)
        .map(PathBuf::from)
}

/// `snapshot <path>` が指定されていれば、1 フレーム後に PNG を書いてイベントループを終える
/// タイマーを仕掛ける（戻り値の `Timer` は呼び出し側で保持すること。drop すると発火しない）。
#[must_use = "the timer must be kept alive until the event loop runs"]
pub fn arm(window: slint::Weak<impl slint::ComponentHandle + 'static>) -> slint::Timer {
    let timer = slint::Timer::default();
    if let Some(path) = requested_path() {
        timer.start(slint::TimerMode::SingleShot, FIRST_FRAME_DELAY, move || {
            if let Some(component) = window.upgrade() {
                write(component.window(), &path);
            }
            slint::quit_event_loop().expect("quitting the event loop should succeed");
        });
    }
    timer
}

/// ウィンドウの現在の見た目を PNG として `path` へ書く。失敗しても確認用バイナリを落とさず
/// ログに留める（撮れなかったことが分かれば足りる）。
fn write(window: &slint::Window, path: &Path) {
    let buffer = match window.take_snapshot() {
        Ok(buffer) => buffer,
        Err(err) => {
            eprintln!("Could not take a snapshot: {err}");
            return;
        }
    };
    let file = match std::fs::File::create(path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!("Could not create {}: {err}", path.display());
            return;
        }
    };
    let mut encoder = png::Encoder::new(
        std::io::BufWriter::new(file),
        buffer.width(),
        buffer.height(),
    );
    encoder.set_color(png::ColorType::Rgba);
    let written = encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(buffer.as_bytes()));
    match written {
        Ok(()) => println!("Wrote {}", path.display()),
        Err(err) => eprintln!("Could not write {}: {err}", path.display()),
    }
}
