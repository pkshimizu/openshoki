//! `shoki-core` の語彙 → Slint の生成型（#188）。
//!
//! **写像はここ 1 箇所**。core は UI の型を知らないので、`TranscriptStatus` のような同名の型が
//! 2 つある——core 側（`shoki_core`）と Slint 側（`crate::` の生成型）。どちらを指しているかが
//! 読めなくなるので、変換をここへ集めて、ほかの場所では書かない。
//!
//! **すべて網羅 match**（ワイルドカードを置かない）。core 側に変種を足したらここが割れて、
//! 写し忘れに気づける。
//!
//! **確認用バイナリと共有する**。`examples/transcript_view.rs` は `#[path]` でこのファイルを
//! 取り込む——`shoki` は bin だけのクレートなので `use shoki::slint_map` と書けない。
//! 以前は `src/reading_pane.rs` を同じやり方で共有していた（#160 / #161。複製すると実際に
//! ずれた）ので、**ハックは消えたのではなく対象が移った**。消すには `src/lib.rs` を足して
//! `slint::include_modules!()` をそちらへ移すことになり、それは段階 05 の領分。

use slint::{ModelRc, SharedString, VecModel};
use std::rc::Rc;

/// 文字起こしの表示状態。
pub fn transcript_status(status: shoki_core::TranscriptStatus) -> crate::TranscriptStatus {
    match status {
        shoki_core::TranscriptStatus::NotTranscribed => crate::TranscriptStatus::NotTranscribed,
        shoki_core::TranscriptStatus::Transcribing => crate::TranscriptStatus::Transcribing,
        shoki_core::TranscriptStatus::Stopping => crate::TranscriptStatus::Stopping,
        shoki_core::TranscriptStatus::Done => crate::TranscriptStatus::Done,
        shoki_core::TranscriptStatus::Failed => crate::TranscriptStatus::Failed,
    }
}

/// 逆向き（Slint のモデルに入っている状態 → core の語彙）。一覧の行が持つ現在値と比べるのに使う。
pub fn transcript_status_from(status: crate::TranscriptStatus) -> shoki_core::TranscriptStatus {
    match status {
        crate::TranscriptStatus::NotTranscribed => shoki_core::TranscriptStatus::NotTranscribed,
        crate::TranscriptStatus::Transcribing => shoki_core::TranscriptStatus::Transcribing,
        crate::TranscriptStatus::Stopping => shoki_core::TranscriptStatus::Stopping,
        crate::TranscriptStatus::Done => shoki_core::TranscriptStatus::Done,
        crate::TranscriptStatus::Failed => shoki_core::TranscriptStatus::Failed,
    }
}

/// 議事録の表示状態。
pub fn summary_status(status: shoki_core::SummaryStatus) -> crate::SummaryStatus {
    match status {
        shoki_core::SummaryStatus::NotSummarized => crate::SummaryStatus::NotSummarized,
        shoki_core::SummaryStatus::Queued => crate::SummaryStatus::Queued,
        shoki_core::SummaryStatus::Summarizing => crate::SummaryStatus::Summarizing,
        shoki_core::SummaryStatus::Done => crate::SummaryStatus::Done,
        shoki_core::SummaryStatus::Failed => crate::SummaryStatus::Failed,
    }
}

/// 逆向き（Slint のプロパティに入っている状態 → core の語彙）。詳細ペインの現在値と比べるのに使う。
pub fn summary_status_from(status: crate::SummaryStatus) -> shoki_core::SummaryStatus {
    match status {
        crate::SummaryStatus::NotSummarized => shoki_core::SummaryStatus::NotSummarized,
        crate::SummaryStatus::Queued => shoki_core::SummaryStatus::Queued,
        crate::SummaryStatus::Summarizing => shoki_core::SummaryStatus::Summarizing,
        crate::SummaryStatus::Done => shoki_core::SummaryStatus::Done,
        crate::SummaryStatus::Failed => shoki_core::SummaryStatus::Failed,
    }
}

/// 空表示のボタンの種別。
pub fn pane_action_kind(kind: shoki_core::PaneActionKind) -> crate::PaneActionKind {
    match kind {
        shoki_core::PaneActionKind::Transcribe => crate::PaneActionKind::Transcribe,
        shoki_core::PaneActionKind::WriteNotes => crate::PaneActionKind::WriteNotes,
        shoki_core::PaneActionKind::CancelNotes => crate::PaneActionKind::CancelNotes,
        shoki_core::PaneActionKind::StopTranscription => crate::PaneActionKind::StopTranscription,
        shoki_core::PaneActionKind::OpenTranscription => crate::PaneActionKind::OpenTranscription,
        shoki_core::PaneActionKind::OpenNotes => crate::PaneActionKind::OpenNotes,
        shoki_core::PaneActionKind::TranscribeThenNotes => {
            crate::PaneActionKind::TranscribeThenNotes
        }
        shoki_core::PaneActionKind::ShowPartialTranscript => {
            crate::PaneActionKind::ShowPartialTranscript
        }
    }
}

/// ボタン 1 つ分。
pub fn pane_action(action: &shoki_core::PaneAction) -> crate::PaneAction {
    crate::PaneAction {
        label: SharedString::from(action.label.as_str()),
        kind: pane_action_kind(action.kind),
        primary: action.primary,
    }
}

/// ボタン列。Slint のモデルへそのまま入れられる形にする。
pub fn pane_actions(actions: &[shoki_core::PaneAction]) -> ModelRc<crate::PaneAction> {
    ModelRc::from(Rc::new(VecModel::from(
        actions.iter().map(pane_action).collect::<Vec<_>>(),
    )))
}
