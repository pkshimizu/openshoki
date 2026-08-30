//! アプリの状態と、それを変える唯一の口（#188 の PR-3b）。
//!
//! ```text
//! shell（副作用がある層）
//!   UI（Slint 配線）／ ワーカー ／ adapters
//!         ↓ Command / Event        ↑ Effect / View
//! core（この層）
//!   update(&mut AppState, Msg) -> Vec<Effect>
//!   view_row / view_detail（`crate::view`）
//! ```
//!
//! # いまここに在るのは文字起こしだけ
//!
//! 段階 01 は**文字起こしの状態だけ**を通す（`docs/plans/done/20260829-core-shell-layers.md`）。
//! 一覧そのもの（`sessions`）・検索・走査・世代・削除は shell に残っていて、次の段階で移す。
//! だから `view_*` はセッションを**引数で受ける**——ここが持っているのは「選んでいるのはどれか」
//! 「読み込み済みの中身は何か」「ジョブはどうなっているか」の 3 つだけ。
//!
//! # ジョブは shell のワーカーの写し（過渡形）
//!
//! `jobs` は `TranscribeWorker` の状態マップを tick が写したもの。**答えを 2 つ持っているのでは
//! なく、写す向きが 1 方向**——表示はすべて `jobs` から出し、ワーカーのマップを表示のために
//! 読む経路は残さない。マップそのものの廃止は #189（段階 02）。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::reading_pane::{TranscribeFailure, TranscriptShortfall};

/// 文字起こしジョブの通番（`TranscribeWorker` が採る `seq`）。
///
/// **core では採らない**（#188）。進捗は FFI のコールバックスレッドから来るので、番号を知って
/// いるのはワーカー側でしかありえない。
///
/// **相だけでなくこれも比べる**。100ms の間に「完了 → 再投入」と往復すると、相の比較だけでは
/// 差分が 1 件も立たず、完成した本文が画面に出ないまま次が走る。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct JobId(pub u64);

/// 文字起こしジョブが**いまどうなっているか**。
///
/// `TranscribeState`（shell）と 1 対 1。**「止めた」「対象なし」はここに無い**——それは
/// `jobs` から**消える**ことで表す（ワーカーも同じで、降りたジョブはマップから消えて表示は
/// ディスクの印ベースへ戻る）。相として持つと、JSON が 1 行も無いのに「完了」と言うか、
/// 止めただけなのに赤い失敗表示を出すかのどちらかになる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobPhase {
    /// 投入済み（キュー待ちを含む）または処理中。
    Running {
        model_label: String,
        /// whisper が返し始めるまでは `None`。
        percent: Option<u8>,
    },
    /// 止めるよう伝えたが、まだ降りていない（#163）。
    Stopping { model_label: String },
    /// 走り終わった。**食い違いも一緒に持つ**（#176）——持たないと、走った直後だけディスクの
    /// 印に負けて「Transcribed」と言ってしまう。
    Done {
        shortfall: Option<TranscriptShortfall>,
    },
    /// 少なくとも 1 音源が失敗した。
    Failed { reason: TranscribeFailure },
}

impl JobPhase {
    /// ワーカーが**まだこのセッションのファイルを触りうる**か。
    ///
    /// 削除・再投入を止めるかの判断に使う（`Stopping` も含む——降りるまでは JSON を触る）。
    pub fn busy(&self) -> bool {
        matches!(self, Self::Running { .. } | Self::Stopping { .. })
    }
}

/// 1 件のジョブ（通番と相）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: JobId,
    pub phase: JobPhase,
}

/// 選択中の録音の、**読み込み済みの中身から分かること**。
///
/// **本文（セグメント）は持たない**（#188 の PR-3b）。ここが答えるのは「ディスクに残っている
/// 文字起こしの様子」だけで、それに要るのは下の 2 つ。本文は表示へ流すだけなので shell に残る
/// （`Transcript` の移設は PR-3c）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Loaded {
    /// どの録音を読んだか。
    dir: PathBuf,
    /// どの読み込みの結果か。**遅れて届いた結果を捨てる判定はここ 1 箇所**（#152 / #188）。
    generation: u64,
    /// 読める行が 1 行でも在るか。**無いなら食い違いを言わない**（#176）——押しても何も
    /// 現れない `Show partial` を出すことになる。
    has_readable_segments: bool,
    /// 録音との食い違い（`None` は見つからなかったこと）。
    shortfall: Option<TranscriptShortfall>,
}

/// アプリの状態。**変える口は `update` だけ**（`crate::update`）。
#[derive(Debug, Default)]
pub struct AppState {
    /// いま選んでいる録音（`None` は未選択）。**識別子だけ**。
    selected: Option<PathBuf>,
    /// 選択中の録音の読み込み結果。**1 件だけ**持つ。
    loaded: Option<Loaded>,
    /// 文字起こしのジョブ（走っているもの・走り終わった記録）。
    jobs: HashMap<PathBuf, Job>,
}

impl AppState {
    /// いま選んでいる録音。
    pub fn selected(&self) -> Option<&std::path::Path> {
        self.selected.as_deref()
    }

    /// この録音のジョブ（無ければ `None`）。
    pub fn job(&self, dir: &std::path::Path) -> Option<&Job> {
        self.jobs.get(dir)
    }

    /// ジョブの一覧（shell が差分を取るために読む）。
    pub fn jobs(&self) -> &HashMap<PathBuf, Job> {
        &self.jobs
    }

    /// **テストが状態を組むための口**（本番は `update` を通す）。
    ///
    /// `pub` にしてあるが、本番から呼ぶと「状態を変える口は `update` だけ」が崩れる。
    /// 呼んでよいのはテストだけ、というのはコンパイラではなく約束で守っている。
    #[doc(hidden)]
    pub fn for_test(selected: Option<PathBuf>, jobs: HashMap<PathBuf, Job>) -> Self {
        Self {
            selected,
            loaded: None,
            jobs,
        }
    }

    /// 読み込み済みの中身（`crate::view` が読む）。
    pub(crate) fn loaded_for(&self, dir: &std::path::Path) -> Option<LoadedFacts> {
        let loaded = self.loaded.as_ref()?;
        (loaded.dir == dir).then_some(LoadedFacts {
            has_readable_segments: loaded.has_readable_segments,
            shortfall: loaded.shortfall,
        })
    }

    /// 読み込みが**まだ届いていない**か（`crate::view` が読む）。
    pub(crate) fn is_loading(&self, dir: &std::path::Path) -> bool {
        self.loaded.as_ref().is_none_or(|loaded| loaded.dir != dir)
    }

    pub(crate) fn set_selected(&mut self, dir: Option<PathBuf>) {
        self.selected = dir;
    }

    pub(crate) fn clear_loaded(&mut self) {
        self.loaded = None;
    }

    /// 届いた読み込みを受け入れるか。**世代の判定はここ 1 箇所**。
    pub(crate) fn accept_loaded(
        &mut self,
        dir: PathBuf,
        generation: u64,
        has_readable_segments: bool,
        shortfall: Option<TranscriptShortfall>,
    ) -> bool {
        // **いま選んでいる録音の結果でなければ捨てる**。選び直したあとに前の読み込みが届く。
        if self.selected.as_deref() != Some(dir.as_path()) {
            return false;
        }
        // **同じか新しい世代だけ**。解除を挟むと世代は飛ぶので、等号だけで見ると正当な結果まで
        // 捨てる（`clear_library_selection` も世代を進める）。
        if self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.generation > generation)
        {
            return false;
        }
        self.loaded = Some(Loaded {
            dir,
            generation,
            has_readable_segments,
            shortfall,
        });
        true
    }

    pub(crate) fn set_job(&mut self, dir: PathBuf, job: Option<Job>) {
        match job {
            Some(job) => {
                self.jobs.insert(dir, job);
            }
            None => {
                self.jobs.remove(&dir);
            }
        }
    }
}

/// `AppState` が読み込み結果から覚えていること（`crate::view` へ渡す形）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LoadedFacts {
    pub has_readable_segments: bool,
    pub shortfall: Option<TranscriptShortfall>,
}
