//! 状態へ入る唯一の入口（`Msg`）と、そこから出る依頼（`Effect`）。#188 の PR-3b。
//!
//! **下り（`Command`）は人が起こしたこと、上り（`Event`）は外で起きた事実**。どちらも
//! `update` を通り、`update` だけが `AppState` を変える。
//!
//! `Effect` は **shell がそれだけで実行できる依頼**にする。core は `sessions` を持っていない
//! （PR-3b の範囲。`crate::app` の doc）ので、`dir` から音源やパスを引くのは shell の仕事——
//! **依頼に足りない判断を混ぜない**。`replaces_playback` を積んであるのはそのため（下記）。

use std::path::PathBuf;

use crate::app::Job;
use crate::reading_pane::TranscriptShortfall;

/// 状態へ入る唯一の入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Command(Command),
    Event(Event),
}

/// 人が起こしたこと。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// 一覧で録音を選んだ（`None` は解除）。
    Select(Option<PathBuf>),
}

/// 外で起きた事実。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// 読み込みが届いた。**受け入れるかは `update` が決める**（世代と対象の照合はそこ 1 箇所）。
    SessionLoaded {
        dir: PathBuf,
        generation: u64,
        /// 読める行が 1 行でも在るか。
        has_readable_segments: bool,
        shortfall: Option<TranscriptShortfall>,
    },
    /// 読み込みを**始められなかった**。
    ///
    /// 閉じている間に文字起こしが完了し、開き直した直後（一覧はまだ空で走査は非同期）に
    /// 読み直しが起きると、shell は `dir` から録音を引けない。これを返さないと「読み込み中」の
    /// 表示が永久に残る。
    LoadCouldNotStart { dir: PathBuf },
    /// ジョブの様子が変わった（`None` は**エントリが消えた**＝止めた・対象が無かった）。
    ///
    /// tick が `TranscribeWorker` のマップと `jobs` を突き合わせて、違うものだけ流す。
    JobChanged { dir: PathBuf, job: Option<Job> },
    /// 録音を消した。
    Deleted { dir: PathBuf },
}

/// core から shell への依頼。
#[derive(Debug, Clone, PartialEq, Eq)]
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
