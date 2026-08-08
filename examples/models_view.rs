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
//! cargo run --example models_view -- unreadable          # models/ を走査できなかった通知
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

/// 確認モーダルの後半の文言（`src/main.rs` の `model_delete_detail` の写像。使用状況で変わるので、
/// `can_use` から推測せずに行ごとに渡す）。
#[derive(Clone, Copy)]
enum Returns {
    /// カタログの行（消しても次に必要になったとき取得される）。
    Redownloads,
    /// `config.toml` が指している（消したら設定を直すまで戻らない）。
    InConfig,
    /// カタログ外（アプリでは戻せない）。
    Never,
}

/// 1 行。状態テキストは Rust 側（`src/main.rs` の `model_row_status_text`）の複製を置く
/// （長さが見え方に効くのが確認の主目的）。
/// 行 1 つの指定（引数が増えすぎないようまとめる）。
struct Sample<'a> {
    name: &'a str,
    detail: &'a str,
    size: &'a str,
    status_text: &'a str,
    status: ModelStatus,
    returns: Returns,
    can_use: bool,
    can_delete: bool,
}

fn row(sample: Sample) -> ModelRow {
    let Sample {
        name,
        detail,
        size,
        status_text,
        status,
        returns,
        can_use,
        can_delete,
    } = sample;
    ModelRow {
        is_heading: false,
        name: name.into(),
        detail: detail.into(),
        size: size.into(),
        status_text: status_text.into(),
        delete_detail: format!(
            "This frees {size}. {DELETE_DETAIL_HEAD} {}",
            match returns {
                Returns::Redownloads => "It downloads again the next time it is needed.",
                Returns::InConfig =>
                    "config.toml points at this file, so the app cannot download it again.",
                Returns::Never => "The app cannot download this file again.",
            }
        )
        .into(),
        status,
        can_use,
        // 取得できるのは、ディスクに実体が無いカタログの行だけ（`can_download_row`）。
        can_download: matches!(status, ModelStatus::NotDownloaded | ModelStatus::Failed),
        can_delete,
    }
}

/// 既定の並び: 種別 2 つ（未取得・取得中・取得済み・選択中・失敗）＋カタログ外・config 上書き先。
fn sample_rows() -> Vec<ModelRow> {
    vec![
        heading("Transcription — Whisper"),
        row(Sample {
            name: "Small",
            detail: "balanced speed and accuracy",
            size: "465 MB",
            status_text: "Downloaded — selected in Settings",
            status: ModelStatus::Installed,
            returns: Returns::Redownloads,
            can_use: false,
            can_delete: true,
        }),
        row(Sample {
            name: "Large v3 Turbo",
            detail: "the most accurate, and the slowest",
            size: "1.5 GB",
            status_text: "Downloading… 42%",
            status: ModelStatus::Downloading,
            returns: Returns::Redownloads,
            can_use: true,
            can_delete: false,
        }),
        row(Sample {
            name: "Tiny",
            detail: "the fastest, least accurate",
            size: "74 MB",
            status_text: "Not downloaded",
            status: ModelStatus::NotDownloaded,
            returns: Returns::Redownloads,
            can_use: true,
            can_delete: false,
        }),
        heading("Meeting minutes — LLM"),
        row(Sample {
            name: "Qwen2.5 7B Instruct",
            detail: "54 s and 8.2 GB of memory for a 4-min meeting, more faithful",
            size: "4.4 GB",
            status_text: "Downloaded — selected in Settings — cannot be deleted while it is in use",
            status: ModelStatus::Installed,
            returns: Returns::Redownloads,
            can_use: false,
            can_delete: false,
        }),
        row(Sample {
            // 失敗の理由まで行に出るので、いちばん長い状態テキストになる（折り返しの確認用）。
            name: "Qwen2.5 3B Instruct",
            detail: "25 s and 3.7 GB of memory for a 4-min meeting, but can invent details",
            size: "2.0 GB",
            status_text: "Download failed: not enough free disk space for Qwen2.5 3B Instruct — free up about 1.2 GB and try again",
            status: ModelStatus::Failed,
            returns: Returns::Redownloads,
            can_use: true,
            can_delete: false,
        }),
        heading("Other files in the models folder"),
        row(Sample {
            // カタログ外はファイル名がそのまま表示名になるので、長いものを入れて縮退を見る
            // （2 行目は空＝行が出ない）。
            name: "ggml-distil-large-v3-some-very-long-experimental-name.bin",
            detail: "",
            size: "2.9 GB",
            status_text: "Downloaded — not in the model catalog",
            status: ModelStatus::Installed,
            returns: Returns::Never,
            can_use: false,
            can_delete: true,
        }),
        row(Sample {
            // config 上書き先。確認モーダルの説明がいちばん長くなる行。
            name: "my-own-model.gguf",
            detail: "",
            size: "3.1 GB",
            status_text: "Downloaded — set in config.toml",
            status: ModelStatus::Installed,
            returns: Returns::InConfig,
            can_use: false,
            can_delete: true,
        }),
    ]
}

fn main() {
    let win = ModelsWindow::new()
        .expect("creating the window should succeed in this verification binary");

    // 一覧の中身を引数で選べるようにする（空・走査失敗・モーダル・通知の確認用）。
    let variant = std::env::args().nth(1).unwrap_or_default();
    let rows = match variant.as_str() {
        "empty" => Vec::new(),
        _ => sample_rows(),
    };
    // 空表示・合計・通知は `src/main.rs` の `MODELS_EMPTY_TEXT` / `MODELS_UNREADABLE_NOTICE` /
    // `models_total_text` / `MODEL_IN_USE_NOTICE` の複製。走査の失敗は**通知**で出る
    // （カタログの行は必ず並ぶので、空表示では気づけない）。
    win.set_empty_text("No models available".into());
    if variant == "unreadable" {
        win.set_notice(
            "Could not list the models folder — sizes and states may be out of date.".into(),
        );
    }
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
