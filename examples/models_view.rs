//! モデル一覧ウィンドウの描画確認用バイナリ（`docs/rules/slint.md` の検証手順）。
//!
//! 一覧の行は「名前・種別＋ファイル名・状態」の 3 段と、右のサイズ・Delete が横に並ぶ。
//! 長い表示名やカタログ外の長いファイル名で潰れやすく、**空のとき**の縮退表示と確認モーダルは
//! ビルドやテストバックエンドでは検出できないため、ここで目視する。
//!
//! ```sh
//! cargo run --example models_view                        # 既定（複数行＋カタログ外＋長い名前）
//! cargo run --example models_view -- empty               # 1 つも取得していない状態
//! cargo run --example models_view -- one                 # 1 行だけ
//! cargo run --example models_view -- confirm             # 削除の確認モーダルを開いた状態
//! cargo run --example models_view -- <上記> snapshot out.png  # PNG に書き出して確認
//! ```

slint::include_modules!();

#[path = "verification/snapshot.rs"]
mod snapshot;

use std::rc::Rc;

use slint::{ComponentHandle, ModelRc, VecModel};

/// 状態テキスト・モーダルの説明は Rust 側（`src/main.rs` の `model_state_text` /
/// `model_delete_detail`）が組む。bin クレートなので import できないため、**実アプリで出る文言の
/// 複製**を置く（あちらを変えたらここも合わせること。長さが見え方に効くのが確認の主目的）。
fn row(name: &str, detail: &str, size: &str, state_text: &str, state: ModelRowState) -> ModelRow {
    ModelRow {
        name: name.into(),
        detail: detail.into(),
        size: size.into(),
        state_text: state_text.into(),
        delete_detail: if state == ModelRowState::Unknown {
            // カタログ外は再取得できないので文言が変わる（`model_delete_detail`）。
            format!("This frees {size}. The file is deleted permanently — it does not go to the Trash. The app cannot download this file again.")
        } else {
            format!("This frees {size}. The file is deleted permanently — it does not go to the Trash. It downloads again the next time it is needed.")
        }
        .into(),
        state,
    }
}

/// 既定の並び: 選択中・使用中・取得中・カタログ外（長いファイル名）を混ぜる。
fn sample_rows() -> Vec<ModelRow> {
    vec![
        row(
            "Qwen2.5 7B Instruct",
            "Summary LLM · qwen2.5-7b-instruct-q4_k_m.gguf",
            "4.4 GB",
            "Downloaded · selected in Settings",
            ModelRowState::Selected,
        ),
        row(
            "Large v3 Turbo",
            "Whisper speech · ggml-large-v3-turbo.bin",
            "1.5 GB",
            "In use right now — cannot be deleted",
            ModelRowState::InUse,
        ),
        row(
            "Small",
            "Whisper speech · ggml-small.bin",
            "465 MB",
            "Downloading — cannot be deleted",
            ModelRowState::Downloading,
        ),
        row(
            // カタログ外はファイル名がそのまま表示名になるので、長いものを入れて縮退を見る。
            "ggml-distil-large-v3-some-very-long-experimental-name.bin",
            "ggml-distil-large-v3-some-very-long-experimental-name.bin",
            "2.9 GB",
            "Not in the model catalog",
            ModelRowState::Unknown,
        ),
    ]
}

fn main() {
    let win = ModelsWindow::new()
        .expect("creating the window should succeed in this verification binary");

    // 一覧の中身を引数で選べるようにする（空・1 行・複数行・モーダルの確認用）。
    let variant = std::env::args().nth(1).unwrap_or_default();
    let rows = match variant.as_str() {
        "empty" => Vec::new(),
        "one" => vec![sample_rows().remove(0)],
        _ => sample_rows(),
    };
    // 合計は `src/main.rs` の `models_total_text` の複製（空のときは一覧の中央に出る文言）。
    win.set_total_text(match rows.len() {
        0 => "No models downloaded yet".into(),
        1 => "1 model · 4.4 GB".into(),
        _ => "4 models · 9.2 GB".into(),
    });
    win.set_models(ModelRc::from(Rc::new(VecModel::from(rows))));
    if variant == "confirm" {
        // 対象は長い名前の行（モーダルの折り返しを見る）。
        win.set_delete_index(3);
        win.set_show_delete_confirm(true);
    }

    win.window()
        .set_position(slint::LogicalPosition::new(60.0, 60.0));
    // 実アプリと同じ寸法で見る（`src/main.rs` の MODELS_WIDTH/HEIGHT と一致させること）。
    win.window().set_size(slint::LogicalSize::new(460.0, 420.0));
    win.show()
        .expect("showing the window should succeed in this verification binary");

    // `snapshot <path>` が指定されていれば 1 フレーム後に PNG を書く（`snapshot` モジュール）。
    let _snapshot_timer = snapshot::arm(win.as_weak());

    slint::run_event_loop().expect("the event loop should run in this verification binary");
}
