//! 文字起こし設定ウィンドウの描画確認用バイナリ（`docs/rules/slint.md` の検証手順）。
//!
//! 上に機能のスイッチ、下にモデルの一覧が積まれる。行は「名前・サイズ・バッジ／説明／状態」と
//! 右の操作列で、状態によって操作の数が変わるので**行の高さと列の位置が揃わなくなりやすい**。
//! 空のときの縮退表示と確認モーダルも、ビルドやテストバックエンドでは検出できないため、ここで
//! 目視する。
//!
//! ```sh
//! cargo run --example transcription_view                       # 既定（カタログ＋カタログ外）
//! cargo run --example transcription_view -- empty              # 一覧が空（表示の穴を見る）
//! cargo run --example transcription_view -- unreadable         # models/ を走査できなかった通知
//! cargo run --example transcription_view -- confirm            # 削除の確認モーダルを開いた状態
//! cargo run --example transcription_view -- notice             # 失敗の通知を出した状態
//! cargo run --example transcription_view -- off                # 機能を OFF にした状態
//! cargo run --example transcription_view -- <上記> snapshot out.png  # PNG に書き出して確認
//! ```

slint::include_modules!();

#[path = "verification/model_rows.rs"]
mod model_rows;
#[path = "verification/snapshot.rs"]
mod snapshot;

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

use model_rows::{Returns, Sample, heading, row, stray_rows};

/// 削除の確認モーダルで見る行（説明がいちばん長い）。`sample_rows` の中の名前と一致させること。
const CONFIRM_SAMPLE_NAME: &str = "my-own-model.bin";

/// 既定の並び: whisper のカタログ（取得済み・使用中・取得中・未取得・失敗）＋カタログ外・
/// config 上書き先。
fn sample_rows(feature_on: bool) -> Vec<ModelRow> {
    let mut rows = vec![
        heading("Transcription — Whisper"),
        row(Sample {
            name: "Medium",
            detail: "Balanced accuracy and speed; handles mixed Japanese and English well.",
            size: "1.5 GB",
            status_text: if feature_on {
                "Downloaded · selected for transcription."
            } else {
                "Downloaded · will be used once the switch above is on."
            },
            status: ModelStatus::Installed,
            tone: StatusTone::Done,
            returns: Returns::Redownloads,
            can_use: false,
            can_delete: true,
            badge: if feature_on { "In use" } else { "Selected" },
            progress: -1.0,
            progress_detail: "",
            mono: false,
        }),
        row(Sample {
            name: "Large v3",
            detail: "Highest accuracy; noticeably slower without Apple silicon.",
            size: "3.1 GB",
            status_text: "Downloading…",
            status: ModelStatus::Downloading,
            tone: StatusTone::Active,
            returns: Returns::Redownloads,
            can_use: true,
            can_delete: false,
            badge: "",
            progress: 0.62,
            progress_detail: "1.9 GB / 3.1 GB · 62%",
            mono: false,
        }),
        row(Sample {
            name: "Large v3 Turbo",
            detail: "Distilled Large; close to Large v3 accuracy at roughly twice the speed.",
            size: "1.6 GB",
            // 失敗の理由がいちばん長い行（折り返しを見る）。
            status_text: "Download failed — connection interrupted after 0.4 GB of 1.6 GB.",
            status: ModelStatus::Failed,
            tone: StatusTone::Danger,
            returns: Returns::Redownloads,
            can_use: true,
            can_delete: false,
            badge: "",
            progress: -1.0,
            progress_detail: "",
            mono: false,
        }),
        row(Sample {
            name: "Tiny",
            detail: "Light-weight fallback for short voice memos.",
            size: "74 MB",
            status_text: "Not downloaded.",
            status: ModelStatus::NotDownloaded,
            tone: StatusTone::Neutral,
            returns: Returns::Redownloads,
            can_use: true,
            can_delete: false,
            badge: "",
            progress: -1.0,
            progress_detail: "",
            mono: false,
        }),
        row(Sample {
            // config 上書き先。確認モーダルの説明がいちばん長くなる行。
            name: CONFIRM_SAMPLE_NAME,
            detail: "",
            size: "3.1 GB",
            status_text: "Downloaded · selected for transcription.",
            status: ModelStatus::Installed,
            tone: StatusTone::Done,
            returns: Returns::InConfig,
            can_use: false,
            can_delete: true,
            badge: "",
            progress: -1.0,
            progress_detail: "",
            mono: true,
        }),
    ];
    rows.extend(stray_rows());
    rows
}

fn main() {
    let win = TranscriptionWindow::new()
        .expect("creating the window should succeed in this verification binary");

    // 一覧の中身と機能の状態を引数で選ぶ（空・走査失敗・モーダル・通知・OFF の確認用）。
    let variant = std::env::args().nth(1).unwrap_or_default();
    let feature_on = variant != "off";
    let rows = match variant.as_str() {
        "empty" => Vec::new(),
        _ => sample_rows(feature_on),
    };

    win.set_auto_transcribe(feature_on);
    win.set_feature_note(
        if feature_on {
            "Turns a finished recording into text on this Mac. Nothing is uploaded."
        } else {
            "Off — recordings stay as audio only."
        }
        .into(),
    );
    win.set_languages(ModelRc::from(Rc::new(VecModel::from(vec![
        slint::SharedString::from("Auto-detect"),
        slint::SharedString::from("English"),
        slint::SharedString::from("Japanese"),
    ]))));
    win.set_language_index(2);

    // 空表示・合計・通知は `src/windows/models.rs` の `MODELS_EMPTY_TEXT` /
    // `MODELS_UNREADABLE_NOTICE` / `models_total_text` / `MODEL_IN_USE_NOTICE` の複製。走査の
    // 失敗は**通知**で出る（カタログの行は必ず並ぶので、空表示では気づけない）。
    win.set_empty_text("No models available".into());
    if variant == "unreadable" {
        win.set_notice(
            "Could not list the models folder — sizes and states may be out of date.".into(),
        );
    }
    win.set_total_text(if rows.is_empty() {
        "".into()
    } else {
        // 取得済みは 3 件（Medium / config 上書き先 / カタログ外）。
        "3 models — 5.1 GB".into()
    });
    if variant == "confirm" {
        // 対象は config 上書き先の行（説明がいちばん長いので折り返しを見る）。行を足しても
        // ずれないよう、名前で引く（添字直書きは 1 行足すたびに別の行を指してしまう）。
        let index = rows
            .iter()
            .position(|row| row.name == CONFIRM_SAMPLE_NAME)
            .expect("the sample rows should contain the config override row");
        win.set_delete_index(index as i32);
        win.set_show_delete_confirm(true);
    }
    win.set_models(ModelRc::from(Rc::new(VecModel::from(rows))));
    if variant == "notice" {
        win.set_notice("This model is in use right now — it was not deleted.".into());
    }

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    // 実アプリと同じ寸法で見る（`src/main.rs` の TRANSCRIPTION_WIDTH/HEIGHT と一致させること）。
    win.window().set_size(slint::LogicalSize::new(620.0, 780.0));
    win.show()
        .expect("showing the window should succeed in this verification binary");

    let _snapshot_timer = snapshot::arm(win.as_weak());

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}
