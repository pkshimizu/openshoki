//! 一覧の行を組み立てる、確認用バイナリの共通部分（#141 で 2 つのウィンドウに分かれたので、
//! `transcription_view` / `minutes_view` の両方から使う）。
//!
//! 文言は `src/windows/models.rs` の複製（bin クレートなので import できない。あちらを変えたら
//! ここも合わせること）。**長さが見え方に効く**のが確認の主目的なので、実物と同じ長さの文を置く。

use super::{ModelRow, ModelStatus, StatusTone};

/// 確認モーダルの説明の共通部分（`model_delete_detail` の複製）。
const DELETE_DETAIL_HEAD: &str = "The file is deleted permanently — it does not go to the Trash.";

/// 種別の区切り行。
pub fn heading(title: &str) -> ModelRow {
    ModelRow {
        is_heading: true,
        name: title.into(),
        ..ModelRow::default()
    }
}

/// 確認モーダルの後半の文言（`model_delete_detail` の写像。使用状況で変わるので、`can_use` から
/// 推測せずに行ごとに渡す）。
// このモジュールは `#[path]` で各確認用バイナリへ取り込まれるので、**使わないバリアントが出る**
// （config 上書きは文字起こし側でしか見ていない）。写像としては全部あるのが正しい形なので、
// dead_code は許可する。
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Returns {
    /// カタログの行（消しても次に必要になったとき取得される）。
    Redownloads,
    /// `config.toml` がこの行のファイルを指している（自動では戻らないが Download で取り直せる）。
    InConfig,
    /// その種別が `config.toml` で上書きされている（上書きを外すまで戻らない）。
    Overridden,
    /// カタログ外（アプリでは戻せない）。
    Never,
}

/// 行 1 つの指定（引数が増えすぎないようまとめる）。
pub struct Sample<'a> {
    pub name: &'a str,
    pub detail: &'a str,
    pub size: &'a str,
    pub status_text: &'a str,
    pub status: ModelStatus,
    pub tone: StatusTone,
    pub returns: Returns,
    pub can_use: bool,
    pub can_delete: bool,
    /// 使用中の標（`In use` / `Selected`）。空なら出さない。
    pub badge: &'a str,
    /// 取得中だけ 0.0〜1.0（それ以外は -1）と、その内訳。
    pub progress: f32,
    pub progress_detail: &'a str,
    /// カタログ外のファイル名は等幅で出す。
    pub mono: bool,
}

pub fn row(sample: Sample) -> ModelRow {
    let Sample {
        name,
        detail,
        size,
        status_text,
        status,
        tone,
        returns,
        can_use,
        can_delete,
        badge,
        progress,
        progress_detail,
        mono,
    } = sample;
    ModelRow {
        is_heading: false,
        name: name.into(),
        detail: detail.into(),
        size: size.into(),
        status_text: status_text.into(),
        tone,
        progress,
        progress_detail: progress_detail.into(),
        badge: badge.into(),
        mono,
        delete_detail: format!(
            "This frees {size}. {DELETE_DETAIL_HEAD} {}",
            match returns {
                Returns::Redownloads => "It downloads again the next time it is needed.",
                Returns::InConfig =>
                    "config.toml points at this file, so it does not come back on its own — use Download to fetch it again.",
                Returns::Overridden =>
                    "It downloads again once config.toml no longer sets the model file.",
                Returns::Never => "The app cannot download this file again.",
            }
        )
        .into(),
        status,
        can_use,
        // 取得できるのは、ディスクに実体が無いカタログの行で、上書き先が別のファイルでないとき
        // （`can_download_row`）。この確認用バイナリでは `Overridden` の行だけ出さない。
        can_download: matches!(status, ModelStatus::NotDownloaded | ModelStatus::Failed)
            && returns != Returns::Overridden,
        can_delete,
    }
}

/// カタログ外のファイル（**両方のウィンドウの末尾に同じものが出る**。種別が判定できない以上、
/// 片方に置くのは恣意的で、片方しか開かない人からは掃除できなくなる）。
pub fn stray_rows() -> Vec<ModelRow> {
    vec![
        heading("Other files in the models folder"),
        row(Sample {
            name: "ggml-medium-q5.bin",
            detail: "",
            size: "539 MB",
            status_text: "Downloaded · not recognised, so shoki cannot tell which feature it \
                          belongs to. It is never used; deleting it here only removes the file.",
            status: ModelStatus::Installed,
            tone: StatusTone::Caution,
            returns: Returns::Never,
            can_use: false,
            can_delete: true,
            badge: "",
            progress: -1.0,
            progress_detail: "",
            mono: true,
        }),
    ]
}
