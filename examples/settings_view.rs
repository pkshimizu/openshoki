//! 設定ウィンドウの描画確認用バイナリ
//! （`docs/rules/slint.md` の検証手順）。ダミーの登録アプリを流し込んで AppWindow を表示する。
//!
//! 監視アプリ（Watched apps）の一覧は注記が折り返すので、行の高さが伸びて下が詰まりやすい。
//! ビルドやテストバックエンドでは検出できないため、ここで目視する。
//!
//! ```sh
//! cargo run --example settings_view                    # 表示して screencapture で確認
//! cargo run --example settings_view -- snapshot out.png # PNG に書き出して確認
//! ```
//!
//! `snapshot` を使うと、画面収録の許可が無い環境でも見た目を確認できる。

slint::include_modules!();

#[path = "verification/snapshot.rs"]
mod snapshot;

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

/// 注記つきの行がどれだけ縦を食うかを見たいので、対象外アプリを混ぜた並びを既定にする。
fn sample_apps() -> Vec<TriggerApp> {
    // `app_audio_monitor::auto_record_limitation` の文言の複製（bin クレートなので import
    // できない）。あちらを変えたらここも合わせること。長さが見え方に効くのが確認の主目的。
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
    // 引数は 1 度だけ集める（フラグごとに走査しない）。
    let args: Vec<String> = std::env::args().collect();
    let win =
        AppWindow::new().expect("creating the window should succeed in this verification binary");

    // `src/main.rs` の `app_version_text` の複製（bin クレートなので import できない）。
    // あちらを変えたらここも合わせること。
    win.set_app_version(format!("shoki v{}", env!("CARGO_PKG_VERSION")).into());
    win.set_recording_dir("/Users/example/Recordings".into());
    win.set_auto_record_app(true);
    win.set_auto_stop_debounce_secs(4);
    // 引数に `empty-apps` を渡すと監視アプリ 0 件（空状態の文言）を見られる。
    let empty_apps = args.iter().any(|arg| arg == "empty-apps");
    win.set_app_list(ModelRc::from(Rc::new(VecModel::from(if empty_apps {
        Vec::new()
    } else {
        sample_apps()
    }))));
    // 扉（#141）。文言は `src/windows/transcription.rs` / `minutes.rs` の複製で、**いちばん
    // 長くなる形**で見る: 構成行は言語・モデル名・件数の 3 つ、状態行は取得中と「待っている
    // 理由」（いずれも折り返しやすい）。
    win.set_auto_transcribe(true);
    win.set_transcription_state("On".into());
    win.set_transcription_summary("Japanese · Medium (1.5 GB) · 2 of 6 models downloaded".into());
    win.set_transcription_status("Large v3 downloading — 62%".into());
    win.set_transcription_tone(StatusTone::Active);
    win.set_auto_summarize(true);
    win.set_minutes_state("On".into());
    win.set_minutes_summary("Qwen2.5 7B Instruct (4.4 GB) · 1 of 2 models downloaded".into());
    win.set_minutes_status("Waits for a transcript — automatic transcription is off".into());
    win.set_minutes_tone(StatusTone::Caution);

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    // 実アプリと同じ寸法で見る（`src/main.rs` の WINDOW_WIDTH/HEIGHT と一致させること）。
    // 引数に `tall` を渡すと、スクロールせずに本文の全長を 1 枚で見られる高さにする
    // （末尾の議事録ブロック・注記・バージョンまで確認するため）。
    // `min` を渡すと最小サイズ（`ui/app-window.slint` の min-width/height）で見る。
    // 寸法は 1 つの分岐で決める（幅と高さを別々に判定すると片方だけ直す事故になる）。
    let (width, height) = if args.iter().any(|arg| arg == "min") {
        (420.0, 520.0) // `ui/app-window.slint` の min-width / min-height
    } else if args.iter().any(|arg| arg == "tall") {
        (460.0, 1560.0) // 本文の全長（画面より大きいと OS 側で切り詰められる）
    } else {
        (460.0, 900.0) // 実アプリの標準（`src/main.rs` の WINDOW_WIDTH/HEIGHT）
    };
    win.window()
        .set_size(slint::LogicalSize::new(width, height));
    // 本文は画面の高さより長いので、下端（議事録ブロック・注記・バージョン）は `bottom` を
    // 渡してスクロールさせてから撮る。
    if args.iter().any(|arg| arg == "bottom") {
        win.set_body_scroll(-620.0);
    }
    win.show()
        .expect("showing the window should succeed in this verification binary");

    // `snapshot <path>` が指定されていれば 1 フレーム後に PNG を書く（`snapshot` モジュール）。
    let _snapshot_timer = snapshot::arm(win.as_weak());

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}
