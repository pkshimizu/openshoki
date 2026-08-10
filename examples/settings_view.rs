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
    let win =
        AppWindow::new().expect("creating the window should succeed in this verification binary");

    // `src/main.rs` の `app_version_text` の複製（bin クレートなので import できない）。
    // あちらを変えたらここも合わせること。
    win.set_app_version(format!("shoki v{}", env!("CARGO_PKG_VERSION")).into());
    win.set_recording_dir("/Users/example/Recordings".into());
    win.set_auto_record_app(true);
    win.set_auto_stop_debounce_secs(4);
    // 引数に `empty-apps` を渡すと監視アプリ 0 件（空状態の文言）を見られる。
    let empty_apps = std::env::args().any(|arg| arg == "empty-apps");
    win.set_app_list(ModelRc::from(Rc::new(VecModel::from(if empty_apps {
        Vec::new()
    } else {
        sample_apps()
    }))));
    // Transcription セクションは従属コントロールが多く、下端の余白を食い潰しやすい。
    // 状態行は実アプリで出る最長の文言（未取得＋サイズ）で見る。
    win.set_auto_transcribe(true);
    win.set_transcribe_languages(ModelRc::from(Rc::new(VecModel::from(vec![
        slint::SharedString::from("Japanese"),
    ]))));
    win.set_whisper_models(ModelRc::from(Rc::new(VecModel::from(vec![
        slint::SharedString::from("Small — 465 MB — balanced speed and accuracy"),
    ]))));
    // 状態行は「取得中」で見る（ドット・注意色・進捗バーが同時に出る、いちばん背の高い形）。
    win.set_whisper_model_status("Downloading… 62%".into());
    win.set_whisper_model_tone(StatusTone::Active);
    win.set_whisper_model_progress(0.62);
    // 要約 LLM は選択肢・説明行（選択中の選択肢の全文）・状態行の 3 段。いちばん背が高くなる
    // 組み合わせで見る: 議事録要約は OFF（状態行が取得の契機を説明する長い文言になる）＋
    // 説明が最長の 3B を選択中（`src/summary_model.rs` の description の複製。あちらを
    // 変えたらここも合わせること）。
    win.set_auto_summarize(false);
    win.set_summary_models(ModelRc::from(Rc::new(VecModel::from(vec![
        slint::SharedString::from(
            "Qwen2.5 3B Instruct — 2.0 GB — 25 s and 3.7 GB of memory for a 4-min meeting, but can invent details",
        ),
    ]))));
    win.set_summary_model_status(
        "Not downloaded — downloads when minutes are generated (2.0 GB)".into(),
    );
    win.set_summary_model_tone(StatusTone::Neutral);

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    // 実アプリと同じ寸法で見る（`src/main.rs` の WINDOW_WIDTH/HEIGHT と一致させること）。
    // 引数に `tall` を渡すと、スクロールせずに本文の全長を 1 枚で見られる高さにする
    // （末尾の議事録ブロック・注記・バージョンまで確認するため）。
    // `min` を渡すと最小サイズ（`ui/app-window.slint` の min-width/height）で見る。
    let height = if std::env::args().any(|arg| arg == "tall") {
        1560.0
    } else if std::env::args().any(|arg| arg == "min") {
        520.0
    } else {
        900.0
    };
    let width = if std::env::args().any(|arg| arg == "min") {
        420.0
    } else {
        460.0
    };
    win.window()
        .set_size(slint::LogicalSize::new(width, height));
    // 本文は画面の高さより長いので、下端（議事録ブロック・注記・バージョン）は `bottom` を
    // 渡してスクロールさせてから撮る。
    if std::env::args().any(|arg| arg == "bottom") {
        win.set_body_scroll(-620.0);
    }
    win.show()
        .expect("showing the window should succeed in this verification binary");

    // `snapshot <path>` が指定されていれば 1 フレーム後に PNG を書く（`snapshot` モジュール）。
    let _snapshot_timer = snapshot::arm(win.as_weak());

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}
