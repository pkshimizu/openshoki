//! 読む領域（Recordings ウィンドウの Transcript / Notes タブ）に出す文言と操作（#154 / #160）。
//!
//! **確認用バイナリと共有するために切り出してある**。`examples/transcript_view.rs` は
//! `#[path]` でこのファイルを取り込む——複製していたときは実際にずれた（#161 で
//! `Waiting to summarize…` と `Waiting to write notes…` に割れているのが見つかった）。
//! 目視で確認するのが出荷される文言でなくなると、確認そのものが意味を失う。
//!
//! そのため**このモジュールは crate 内の何にも依存しない**。使うのは Slint の生成型
//! （`TranscriptStatus` / `SummaryStatus` / `PaneAction` / `PaneActionKind`）と std だけ。
//! 失敗の種別（`TranscribeFailure` / `SummarizeFailure`）をここに置いているのもそのため——
//! 種別は「読む領域が説明できることの語彙」で、文言表の網羅 match のすぐ隣にある。

use std::time::Duration;

// Slint の生成型。**`crate::` で引く**——bin でも確認用バイナリでも、`slint::include_modules!()`
// がクレート直下に置くので、同じ書き方で両方から通る。
use crate::{PaneAction, PaneActionKind, SummaryStatus, TranscriptStatus};

/// 文字起こしが失敗した理由（#159）。
///
/// **文言は持たない**。種別だけを持ち、文にするのは下の `transcribe_failure_text`。
/// ワーカー層が UI のコピーを持つと、状態→文言の対応表が 2 箇所に割れる
/// （`docs/rules/messages.md` の管轄）。種別を足せば網羅 match が割れて、書き忘れに気づける。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscribeFailure {
    /// モデルを取ってこられなかった。
    ModelDownload,
    /// モデルのファイルが無い。
    ModelMissing,
    /// モデルのパスを開けない（UTF-8 でない等）。
    ModelUnreadable,
    /// モデルは在るが読み込めなかった。
    ModelLoad,
    /// 音源の文字起こしに失敗した。**ファイル名だけ**を持つ（パスは持たない。
    /// `docs/rules/security.md`）。名前を作るのは `audio_display_name` だけで、そこが保証する。
    ///
    /// **空にならない**——構築するのは `run_job` の 1 箇所で、1 本以上失敗したときにしか作らない
    /// （空だと文言が ` could not be transcribed.` になる）。
    Files(Vec<String>),
    /// ワーカーがパニックした（**なぜかは分からない**）。
    Panicked,
}

/// 議事録の生成が失敗した理由（#159。文言は下の `summarize_failure_text` が正）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizeFailure {
    /// モデルを用意できなかった。
    ModelPrepare,
    /// モデルが走り切らなかった（メモリ不足が最も多い）。
    ModelRun,
    /// 走ったが何も返さなかった。
    EmptyOutput,
    /// 生成はできたが保存に失敗した。
    Save,
    /// ワーカーがパニックした。
    Panicked,
}

/// 「文字起こし中」の表示ラベル。状態テキストと Transcript の空表示の両方で同じ文言を
/// 使うため、1 箇所で管理する（片方だけ変えて食い違うのを防ぐ）。
pub const TRANSCRIBING_LABEL: &str = "Transcribing…";

/// 「要約生成中」の表示ラベル。状態テキストと Notes の空表示で同じ文言を使うため 1 箇所で
/// 管理する（`TRANSCRIBING_LABEL` と同じ理由）。
pub const SUMMARIZING_LABEL: &str = "Writing notes…";

/// 「キュー待ち」の表示ラベル。生成中と区別できる語にする: この間はまだ CPU を使っておらず、
/// 取り消せる（`SummarizeWorker::cancel`）。
///
/// **いまの参照は状態行（`summary_status_text`）だけ**。空表示の見出しは常に番号まで出すように
/// なった（#159 で順番が必ず分かるようになった。`SummaryPane::message`）。
pub const SUMMARY_QUEUED_LABEL: &str = "Waiting to write notes…";

/// 読む領域に出す 1 タブ分の中身（#154）。見出し・理由・次の操作の 3 つで 1 組。
///
/// **3 つをまとめて返す**のは、状態ごとに別々の関数で組み立てると「見出しは失敗なのに
/// ボタンは Transcribe now」のような食い違いを作れてしまうため。
pub struct PaneMessage {
    pub heading: String,
    /// 見出しの下の 1〜2 文。空なら段ごと出さない。
    pub body: String,
    /// 並べるボタン（最大 2 つ。主操作は 1 つだけ）。
    pub actions: Vec<PaneAction>,
}

impl PaneMessage {
    /// 見出しと理由だけの土台。操作は `with_primary` / `with_secondary` で足す。
    pub fn new(heading: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            heading: heading.into(),
            body: body.into(),
            actions: Vec::new(),
        }
    }

    /// 主操作を 1 つ添える（並ぶのは最大 2 つで、主はこれ 1 つだけ）。
    pub fn with_primary(mut self, label: &str, kind: PaneActionKind) -> Self {
        self.actions.push(PaneAction {
            label: label.into(),
            kind,
            primary: true,
        });
        self
    }

    /// 補助の操作を添える（`with_action` の後ろに並ぶ）。
    pub fn with_secondary(mut self, label: &str, kind: PaneActionKind) -> Self {
        self.actions.push(PaneAction {
            label: label.into(),
            kind,
            primary: false,
        });
        self
    }
}

/// 読む領域が出す文字起こしの状態と、そこに出す中身（#154）。
///
/// **`TranscriptStatus` はここから導出する**（`status`）。状態 enum と説明を別々に組み立てて
/// 渡すと、片方だけ更新した瞬間にありえない組み合わせができる（`docs/rules/slint.md`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptPane {
    /// まだ走らせていない。`auto_on` は設定の自動文字起こし（なぜ無いのかの説明が変わる）。
    NotTranscribed { auto_on: bool },
    Transcribing {
        model: String,
        /// whisper が返した進捗（返し始めるまでは `None`）。
        percent: Option<u8>,
    },
    /// 走り終わっている。**この空表示が出るのは JSON が読めなかったときだけ**
    /// （セグメントがあれば一覧が出るので、ここへは来ない）。
    Done,
    /// **理由は必ずある**（失敗の記録は理由と一緒にしか作られない。#159）。文言はここで組む。
    Failed { reason: TranscribeFailure },
}

impl TranscriptPane {
    /// Slint へ渡す状態 enum。文言と同じ値から作るので、両者が食い違わない。
    pub fn status(&self) -> TranscriptStatus {
        match self {
            Self::NotTranscribed { .. } => TranscriptStatus::NotTranscribed,
            Self::Transcribing { .. } => TranscriptStatus::Transcribing,
            Self::Done => TranscriptStatus::Done,
            Self::Failed { .. } => TranscriptStatus::Failed,
        }
    }

    /// 状態 → 読む領域の見出し・理由・次の操作。**ワイルドカードを置かない**（状態を足したら
    /// ここが割れて気づく。`docs/rules/slint.md`）。
    pub fn message(&self) -> PaneMessage {
        match self {
            Self::NotTranscribed { auto_on: false } => PaneMessage::new(
                "No transcript yet",
                "Automatic transcription is off, so this recording was kept as audio only.",
            )
            .with_primary("Transcribe now", PaneActionKind::Transcribe),
            Self::NotTranscribed { auto_on: true } => PaneMessage::new(
                "No transcript yet",
                "Automatic transcription is on, but this recording has not been through it.",
            )
            .with_primary("Transcribe now", PaneActionKind::Transcribe),
            Self::Transcribing { model, percent } => PaneMessage::new(
                match percent {
                    Some(percent) => format!("Transcribing — {percent}%"),
                    None => TRANSCRIBING_LABEL.to_owned(),
                },
                format!(
                    "{model} is running on this Mac. Finished lines appear here as they are \
                     recognized."
                ),
            ),
            Self::Done => PaneMessage::new(
                "No transcript to show",
                "The transcript file is missing or could not be read. Transcribing again will \
                 rebuild it.",
            )
            .with_primary("Transcribe again", PaneActionKind::Transcribe),
            Self::Failed { reason } => {
                PaneMessage::new("Transcription failed", transcribe_failure_text(reason))
                    .with_primary("Try again", PaneActionKind::Transcribe)
            }
        }
    }
}

/// 文字起こしの表示状態 → 詳細ペインの状態テキスト。
pub fn transcript_status_text(display_status: TranscriptStatus) -> &'static str {
    match display_status {
        TranscriptStatus::NotTranscribed => "Not transcribed",
        TranscriptStatus::Transcribing => TRANSCRIBING_LABEL,
        TranscriptStatus::Done => "Transcribed",
        TranscriptStatus::Failed => "Transcription failed",
    }
}

/// 走っているジョブがある間、**中身を作り直す操作を出さない**。
///
/// ワーカーがこのセッションの JSON / `summary.md` を読み書きしている最中に別のジョブを重ねると、
/// 書き換え途中の内容を読ませてしまう。詳細ヘッダの Transcribe / Summarize が同じ理由で無効に
/// なるので、押す場所が増えた空表示にも同じゲートを掛ける（取り消しと窓を開くのは残す）。
pub fn actions_allowed_while_busy(actions: Vec<PaneAction>, jobs_pending: bool) -> Vec<PaneAction> {
    if !jobs_pending {
        return actions;
    }
    actions
        .into_iter()
        .filter(|action| match action.kind {
            PaneActionKind::Transcribe | PaneActionKind::WriteNotes => false,
            PaneActionKind::CancelNotes
            | PaneActionKind::OpenTranscription
            | PaneActionKind::OpenNotes => true,
        })
        .collect()
}

/// 議事録生成の表示状態 → 詳細ペインの状態テキスト。
pub fn summary_status_text(display_status: SummaryStatus) -> &'static str {
    match display_status {
        SummaryStatus::NotSummarized => "No notes",
        SummaryStatus::Queued => SUMMARY_QUEUED_LABEL,
        SummaryStatus::Summarizing => SUMMARIZING_LABEL,
        SummaryStatus::Done => "Notes ready",
        SummaryStatus::Failed => "Notes failed",
    }
}

/// 読む領域が出す議事録の状態と、そこに出す中身（#154。`TranscriptPane` と対称）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryPane {
    /// **文字起こしが無いので動けない**。議事録は文字起こしを入力にするので、ここだけは
    /// 「まだ書いていない」ではなく「まだ書けない」。
    Blocked,
    /// 文字起こしはあるが、まだ書いていない。
    NotSummarized { auto_on: bool },
    Queued {
        /// キュー待ちの中で何番目か（1 始まり）。**必ず分かる**（キュー待ちの記録は順番と
        /// 一緒にしか作られない。#159）。
        position: usize,
    },
    Summarizing {
        model: String,
        /// 始めてからの経過（読める粒度に丸めた文字列）。**必ず分かる**（生成中の記録は開始
        /// 時刻と一緒にしか作られない）。
        started_ago: String,
    },
    /// 書き終わっている。**この空表示が出るのは `summary.md` が読めなかったときだけ**。
    Done,
    /// **理由は必ずある**（失敗の記録は理由と一緒にしか作られない。#159）。
    Failed { reason: SummarizeFailure },
}

impl SummaryPane {
    /// Slint へ渡す状態 enum。`Blocked` と `NotSummarized` はどちらも「未生成」に落ちる
    /// （読む領域の説明は違うが、ボタンの活性や一覧の見え方は同じ）。
    pub fn status(&self) -> SummaryStatus {
        match self {
            Self::Blocked | Self::NotSummarized { .. } => SummaryStatus::NotSummarized,
            Self::Queued { .. } => SummaryStatus::Queued,
            Self::Summarizing { .. } => SummaryStatus::Summarizing,
            Self::Done => SummaryStatus::Done,
            Self::Failed { .. } => SummaryStatus::Failed,
        }
    }

    /// 状態 → 読む領域の見出し・理由・次の操作（`TranscriptPane::message` と同じ流儀）。
    pub fn message(&self) -> PaneMessage {
        match self {
            Self::Blocked => PaneMessage::new(
                "No notes yet",
                "Notes are written from the transcript, and this recording has none. \
                 Transcribing it first will let notes run.",
            )
            .with_primary("Transcribe now", PaneActionKind::Transcribe)
            .with_secondary("Open transcription", PaneActionKind::OpenTranscription),
            Self::NotSummarized { auto_on: false } => PaneMessage::new(
                "No notes yet",
                "Notes are not written automatically, so this recording does not have any.",
            )
            .with_primary("Write notes", PaneActionKind::WriteNotes),
            Self::NotSummarized { auto_on: true } => PaneMessage::new(
                "No notes yet",
                "Automatic notes are on, but this recording has not been through them.",
            )
            .with_primary("Write notes", PaneActionKind::WriteNotes),
            Self::Queued { position } => PaneMessage::new(
                format!("Waiting to start — number {position} in the queue"),
                "Notes start once the work ahead of this recording finishes. Nothing is running \
                 for it yet, so it can still be canceled.",
            )
            .with_secondary("Cancel", PaneActionKind::CancelNotes),
            Self::Summarizing { model, started_ago } => PaneMessage::new(
                SUMMARIZING_LABEL,
                format!(
                    "{model} is running on this Mac, started {started_ago} ago. Re-transcribing \
                     is unavailable until this finishes, because it would change the input."
                ),
            ),
            Self::Done => PaneMessage::new(
                "No notes to show",
                "The notes file is missing or could not be read. Writing them again will \
                 rebuild it.",
            )
            .with_primary("Write notes again", PaneActionKind::WriteNotes),
            Self::Failed { reason } => {
                PaneMessage::new("Notes could not be written", summarize_failure_text(reason))
                    .with_primary("Try again", PaneActionKind::WriteNotes)
                    .with_secondary("Open meeting notes", PaneActionKind::OpenNotes)
            }
        }
    }
}

/// 文字起こしが失敗した理由 → 読む領域に出す 1 文（#159）。
///
/// **ワイルドカードを置かない**。種別を足したらここが割れて、書き忘れに気づける
/// （文言をワーカー層に置かないのはこのため。`docs/rules/messages.md` の管轄が割れる）。
pub fn transcribe_failure_text(reason: &TranscribeFailure) -> String {
    match reason {
        TranscribeFailure::ModelDownload => {
            "The transcription model could not be downloaded.".to_owned()
        }
        TranscribeFailure::ModelMissing => "The transcription model file is missing.".to_owned(),
        TranscribeFailure::ModelUnreadable => {
            "The transcription model file could not be opened.".to_owned()
        }
        TranscribeFailure::ModelLoad => "The transcription model could not be loaded.".to_owned(),
        // **件数で文の形は変えない**（`docs/rules/messages.md`）ので、1 本でも複数でも
        // 名前を並べた同じ形にする。
        TranscribeFailure::Files(names) => {
            format!("{} could not be transcribed.", names.join(", "))
        }
        // **なぜ止まったかは分からない**ので、分かったふりをしない。
        TranscribeFailure::Panicked => {
            "Transcribing this recording stopped unexpectedly.".to_owned()
        }
    }
}

/// 議事録の生成が失敗した理由 → 読む領域に出す 1 文（#159。`transcribe_failure_text` と対称）。
pub fn summarize_failure_text(reason: &SummarizeFailure) -> String {
    match reason {
        SummarizeFailure::ModelPrepare => {
            "The meeting notes model could not be prepared.".to_owned()
        }
        // **なぜ落ちたかは分からない**（llama.cpp は理由を返さないことが多い）ので、断定せずに
        // 「いちばんよくある原因」と、そこから取れる手を添える。
        SummarizeFailure::ModelRun => "The model could not finish. It may need more \
             free memory than this Mac has right now — closing other apps, or choosing a smaller \
             model, can let it run."
            .to_owned(),
        SummarizeFailure::EmptyOutput => "The model returned nothing to write.".to_owned(),
        SummarizeFailure::Save => "The notes could not be saved.".to_owned(),
        SummarizeFailure::Panicked => "Writing notes stopped unexpectedly.".to_owned(),
    }
}

/// 経過を読める粒度へ丸める（`40 seconds` / `3 minutes`）。**秒まで出すのは 1 分未満だけ**——
/// 分オーダーの処理で秒を刻んでも読めないうえ、100ms の tick で数字が動き続ける。
pub fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return plural(seconds, "second");
    }
    plural(seconds / 60, "minute")
}

/// 数と単位を英語として揃える（`1 minute` / `3 minutes`）。**そのまま画面に出る**文なので、
/// 単複が崩れると読みにくい（`docs/rules/messages.md`）。
pub fn plural(count: u64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}
