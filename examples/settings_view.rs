//! 設定ウィンドウ（Auto-Record セクション）の描画確認用バイナリ
//! （`docs/rules/slint.md` の検証手順）。ダミーの登録アプリを流し込んで AppWindow を表示する。
//!
//! Trigger apps の一覧は**固定高さ・clip 付き**の箱なので、折り返す注記を入れると潰れやすい。
//! ビルドやテストバックエンドでは検出できないため、ここで目視する。
//!
//! ```sh
//! cargo run --example settings_view                    # 表示して screencapture で確認
//! cargo run --example settings_view -- snapshot out.png # PNG に書き出して確認
//! ```
//!
//! `snapshot` を使うと、画面収録の許可が無い環境でも見た目を確認できる。

slint::include_modules!();

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

/// 注記つきの行がどれだけ縦を食うかを見たいので、対象外アプリを混ぜた並びを既定にする。
fn sample_apps() -> Vec<TriggerApp> {
    let note = "Not detected — record manually";
    vec![
        TriggerApp {
            name: "Google Chrome".into(),
            limitation_note: "".into(),
        },
        TriggerApp {
            name: "Safari".into(),
            limitation_note: note.into(),
        },
        TriggerApp {
            name: "zoom.us".into(),
            limitation_note: "".into(),
        },
        TriggerApp {
            name: "Slack".into(),
            limitation_note: "".into(),
        },
    ]
}

fn main() {
    let win =
        AppWindow::new().expect("creating the window should succeed in this verification binary");

    win.set_recording_dir("/Users/example/Recordings".into());
    win.set_auto_record_app(true);
    win.set_auto_stop_debounce_secs(4);
    win.set_app_list(ModelRc::from(Rc::new(VecModel::from(sample_apps()))));

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    win.show()
        .expect("showing the window should succeed in this verification binary");

    // `snapshot <path>`: 最初のフレームが描かれてから書き出す。ループ開始前だと中身が空になる。
    let snapshot_path = std::env::args()
        .skip_while(|arg| arg != "snapshot")
        .nth(1)
        .map(std::path::PathBuf::from);
    let timer = slint::Timer::default();
    if let Some(path) = snapshot_path {
        let handle = win.as_weak();
        timer.start(
            slint::TimerMode::SingleShot,
            std::time::Duration::from_millis(500),
            move || {
                if let Some(win) = handle.upgrade() {
                    write_snapshot(&win, &path);
                }
                slint::quit_event_loop().expect("quitting the event loop should succeed");
            },
        );
    }

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}

fn write_snapshot(win: &AppWindow, path: &std::path::Path) {
    let buffer = match win.window().take_snapshot() {
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
    let write = encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(buffer.as_bytes()));
    match write {
        Ok(()) => println!("Wrote {}", path.display()),
        Err(err) => eprintln!("Could not write {}: {err}", path.display()),
    }
}
