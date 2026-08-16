//! 議事録設定ウィンドウの描画確認用バイナリ（骨格は `transcription_view` と同じ。説明はあちら）。
//!
//! ここだけの見どころは**文字起こしへの従属の注意書き**（`depends`）。別ウィンドウの状態を
//! 説明する長い文なので、折り返しとボタンの収まりを目視する。
//!
//! ```sh
//! cargo run --example minutes_view                       # 既定
//! cargo run --example minutes_view -- empty              # 一覧が空
//! cargo run --example minutes_view -- unreadable         # models/ を走査できなかった通知
//! cargo run --example minutes_view -- confirm            # 削除の確認モーダルを開いた状態
//! cargo run --example minutes_view -- notice             # 失敗の通知を出した状態
//! cargo run --example minutes_view -- depends            # 自動文字起こしが OFF の注意書き
//! cargo run --example minutes_view -- <上記> snapshot out.png
//! ```

slint::include_modules!();

#[path = "verification/model_rows.rs"]
mod model_rows;
#[path = "verification/snapshot.rs"]
mod snapshot;

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

use model_rows::{Returns, Sample, heading, row, stray_rows};

const CONFIRM_SAMPLE_NAME: &str = "Qwen2.5 7B Instruct";

/// 既定の並び: 要約 LLM のカタログ（使用中・取得中）＋カタログ外。
fn sample_rows() -> Vec<ModelRow> {
    let mut rows = vec![
        heading("Meeting notes — LLM"),
        row(Sample {
            name: CONFIRM_SAMPLE_NAME,
            detail: "About 40 s per hour of audio; needs 6 GB of free memory.",
            // 使用中のジョブがある行（削除できない理由がいちばん長い）。
            status_text: "Downloaded · selected for notes · a recording is being summarised now, \
                          so it cannot be deleted until that finishes.",
            size: "2.0 GB",
            status: ModelStatus::Installed,
            tone: StatusTone::Done,
            returns: Returns::Redownloads,
            can_use: false,
            can_delete: false,
            badge: "In use",
            progress: -1.0,
            progress_detail: "",
            mono: false,
        }),
        row(Sample {
            name: "Llama 8B",
            detail: "Longer, more structured notes; about 2 minutes per hour of audio and 10 GB \
                     of free memory.",
            size: "4.4 GB",
            status_text: "Downloading…",
            status: ModelStatus::Downloading,
            tone: StatusTone::Active,
            returns: Returns::Redownloads,
            can_use: true,
            can_delete: false,
            badge: "",
            progress: 0.21,
            progress_detail: "0.9 GB / 4.4 GB · 21%",
            mono: false,
        }),
    ];
    rows.extend(stray_rows());
    rows
}

fn main() {
    let win = MinutesWindow::new()
        .expect("creating the window should succeed in this verification binary");

    let variant = std::env::args().nth(1).unwrap_or_default();
    let rows = match variant.as_str() {
        "empty" => Vec::new(),
        _ => sample_rows(),
    };

    win.set_auto_summarize(true);
    win.set_feature_note(
        "Reads a transcript and writes decisions and action items. Runs on this Mac.".into(),
    );
    if variant == "depends" {
        // 自動文字起こしが OFF のときの注意（`windows::minutes::depends_note` の複製）。
        win.set_depends_note(
            "Automatic transcription is off, so no transcript is produced on its own and nothing \
             will run automatically. Notes you ask for by hand still work — shoki transcribes \
             that recording first."
                .into(),
        );
    }

    win.set_empty_text("No models available".into());
    if variant == "unreadable" {
        win.set_notice(
            "Could not list the models folder — sizes and states may be out of date.".into(),
        );
    }
    win.set_total_text(if rows.is_empty() {
        "".into()
    } else {
        // 取得済みは 2 件（Qwen 7B / カタログ外）。
        "2 models — 2.5 GB".into()
    });
    if variant == "confirm" {
        let index = rows
            .iter()
            .position(|row| row.name == CONFIRM_SAMPLE_NAME)
            .expect("the sample rows should contain the in-use row");
        win.set_delete_index(index as i32);
        win.set_show_delete_confirm(true);
    }
    win.set_models(ModelRc::from(Rc::new(VecModel::from(rows))));
    if variant == "notice" {
        win.set_notice("This model is in use right now — it was not deleted.".into());
    }

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    // 実アプリと同じ寸法で見る（`src/main.rs` の MINUTES_WIDTH/HEIGHT と一致させること）。
    win.window().set_size(slint::LogicalSize::new(620.0, 700.0));
    win.show()
        .expect("showing the window should succeed in this verification binary");

    let _snapshot_timer = snapshot::arm(win.as_weak());

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}
