//! モデル管理ウィンドウの描画確認用バイナリ（`docs/rules/slint.md` の検証手順）。
//!
//! 行は「名前・説明・状態」の 3 段と、右のサイズ・最大 2 つのボタン（Use / Download / Delete）が
//! 横に並ぶ。種別の見出しを挟み、状態によってボタンの数が変わるので**行の高さと横幅が揃わなく
//! なりやすい**。空のときの縮退表示と確認モーダルも、ビルドやテストバックエンドでは検出できない
//! ため、ここで目視する。
//!
//! ```sh
//! cargo run --example models_view                        # 既定（種別 2 つ＋カタログ外）
//! cargo run --example models_view -- empty               # 一覧が空（表示の穴を見る）
//! cargo run --example models_view -- unreadable          # models/ を走査できなかった状態
//! cargo run --example models_view -- confirm             # 削除の確認モーダルを開いた状態
//! cargo run --example models_view -- notice              # 失敗の通知を出した状態
//! cargo run --example models_view -- <上記> snapshot out.png  # PNG に書き出して確認
//! ```

slint::include_modules!();

#[path = "verification/snapshot.rs"]
mod snapshot;

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

/// 確認モーダルの説明の共通部分（`src/main.rs` の `model_delete_detail` の複製。bin クレートなので
/// import できない。あちらを変えたらここも合わせること）。
const DELETE_DETAIL_HEAD: &str = "The file is deleted permanently — it does not go to the Trash.";

/// 種別の区切り行。
fn heading(title: &str) -> ModelRow {
    ModelRow {
        is_heading: true,
        name: title.into(),
        ..ModelRow::default()
    }
}

/// 1 行。状態テキストは Rust 側（`src/main.rs` の `model_row_status_text`）の複製を置く
/// （長さが見え方に効くのが確認の主目的）。
fn row(
    name: &str,
    detail: &str,
    size: &str,
    status_text: &str,
    status: ModelStatus,
    can_use: bool,
    can_delete: bool,
) -> ModelRow {
    ModelRow {
        is_heading: false,
        name: name.into(),
        detail: detail.into(),
        size: size.into(),
        status_text: status_text.into(),
        delete_detail: format!(
            "This frees {size}. {DELETE_DETAIL_HEAD} {}",
            // 後半は使用状況で変わる（`model_delete_detail`）。config 上書き先とカタログ外は
            // 再取得できないので、いちばん長い文言になる。
            if can_use {
                "It downloads again the next time it is needed."
            } else {
                "config.toml points at this file, so the app cannot download it again."
            }
        )
        .into(),
        status,
        can_use,
        can_delete,
    }
}

/// 既定の並び: 種別 2 つ（未取得・取得中・取得済み・選択中・失敗）＋カタログ外・config 上書き先。
fn sample_rows() -> Vec<ModelRow> {
    vec![
        heading("Transcription — Whisper"),
        row(
            "Small",
            "balanced speed and accuracy",
            "465 MB",
            "Downloaded — selected in Settings",
            ModelStatus::Installed,
            false,
            true,
        ),
        row(
            "Large v3 Turbo",
            "the most accurate, and the slowest",
            "1.5 GB",
            "Downloading… 42%",
            ModelStatus::Downloading,
            true,
            false,
        ),
        row(
            "Tiny",
            "the fastest, least accurate",
            "74 MB",
            "Not downloaded",
            ModelStatus::NotDownloaded,
            true,
            false,
        ),
        heading("Meeting minutes — LLM"),
        row(
            "Qwen2.5 7B Instruct",
            "54 s and 8.2 GB of memory for a 4-min meeting, more faithful",
            "4.4 GB",
            "Downloaded — selected in Settings — in use right now — cannot be deleted",
            ModelStatus::Installed,
            false,
            false,
        ),
        row(
            "Qwen2.5 3B Instruct",
            "25 s and 3.7 GB of memory for a 4-min meeting, but can invent details",
            "2.0 GB",
            "Download failed — see the log",
            ModelStatus::Failed,
            true,
            false,
        ),
        heading("Other files in the models folder"),
        row(
            // カタログ外はファイル名がそのまま表示名になるので、長いものを入れて縮退を見る
            // （2 行目は空＝行が出ない）。
            "ggml-distil-large-v3-some-very-long-experimental-name.bin",
            "",
            "2.9 GB",
            "Downloaded — not in the model catalog",
            ModelStatus::Installed,
            false,
            true,
        ),
        row(
            // config 上書き先。確認モーダルの説明がいちばん長くなる行。
            "my-own-model.gguf",
            "",
            "3.1 GB",
            "Downloaded — set in config.toml",
            ModelStatus::Installed,
            false,
            true,
        ),
    ]
}

fn main() {
    let win = ModelsWindow::new()
        .expect("creating the window should succeed in this verification binary");

    // 一覧の中身を引数で選べるようにする（空・走査失敗・モーダル・通知の確認用）。
    let variant = std::env::args().nth(1).unwrap_or_default();
    let rows = match variant.as_str() {
        "empty" | "unreadable" => Vec::new(),
        _ => sample_rows(),
    };
    // 空表示・合計・通知は `src/main.rs` の `MODELS_EMPTY_TEXT` / `MODELS_UNREADABLE_TEXT` /
    // `models_total_text` / `MODEL_IN_USE_NOTICE` の複製。
    win.set_empty_text(
        if variant == "unreadable" {
            "Could not list the models folder"
        } else {
            "No models available"
        }
        .into(),
    );
    win.set_total_text(if rows.is_empty() {
        "".into()
    } else {
        // 取得済みは 4 件（Small / Qwen 7B / カタログ外 / config 上書き先）。
        "4 models — 10.9 GB".into()
    });
    win.set_models(ModelRc::from(Rc::new(VecModel::from(rows))));
    if variant == "confirm" {
        // 対象は config 上書き先の行（説明がいちばん長いので折り返しを見る）。
        win.set_delete_index(9);
        win.set_show_delete_confirm(true);
    }
    if variant == "notice" {
        win.set_notice("This model is in use right now — it was not deleted.".into());
    }

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    // 実アプリと同じ寸法で見る（`src/main.rs` の MODELS_WIDTH/HEIGHT と一致させること）。
    win.window().set_size(slint::LogicalSize::new(560.0, 520.0));
    win.show()
        .expect("showing the window should succeed in this verification binary");

    // `snapshot <path>` が指定されていれば 1 フレーム後に PNG を書く（`snapshot` モジュール）。
    let _snapshot_timer = snapshot::arm(win.as_weak());

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}
