//! `shoki-core` の語彙 ⇄ Slint の生成型（#188）。
//!
//! **写像はここ 1 箇所**。core は UI の型を知らないので、`TranscriptStatus` のような同名の型が
//! 2 つある——core 側（`shoki_core`）と Slint 側（`crate::` の生成型）。どちらを指しているかが
//! 読めなくなるので、変換をここへ集めて、ほかの場所では書かない。
//!
//! **すべて網羅 match**（ワイルドカードを置かない）。core 側に変種を足したらここが割れて、
//! 写し忘れに気づける。struct（`PaneAction`）は分解束縛で受けるので、**フィールドを足しても
//! 割れる**——束縛のまま写すと、新しいフィールドが黙って落ちる。
//!
//! **Slint 側の型は `Ui` を頭に付けて呼ぶ**。`shoki` の中では裸の `PaneAction` が Slint 型と
//! core 型のどちらも指しうるので（本番は Slint、テストは core）、別名で向きを固定する。
//!
//! **確認用バイナリと共有する**。`examples/transcript_view.rs` は `#[path]` でこのファイルを
//! 取り込む——`shoki` は bin だけのクレートなので `use shoki::slint_map` と書けない。
//! 以前は `src/reading_pane.rs` を同じやり方で共有していた（#160 / #161。複製すると実際に
//! ずれた）ので、**ハックは消えたのではなく対象が移った**。消すには `src/lib.rs` を足して
//! `slint::include_modules!()` をそちらへ移すことになり、それは段階 05 の領分。

use slint::{ModelRc, SharedString, VecModel};
use std::rc::Rc;

// Slint 生成型の別名。**`shoki` の中で Slint 型を指すときはこちらを使う**（裸の名前は core の
// 語彙に譲る）。
pub use crate::{
    PaneAction as UiPaneAction, PaneActionKind as UiPaneActionKind,
    SummaryStatus as UiSummaryStatus, TranscriptStatus as UiTranscriptStatus,
};

/// 文字起こしの表示状態。
pub fn to_ui_transcript_status(status: shoki_core::TranscriptStatus) -> UiTranscriptStatus {
    match status {
        shoki_core::TranscriptStatus::NotTranscribed => UiTranscriptStatus::NotTranscribed,
        shoki_core::TranscriptStatus::Transcribing => UiTranscriptStatus::Transcribing,
        shoki_core::TranscriptStatus::Stopping => UiTranscriptStatus::Stopping,
        shoki_core::TranscriptStatus::Done => UiTranscriptStatus::Done,
        shoki_core::TranscriptStatus::Failed => UiTranscriptStatus::Failed,
    }
}

/// 逆向き（Slint のモデルに入っている状態 → core の語彙）。一覧の行が持つ現在値と比べるのに使う。
pub fn from_ui_transcript_status(status: UiTranscriptStatus) -> shoki_core::TranscriptStatus {
    match status {
        UiTranscriptStatus::NotTranscribed => shoki_core::TranscriptStatus::NotTranscribed,
        UiTranscriptStatus::Transcribing => shoki_core::TranscriptStatus::Transcribing,
        UiTranscriptStatus::Stopping => shoki_core::TranscriptStatus::Stopping,
        UiTranscriptStatus::Done => shoki_core::TranscriptStatus::Done,
        UiTranscriptStatus::Failed => shoki_core::TranscriptStatus::Failed,
    }
}

/// 議事録の表示状態。
pub fn to_ui_summary_status(status: shoki_core::SummaryStatus) -> UiSummaryStatus {
    match status {
        shoki_core::SummaryStatus::NotSummarized => UiSummaryStatus::NotSummarized,
        shoki_core::SummaryStatus::Queued => UiSummaryStatus::Queued,
        shoki_core::SummaryStatus::Summarizing => UiSummaryStatus::Summarizing,
        shoki_core::SummaryStatus::Done => UiSummaryStatus::Done,
        shoki_core::SummaryStatus::Failed => UiSummaryStatus::Failed,
    }
}

/// 逆向き（Slint のプロパティに入っている状態 → core の語彙）。詳細ペインの現在値と比べるのに使う。
pub fn from_ui_summary_status(status: UiSummaryStatus) -> shoki_core::SummaryStatus {
    match status {
        UiSummaryStatus::NotSummarized => shoki_core::SummaryStatus::NotSummarized,
        UiSummaryStatus::Queued => shoki_core::SummaryStatus::Queued,
        UiSummaryStatus::Summarizing => shoki_core::SummaryStatus::Summarizing,
        UiSummaryStatus::Done => shoki_core::SummaryStatus::Done,
        UiSummaryStatus::Failed => shoki_core::SummaryStatus::Failed,
    }
}

/// 空表示のボタンの種別。
pub fn to_ui_pane_action_kind(kind: shoki_core::PaneActionKind) -> UiPaneActionKind {
    match kind {
        shoki_core::PaneActionKind::Transcribe => UiPaneActionKind::Transcribe,
        shoki_core::PaneActionKind::WriteNotes => UiPaneActionKind::WriteNotes,
        shoki_core::PaneActionKind::CancelNotes => UiPaneActionKind::CancelNotes,
        shoki_core::PaneActionKind::StopTranscription => UiPaneActionKind::StopTranscription,
        shoki_core::PaneActionKind::OpenTranscription => UiPaneActionKind::OpenTranscription,
        shoki_core::PaneActionKind::OpenNotes => UiPaneActionKind::OpenNotes,
        shoki_core::PaneActionKind::TranscribeThenNotes => UiPaneActionKind::TranscribeThenNotes,
        shoki_core::PaneActionKind::ShowPartialTranscript => {
            UiPaneActionKind::ShowPartialTranscript
        }
    }
}

/// ボタン 1 つ分。
pub fn to_ui_pane_action(action: &shoki_core::PaneAction) -> UiPaneAction {
    // **分解束縛で受ける**。`action.label` のように field access で写すと、core 側に
    // フィールドを足しても黙って落ちる——enum の網羅 match が守っているのと同じことを、
    // struct でも成り立たせる。
    let shoki_core::PaneAction {
        label,
        kind,
        primary,
    } = action;
    UiPaneAction {
        label: SharedString::from(label.as_str()),
        kind: to_ui_pane_action_kind(*kind),
        primary: *primary,
    }
}

/// すでにモデルに入っているボタン 1 つが、core の値と同じか。
///
/// **比較もここに置く**（#188）。呼び出し側でフィールドを並べて比べると、写像は分解束縛で
/// 割れるのに比較だけ黙って通る——足したフィールドが比較から落ち、「その欄だけ変わった tick で
/// 差分が立たず、古いボタンが残る」。**両側とも分解束縛で受ける**ので、どちらにフィールドを
/// 足しても割れる。
///
/// 確保はしない（`set_pane_actions` は 100ms tick を通る）。
pub fn ui_pane_action_matches(current: &UiPaneAction, next: &shoki_core::PaneAction) -> bool {
    let UiPaneAction {
        label: ui_label,
        kind: ui_kind,
        primary: ui_primary,
    } = current;
    let shoki_core::PaneAction {
        label,
        kind,
        primary,
    } = next;
    ui_label.as_str() == label && *ui_kind == to_ui_pane_action_kind(*kind) && ui_primary == primary
}

/// ボタン列。Slint のモデルへそのまま入れられる形にする。
pub fn to_ui_pane_actions(actions: &[shoki_core::PaneAction]) -> ModelRc<UiPaneAction> {
    ModelRc::from(Rc::new(VecModel::from(
        actions.iter().map(to_ui_pane_action).collect::<Vec<_>>(),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **写像が往復で戻ること**（#188）。
    ///
    /// 変種を足したときは網羅 match が捕まえるが、**写し先を付け替えたときは両方向とも
    /// コンパイルが通る**。そのとき非対称になると、`came_off_the_worker` が立たず読み直しが
    /// 走らない／議事録が完成しても表示が差し替わらない、という形で静かに壊れる
    /// （#152 / #162 と同じ追従漏れ）。
    ///
    /// **両方向から回す**。片方だけだと、2 つの変種が 1 つへ潰れる写し間違いを取り逃す。
    #[test]
    fn the_transcript_status_maps_back_to_itself() {
        for status in [
            shoki_core::TranscriptStatus::NotTranscribed,
            shoki_core::TranscriptStatus::Transcribing,
            shoki_core::TranscriptStatus::Stopping,
            shoki_core::TranscriptStatus::Done,
            shoki_core::TranscriptStatus::Failed,
        ] {
            assert_eq!(
                from_ui_transcript_status(to_ui_transcript_status(status)),
                status
            );
        }
        for status in [
            UiTranscriptStatus::NotTranscribed,
            UiTranscriptStatus::Transcribing,
            UiTranscriptStatus::Stopping,
            UiTranscriptStatus::Done,
            UiTranscriptStatus::Failed,
        ] {
            assert_eq!(
                to_ui_transcript_status(from_ui_transcript_status(status)),
                status
            );
        }
    }

    /// 議事録側も同じ（`the_transcript_status_maps_back_to_itself` と対称）。
    #[test]
    fn the_summary_status_maps_back_to_itself() {
        for status in [
            shoki_core::SummaryStatus::NotSummarized,
            shoki_core::SummaryStatus::Queued,
            shoki_core::SummaryStatus::Summarizing,
            shoki_core::SummaryStatus::Done,
            shoki_core::SummaryStatus::Failed,
        ] {
            assert_eq!(from_ui_summary_status(to_ui_summary_status(status)), status);
        }
        for status in [
            UiSummaryStatus::NotSummarized,
            UiSummaryStatus::Queued,
            UiSummaryStatus::Summarizing,
            UiSummaryStatus::Done,
            UiSummaryStatus::Failed,
        ] {
            assert_eq!(to_ui_summary_status(from_ui_summary_status(status)), status);
        }
    }

    /// 操作の種別は**片道しかない**ので、往復では守れない。代わりに
    /// **2 つの種別が 1 つへ潰れていないこと**を見る——潰れると、押した操作が別の操作として
    /// 実行される（`PaneActionKind` の doc が「ラベルの文字列で分岐しない」理由と同じ）。
    #[test]
    fn every_pane_action_kind_maps_to_a_different_one() {
        let kinds = [
            shoki_core::PaneActionKind::Transcribe,
            shoki_core::PaneActionKind::WriteNotes,
            shoki_core::PaneActionKind::CancelNotes,
            shoki_core::PaneActionKind::StopTranscription,
            shoki_core::PaneActionKind::OpenTranscription,
            shoki_core::PaneActionKind::OpenNotes,
            shoki_core::PaneActionKind::TranscribeThenNotes,
            shoki_core::PaneActionKind::ShowPartialTranscript,
        ];
        let mapped: Vec<UiPaneActionKind> = kinds.into_iter().map(to_ui_pane_action_kind).collect();
        for (i, left) in mapped.iter().enumerate() {
            for right in &mapped[i + 1..] {
                assert_ne!(left, right, "two kinds must not map to the same one");
            }
        }
    }
}
