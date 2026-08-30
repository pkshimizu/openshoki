//! 状態へ入る唯一の入口（`Msg`）と、そこから出る依頼（`Effect`）。#188。
//!
//! **下り（`Command`）は人が起こしたこと、上り（`Event`）は外で起きた事実**。どちらも
//! `update` を通り、`update` だけが `AppState` を変える。
//!
//! `Effect` は **shell がそれだけで実行できる依頼**にする。core は `sessions` を持っていない
//! （`crate::app` の doc）ので、`dir` から音源やパスを引くのは shell の仕事——
//! **依頼に足りない判断を混ぜない**。`replaces_playback` を積んであるのはそのため（下記）。

use std::path::PathBuf;

use crate::app::{Job, ShownPath, SummaryJob};
use crate::reading_pane::TranscriptShortfall;

/// 状態へ入る唯一の入口。
#[derive(Clone, PartialEq, Eq)]
pub enum Msg {
    Command(Command),
    Event(Event),
}

/// 人が起こしたこと。
#[derive(Clone, PartialEq, Eq)]
pub enum Command {
    /// 一覧で録音を選んだ（`None` は解除）。
    Select(Option<PathBuf>),
}

/// 外で起きた事実。
#[derive(Clone, PartialEq, Eq)]
pub enum Event {
    /// 読み込みが届いた。**受け入れるかは `update` が決める**（世代と対象の照合はそこ 1 箇所）。
    SessionLoaded {
        dir: PathBuf,
        generation: u64,
        /// 読める行が 1 行でも在るか。
        has_readable_segments: bool,
        shortfall: Option<TranscriptShortfall>,
    },
    /// 読み込みを**始められなかった**（shell が `dir` から録音を引けなかった）。
    ///
    /// **いまの配線で到達する経路は見つかっていない**——読み直しを起こすのは選択中の録音だけで、
    /// 選択は必ず一覧から取るし、閉じて開き直すと `Select(None)` が先に流れる。それでも置いて
    /// あるのは、引けなかったときに「読み込み中」の表示を永久に残さないため（受け皿）。
    LoadCouldNotStart { dir: PathBuf },
    /// 文字起こしジョブの様子が変わった（`None` は**エントリが消えた**＝止めた・対象が無かった）。
    ///
    /// tick が `TranscribeWorker` のマップと `jobs` を突き合わせて、違うものだけ流す。
    JobChanged { dir: PathBuf, job: Option<Job> },
    /// 議事録ジョブの様子が変わった（同上。#189）。
    ///
    /// **分けてあるのは、同じ録音で 2 つが同時に在るから**（`crate::app::AppState::summaries` の doc）。
    SummaryChanged {
        dir: PathBuf,
        job: Option<SummaryJob>,
    },
    /// 録音を消した。
    Deleted { dir: PathBuf },
}

/// core から shell への依頼。
#[derive(Clone, PartialEq, Eq)]
pub enum Effect {
    /// この録音を読み直す。
    LoadSession {
        dir: PathBuf,
        /// **再生も差し替えるか**。選び直したときは `true`（音源が変わる）、文字起こしが
        /// 終わって中身だけ読み直すときは `false`。
        ///
        /// **依頼に載せる**（#188）。`dir` だけでは shell が選べず、常に `true` にすると
        /// 文字起こしの完了で再生が止まって先頭へ戻り、常に `false` にすると別の録音を選んでも
        /// 前の音が鳴り続ける（`PlaybackLoad` の doc）。
        replaces_playback: bool,
    },
    /// 届いた読み込みを**画面へ入れてよい**（`update` が世代と対象を確かめた）。
    ///
    /// **shell はこれが来たときだけ入れる**。shell 側でもう一度世代を見ると、判定が 2 つに
    /// なって食い違う（解除を挟むと世代が飛ぶ）。
    ShowLoaded,
    /// 表示中の中身を捨てる（選択を解除した・別の録音を選んだ・消した）。
    ///
    /// 文字起こしも議事録も発話由来の機微データなので、詳細ペインが隠れている間も持ち続けない
    /// （`docs/rules/security.md`）。
    ClearLoaded,
}

// **`Debug` は derive しない**——どれも録音のフルパスを運ぶ（`crate::app` の doc）。
// `assert_eq!` に要るので「付けない」ではなく「ファイル名だけ出す」側に倒す。

impl std::fmt::Debug for Msg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Command(command) => f.debug_tuple("Command").field(command).finish(),
            Self::Event(event) => f.debug_tuple("Event").field(event).finish(),
        }
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Select(dir) => f
                .debug_tuple("Select")
                .field(&dir.as_deref().map(ShownPath))
                .finish(),
        }
    }
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionLoaded {
                dir,
                generation,
                has_readable_segments,
                shortfall,
            } => f
                .debug_struct("SessionLoaded")
                .field("dir", &ShownPath(dir))
                .field("generation", generation)
                .field("has_readable_segments", has_readable_segments)
                .field("shortfall", shortfall)
                .finish(),
            Self::LoadCouldNotStart { dir } => f
                .debug_struct("LoadCouldNotStart")
                .field("dir", &ShownPath(dir))
                .finish(),
            Self::JobChanged { dir, job } => f
                .debug_struct("JobChanged")
                .field("dir", &ShownPath(dir))
                .field("job", job)
                .finish(),
            Self::SummaryChanged { dir, job } => f
                .debug_struct("SummaryChanged")
                .field("dir", &ShownPath(dir))
                .field("job", job)
                .finish(),
            Self::Deleted { dir } => f
                .debug_struct("Deleted")
                .field("dir", &ShownPath(dir))
                .finish(),
        }
    }
}

impl std::fmt::Debug for Effect {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LoadSession {
                dir,
                replaces_playback,
            } => f
                .debug_struct("LoadSession")
                .field("dir", &ShownPath(dir))
                .field("replaces_playback", replaces_playback)
                .finish(),
            Self::ShowLoaded => f.write_str("ShowLoaded"),
            Self::ClearLoaded => f.write_str("ClearLoaded"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// **フルパスを `{:?}` で漏らさない**（`docs/rules/security.md`）。`dir` はユーザー名を
    /// 含むので、出すのはファイル名だけ。どれかを derive に戻した瞬間に落ちる。
    #[test]
    fn messages_show_the_file_name_not_the_whole_path() {
        let dir = || PathBuf::from("/Users/someone/Recordings/20260810-140200");
        let shown = [
            format!("{:?}", Msg::Command(Command::Select(Some(dir())))),
            format!("{:?}", Event::LoadCouldNotStart { dir: dir() }),
            format!("{:?}", Event::Deleted { dir: dir() }),
            format!(
                "{:?}",
                Event::JobChanged {
                    dir: dir(),
                    job: None
                }
            ),
            format!(
                "{:?}",
                // **中身まで入れて出す**（#189）。`SummaryPhase` に将来パスを持つフィールドが
                // 増えても、同じ assert がそれを拾う。
                Event::SummaryChanged {
                    dir: dir(),
                    job: Some(crate::app::SummaryJob {
                        id: crate::app::JobId(1),
                        phase: crate::app::SummaryPhase::Summarizing {
                            model_label: "Qwen".to_owned(),
                            started: crate::test_now(),
                        },
                    }),
                }
            ),
            format!(
                "{:?}",
                Event::SessionLoaded {
                    dir: dir(),
                    generation: 1,
                    has_readable_segments: true,
                    shortfall: None,
                }
            ),
            format!(
                "{:?}",
                Effect::LoadSession {
                    dir: dir(),
                    replaces_playback: true
                }
            ),
        ];
        for shown in shown {
            assert!(
                shown.contains("20260810-140200"),
                "the file name is still there, got {shown}"
            );
            assert!(
                !shown.contains("someone"),
                "the rest of the path must not be shown, got {shown}"
            );
        }
    }
}
