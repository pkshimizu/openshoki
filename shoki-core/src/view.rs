//! 状態から画面へ出すもの（#188）。**文字起こしの状態を答えるのはここだけ**。
//!
//! ここへ畳んだ旧経路は 4 つ。どれも「セッション X のいまの状態は何か」に独自の優先順位で
//! 答えていて、直すたびに片方だけ直る形だった:
//!
//! | 旧 | いま |
//! |---|---|
//! | `main::transcript_display_status` | `view_row` の中 |
//! | `main::transcript_pane_of` | `view_detail` の中 |
//! | `main::LoadedTranscript::stored` | `stored_transcript`（`view_detail` の中） |
//! | `reading_pane::TranscriptInput::of` | `view_detail` が返す `transcript_input` |
//!
//! **優先順位は 1 つ**——ジョブの記録があればそれ、無ければディスクの印。ジョブの記録が先なのは、
//! 走り終わった直後にディスクの印だけ見ると「Transcribed」と言ってしまうため（#176）。
//!
//! **セッションは引数で受ける**。一覧そのものは shell に残っている（`crate::app` の doc）。

use crate::app::{AppState, JobPhase};
use crate::reading_pane::{
    PaneAction, StoredTranscript, SummaryStatus, TranscriptInput, TranscriptPane, TranscriptStatus,
    session_transcript_word,
};
use crate::session::RecordingSession;

/// 一覧の行が出す値（`group_heading` は含まない。下の doc）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    /// 時刻（`14:02`）。
    pub time_text: String,
    /// 日付と長さ（`Aug 10, 2026 · 1:12:40`）。長さが分からない録音では日付だけ。
    pub date_text: String,
    /// 音源と状態（`Mic + system · transcribing 48%`）。
    pub detail_text: String,
    pub transcript_status: TranscriptStatus,
}

/// 行の表示が変わったかを**安く**見るための値（#188）。
///
/// **`row_key` が同じなら `view_row` の出力も同じ**（`the_key_decides_the_row` が固定する）。
/// 100ms ごとに全行を回る経路なので、変わっていない行で `view_row` の確保を払わないために要る。
///
/// **`group_heading` は入らない**。`Row` に含めていないから（下の `view_row` の doc）。見出しが
/// 変わる契機（削除で繰り上がる・日をまたぐ）は shell が別に持っている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowKey {
    /// **どの録音か**。位置で引く以上、同じ位置に別の録音が入ったことに気づく手はこれだけ。
    /// パスそのものではなくハッシュにすると、衝突した行が永久に更新されなくなる（狙って
    /// 書かないと再現しにくい——保存先のパスに依存する）ので、識別は呼び出し側が位置と
    /// 対で持つ。ここが持つのは**表示に効く値だけ**。
    started: chrono::NaiveDateTime,
    /// 長さ（`date_text` に効く）。録音直後は `mix.mp3` がまだ無く片方の音源から見積もるので、
    /// 走査し直すと変わる。
    duration: Option<std::time::Duration>,
    has_mic: bool,
    has_system: bool,
    has_transcript: bool,
    /// ジョブの相（`Running` / `Stopping` / `Done` / `Failed` / 無し）。**`Running{None}` と
    /// `Stopping` は割合がどちらも `None`** なので、相を落とすと Stop を押しても行が
    /// 「transcribing」のまま固まる（#163 が潰した症状）。
    phase: PhaseKind,
    /// 走っている間の割合。
    percent: Option<u8>,
}

/// `RowKey` が持つジョブの相（payload を落としたもの）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseKind {
    None,
    Running,
    Stopping,
    Done,
    Failed,
}

impl PhaseKind {
    fn of(phase: Option<&JobPhase>) -> Self {
        match phase {
            None => Self::None,
            Some(JobPhase::Running { .. }) => Self::Running,
            Some(JobPhase::Stopping { .. }) => Self::Stopping,
            Some(JobPhase::Done { .. }) => Self::Done,
            Some(JobPhase::Failed { .. }) => Self::Failed,
        }
    }
}

/// この行の表示に効く値をまとめる。
pub fn row_key(state: &AppState, session: &RecordingSession) -> RowKey {
    let phase = state.job(&session.dir).map(|job| &job.phase);
    // **分解束縛で組む**——`RowKey` にフィールドを足すとここが割れるので、詰め忘れが黙って
    // 通らない（`view_row` の出力に効く値が抜けると、変わった行が更新されなくなる）。
    let RowKey {
        started,
        duration,
        has_mic,
        has_system,
        has_transcript,
        phase,
        percent,
    } = RowKey {
        started: session.started_for_key(),
        duration: session.duration,
        has_mic: session.has_mic,
        has_system: session.has_system,
        has_transcript: session.has_transcript,
        phase: PhaseKind::of(phase),
        percent: percent_of(phase),
    };
    RowKey {
        started,
        duration,
        has_mic,
        has_system,
        has_transcript,
        phase,
        percent,
    }
}

/// 一覧の行を組む。
///
/// **見出し（`group_heading`）は組まない**。見出しは「直前の行と同じ日か」で決まるので、行 1 つを
/// 見る関数では答えられない。shell が一覧の並びから組む。
pub fn view_row(state: &AppState, session: &RecordingSession) -> Row {
    let status = transcript_status(state, session);
    Row {
        time_text: session.display_time(),
        date_text: date_text(session),
        // 音源の語は `source_summary` の 1 箇所に持つ（詳細ヘッダと削除の確認も同じ語を使う）。
        detail_text: format!(
            "{} · {}",
            session.source_summary(),
            session_transcript_word(
                status,
                percent_of(state.job(&session.dir).map(|j| &j.phase))
            )
        ),
        transcript_status: status,
    }
}

/// 行の 2 段目（`Aug 10, 2026 · 1:12:40`）。**長さが分からない録音では区切りごと出さない**
/// ——`—:—` のような穴を作ると、行の意味が分からなくなる（#162）。
fn date_text(session: &RecordingSession) -> String {
    match session.duration.map(crate::reading_pane::format_elapsed) {
        Some(length) => format!("{} · {length}", session.display_date()),
        None => session.display_date(),
    }
}

fn percent_of(phase: Option<&JobPhase>) -> Option<u8> {
    match phase {
        Some(JobPhase::Running { percent, .. }) => *percent,
        _ => None,
    }
}

/// 一覧の行と削除ガードが読む、粗い進行状況。
///
/// **ジョブの記録が先、無ければディスクの印**（#175 / #176）。走り終わった記録は食い違いも
/// 一緒に持っているので、走った直後だけ「Transcribed」と言ってしまうことがない。
fn transcript_status(state: &AppState, session: &RecordingSession) -> TranscriptStatus {
    match state.job(&session.dir).map(|job| &job.phase) {
        Some(JobPhase::Running { .. }) => TranscriptStatus::Transcribing,
        Some(JobPhase::Stopping { .. }) => TranscriptStatus::Stopping,
        Some(JobPhase::Done { .. }) => TranscriptStatus::Done,
        Some(JobPhase::Failed { .. }) => TranscriptStatus::Failed,
        None if session.has_transcript => TranscriptStatus::Done,
        None => TranscriptStatus::NotTranscribed,
    }
}

/// 詳細ペイン（読む領域）が出すもの。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailView {
    /// Transcript タブの状態と中身。
    pub transcript: TranscriptPane,
    /// **議事録タブから見た入力の様子**（#165）。両タブの合流点なので、ここから出す。
    pub transcript_input: TranscriptInput,
    /// ワーカーがこの録音のファイルを触りうるか（`Stopping` も含む）。
    ///
    /// **議事録側の busy と OR してから使う**。片方だけ見ると、議事録生成中に Re-transcribe が
    /// 押せて、要約が読んでいる JSON を文字起こしが上書きしに行く。
    pub transcript_busy: bool,
    /// 空表示のボタン列。**`actions_allowed_while_busy` は掛けていない**（#188）。
    ///
    /// 掛けるのは shell——議事録側の busy を知らないので、ここで掛けると上の穴が開く。
    pub actions: Vec<PaneAction>,
    /// この録音の読み込みがまだ届いていない。
    ///
    /// 立っている間、shell は**前の録音の本文を出さない**（別の録音の発話を読ませない）。
    pub loading: bool,
}

/// 詳細ペインを組む。
///
/// **`session` は「いま画面に出ている録音」**。`state.selected()` とは比べない——比べる相手を
/// 2 つにすると、「読み込み中ではないのに、別の録音の結果として扱われる」組み合わせができる。
pub fn view_detail(state: &AppState, session: &RecordingSession, auto_on: bool) -> DetailView {
    let stored = stored_transcript(state, session);
    let transcript = transcript_pane(state, session, stored, auto_on);
    let message = transcript.message();
    DetailView {
        transcript_input: TranscriptInput::of(&transcript, stored),
        transcript_busy: state.job(&session.dir).is_some_and(|job| job.phase.busy()),
        actions: message.actions,
        loading: state.is_loading(&session.dir),
        transcript,
    }
}

/// ディスクに残っている文字起こしの様子（#175）。
///
/// **極性の反転と組み合わせを型の内側へ入れる**——呼び出し側に真偽値 2 つを書かせると、渡し
/// 違えても通る形が残る。
fn stored_transcript(state: &AppState, session: &RecordingSession) -> StoredTranscript {
    if !session.has_transcript {
        return StoredTranscript::None;
    }
    // **まだ読めていないなら「分からない」**（#175）。伏せるのは驚く動作なので、確信が無い
    // うちはやらない。
    let Some(facts) = state.loaded_for(&session.dir) else {
        return StoredTranscript::NoKnownShortfall;
    };
    let Some(shortfall) = facts.shortfall else {
        return StoredTranscript::NoKnownShortfall;
    };
    // **読める行が無いなら食い違いを言わない**。押しても何も現れない `Show partial` を出す
    // ことになる——読めなかった JSON は `Done` の空表示が担当する。
    if !facts.has_readable_segments {
        return StoredTranscript::NoKnownShortfall;
    }
    StoredTranscript::NotWhole { shortfall }
}

/// どの状態に落とすかを決める。**ジョブの記録が先、無ければディスクの印**。
fn transcript_pane(
    state: &AppState,
    session: &RecordingSession,
    stored: StoredTranscript,
    auto_on: bool,
) -> TranscriptPane {
    match state.job(&session.dir).map(|job| &job.phase) {
        Some(JobPhase::Running {
            model_label,
            percent,
        }) => TranscriptPane::Transcribing {
            model: model_label.clone(),
            percent: *percent,
        },
        Some(JobPhase::Stopping { model_label }) => TranscriptPane::Stopping {
            model: model_label.clone(),
        },
        Some(JobPhase::Done { shortfall: None }) => TranscriptPane::Done,
        Some(JobPhase::Done {
            shortfall: Some(shortfall),
        }) => TranscriptPane::NotWhole {
            shortfall: *shortfall,
        },
        Some(JobPhase::Failed { reason }) => TranscriptPane::Failed {
            reason: reason.clone(),
        },
        None => match stored {
            StoredTranscript::NotWhole { shortfall } => TranscriptPane::NotWhole { shortfall },
            StoredTranscript::NoKnownShortfall => TranscriptPane::Done,
            StoredTranscript::None => TranscriptPane::NotTranscribed { auto_on },
        },
    }
}

/// 議事録のジョブが積まれているか（キュー待ち＋生成中）。
///
/// **文字起こし側の busy と OR して使う**のは shell（`main::refresh_detail_panes`）。そこで
/// 作る値は Slint の `detail-jobs-pending` と**同じ条件**でなければならない——片方だけ変えると、
/// 同じ操作がヘッダからは押せないのに空表示からは押せる、という穴になる。
///
/// 議事録の状態そのものを組むのは shell（core へ移すのは段階 03）。
pub fn summary_is_pending(status: SummaryStatus) -> bool {
    matches!(status, SummaryStatus::Queued | SummaryStatus::Summarizing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Job, JobId, JobPhase};
    use crate::session::DiskFacts;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn session(dir: &str, has_transcript: bool) -> RecordingSession {
        RecordingSession::new(
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                .expect("a real date")
                .and_hms_opt(14, 2, 0)
                .expect("a real time"),
            PathBuf::from(dir),
            DiskFacts {
                has_mic: true,
                has_transcript,
                ..DiskFacts::default()
            },
        )
    }

    fn with_job(dir: &str, phase: JobPhase) -> AppState {
        let mut jobs = HashMap::new();
        jobs.insert(
            PathBuf::from(dir),
            Job {
                id: JobId(1),
                phase,
            },
        );
        AppState::for_test(Some(PathBuf::from(dir)), jobs)
    }

    /// **`row_key` が同じなら `view_row` の出力も同じ**（#188 の受け入れ条件）。
    ///
    /// 行の差分更新はこのキーで間引くので、成り立たないと「変わったのに更新されない行」が
    /// 出る。効く値を 1 つずつ動かして、**キーも出力も動く**ことを確かめる——キーだけ動いても
    /// 出力だけ動いても、この関係は壊れている。
    #[test]
    fn the_key_decides_the_row() {
        let base = session("a", false);
        let empty = AppState::default();

        // 何も変えなければ、キーも出力も同じ。
        assert_eq!(row_key(&empty, &base), row_key(&empty, &base));
        assert_eq!(view_row(&empty, &base), view_row(&empty, &base));

        let mut with_length = base.clone();
        with_length.duration = Some(std::time::Duration::from_secs(4360));
        let mut with_system = base.clone();
        with_system.has_system = true;
        let mut transcribed = base.clone();
        transcribed.has_transcript = true;
        // **時刻と `has_mic` も動かす**。動かさないと、キーからこの 2 つを落としても
        // テストが緑のまま通る（実際に落として確かめた）。
        let later = RecordingSession::new(
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                .expect("a real date")
                .and_hms_opt(15, 30, 0)
                .expect("a real time"),
            PathBuf::from("a"),
            DiskFacts {
                has_mic: true,
                ..DiskFacts::default()
            },
        );
        let mut system_only = base.clone();
        system_only.has_mic = false;
        system_only.has_system = true;

        // 表示に効く値を動かすと、キーも出力も動く。
        for changed in [
            &with_length,
            &with_system,
            &transcribed,
            &later,
            &system_only,
        ] {
            assert_ne!(row_key(&empty, &base), row_key(&empty, changed));
            assert_ne!(view_row(&empty, &base), view_row(&empty, changed));
        }

        // ジョブの相と割合も同じ。
        let running = with_job(
            "a",
            JobPhase::Running {
                model_label: "base".to_owned(),
                percent: Some(40),
            },
        );
        let further = with_job(
            "a",
            JobPhase::Running {
                model_label: "base".to_owned(),
                percent: Some(41),
            },
        );
        // **`Running { percent: None }` と `Stopping` は割合が同じ**。相を落とすと Stop を
        // 押しても行が「transcribing」のまま固まる（#163）。
        let waiting = with_job(
            "a",
            JobPhase::Running {
                model_label: "base".to_owned(),
                percent: None,
            },
        );
        let stopping = with_job(
            "a",
            JobPhase::Stopping {
                model_label: "base".to_owned(),
            },
        );
        assert_ne!(row_key(&running, &base), row_key(&further, &base));
        assert_ne!(view_row(&running, &base), view_row(&further, &base));
        assert_ne!(row_key(&waiting, &base), row_key(&stopping, &base));
        assert_ne!(view_row(&waiting, &base), view_row(&stopping, &base));

        // **モデル名は行に出ない**ので、キーにも入らない（入れると走っている間じゅう
        // 行を組み直すことになる）。
        let other_model = with_job(
            "a",
            JobPhase::Running {
                model_label: "large".to_owned(),
                percent: Some(40),
            },
        );
        assert_eq!(row_key(&running, &base), row_key(&other_model, &base));
        assert_eq!(view_row(&running, &base), view_row(&other_model, &base));
    }

    /// 行の 3 段目は**音源と状態**を 1 行にまとめる（#162）。
    ///
    /// 割合が来ていれば出す——読む領域を開かなくても、どれが動いているか分かる。**止めている
    /// 最中は出さない**（止めると決めた後の進捗は読み手に何も足さない）。
    #[test]
    fn a_row_says_its_sources_and_transcript_state() {
        let both = |has_mic, has_system| {
            RecordingSession::new(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                    .expect("a real date")
                    .and_hms_opt(14, 0, 0)
                    .expect("a real time"),
                PathBuf::from("a"),
                DiskFacts {
                    has_mic,
                    has_system,
                    ..DiskFacts::default()
                },
            )
        };
        let running = |percent| {
            with_job(
                "a",
                JobPhase::Running {
                    model_label: "base".to_owned(),
                    percent,
                },
            )
        };
        assert_eq!(
            view_row(&running(None), &both(true, true)).detail_text,
            "Mic + system · transcribing"
        );
        assert_eq!(
            view_row(&running(Some(48)), &both(true, true)).detail_text,
            "Mic + system · transcribing 48%"
        );
        let stopping = with_job(
            "a",
            JobPhase::Stopping {
                model_label: "base".to_owned(),
            },
        );
        assert_eq!(
            view_row(&stopping, &both(true, true)).detail_text,
            "Mic + system · stopping"
        );
        let done = with_job("a", JobPhase::Done { shortfall: None });
        assert_eq!(
            view_row(&done, &both(true, false)).detail_text,
            "Mic only · transcribed"
        );
        let failed = with_job(
            "a",
            JobPhase::Failed {
                reason: crate::reading_pane::TranscribeFailure::ModelMissing,
            },
        );
        assert_eq!(
            view_row(&failed, &both(false, false)).detail_text,
            "No audio · transcription failed",
            "a session without audio still says what it is"
        );
    }

    /// 長さは**分からないときに段ごと出さない**（`—:—` のような穴を作らない。#162）。
    #[test]
    fn a_row_shows_the_length_only_when_it_is_known() {
        let idle = AppState::default();
        let mut s = session("a", false);
        assert_eq!(view_row(&idle, &s).date_text, "Aug 10, 2026");
        s.duration = Some(std::time::Duration::from_secs(4360));
        assert_eq!(view_row(&idle, &s).date_text, "Aug 10, 2026 · 1:12:40");
        // **既存の整形をそのまま使う**（`format_elapsed`）。デザインは `6:20` だが、同じ
        // ウィンドウのプレイヤーが `01:45 / 05:00` を出すので、1 時間未満のゼロ詰めは揃える
        // ほうを取った（形を 2 つ持つと、どちらが正か分からなくなる）。
        s.duration = Some(std::time::Duration::from_secs(380));
        assert_eq!(view_row(&idle, &s).date_text, "Aug 10, 2026 · 06:20");
    }

    /// **ジョブの記録がディスクの印より先**（#176）。走り終わった記録は食い違いも持っているので、
    /// 走った直後だけ「Transcribed」と言ってしまうことがない。
    #[test]
    fn the_job_record_wins_over_what_is_on_disk() {
        let transcribed = session("a", true);
        let empty = AppState::default();
        // ジョブが無ければディスクの印。
        assert_eq!(
            view_row(&empty, &transcribed).transcript_status,
            TranscriptStatus::Done
        );
        // 走っていれば、ディスクに在っても「走っている」。
        let running = with_job(
            "a",
            JobPhase::Running {
                model_label: "base".to_owned(),
                percent: None,
            },
        );
        assert_eq!(
            view_row(&running, &transcribed).transcript_status,
            TranscriptStatus::Transcribing
        );
        // 走り終わって食い違いが見つかっていれば、ペインはそう言う（行は `Done` のまま）。
        let not_whole = with_job(
            "a",
            JobPhase::Done {
                shortfall: Some(crate::reading_pane::TranscriptShortfall::StopsPartway),
            },
        );
        let detail = view_detail(&not_whole, &transcribed, false);
        assert!(matches!(detail.transcript, TranscriptPane::NotWhole { .. }));
        assert_eq!(
            view_row(&not_whole, &transcribed).transcript_status,
            TranscriptStatus::Done
        );
    }

    /// **読み込みが届くまでは食い違いを言わない**（#175）。伏せるのは驚く動作なので、確信が
    /// 無いうちはやらない。
    #[test]
    fn nothing_is_hidden_until_the_load_arrives() {
        let transcribed = session("a", true);
        let state = AppState::for_test(Some(PathBuf::from("a")), HashMap::new());
        let detail = view_detail(&state, &transcribed, false);
        assert!(detail.loading, "the load has not arrived yet");
        assert_eq!(detail.transcript, TranscriptPane::Done);
        assert_eq!(detail.transcript_input, TranscriptInput::Ready);
    }

    /// **ボタンは掛けずに返す**（#188）。掛けるのは shell——議事録側の busy と OR してからで
    /// ないと、議事録生成中に Re-transcribe が押せる。
    #[test]
    fn the_actions_come_back_ungated() {
        let transcribed = session("a", true);
        let running = with_job(
            "a",
            JobPhase::Running {
                model_label: "base".to_owned(),
                percent: None,
            },
        );
        let detail = view_detail(&running, &transcribed, false);
        assert!(detail.transcript_busy);
        // 走っている間の空表示は Stop を出す。busy で消される側のボタンではないので、
        // 掛けても掛けなくても残る——ここで見たいのは「掛けていない」ことなので、
        // 掛けた結果と比べる。
        let gated = crate::reading_pane::actions_allowed_while_busy(detail.actions.clone(), true);
        assert_eq!(detail.actions, gated, "stop stays available while busy");

        // 走っていない録音の空表示は Re-transcribe を出す。掛けると消える。
        let idle = AppState::for_test(Some(PathBuf::from("a")), HashMap::new());
        let detail = view_detail(&idle, &session("a", false), false);
        let gated = crate::reading_pane::actions_allowed_while_busy(detail.actions.clone(), true);
        assert_ne!(
            detail.actions, gated,
            "the ungated actions must still contain what busy would remove"
        );
    }
}
