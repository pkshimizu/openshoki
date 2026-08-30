//! アプリの状態と、それを変える唯一の口（#188）。
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
//!
//! # フルパスは `Debug` に出さない
//!
//! ここが持つ `PathBuf` は録音の**識別子**で、フルパスはユーザー名を含む。`{:?}` でログへ出すと
//! 漏れるので、この層でパスを持つ型は `Debug` を derive せず、`ShownPath` を通してファイル名
//! だけを出す（`docs/rules/security.md`。`RecordingSession` と同じ形）。

use std::collections::HashMap;
use std::path::PathBuf;

use crate::reading_pane::{TranscribeFailure, TranscriptShortfall};

/// ジョブの通番（ワーカーが採る `seq`）。
///
/// **core では採らない**（#188）。進捗は FFI のコールバックスレッドから来るので、番号を知って
/// いるのはワーカー側でしかありえない。
///
/// **マップをまたいで比べない**（#189）。`TranscribeWorker` と `SummarizeWorker` は**別々の
/// カウンタ**を持つので、`jobs` の `JobId(3)` と `summaries` の `JobId(3)` に関係は無い
/// （型は同じなので比べてもコンパイルは通る）。同じマップの中では、投入順そのもの——番号は
/// キューのロックを持ったまま配られ、ワーカーは FIFO で取り出す（`SummarizeWorker::submit`）。
///
/// **相だけでなくこれも比べる**。観測は 100ms ごとなので、その間に「完了 → 再投入」と往復すると
/// 相はどちらも `Running` のまま。通番を見れば「前のジョブは終わっている」と分かるので、
/// `update` はそこで読み直しを起こす（`a_job_that_was_replaced_within_one_tick_still_reloads`）。
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

/// 議事録ジョブが**いまどうなっているか**（#189）。
///
/// **順番と経過はここに持たない**。順番は前が終われば繰り上がるので、投入時に固定すると嘘に
/// なる（`crate::view::queued_position` が読み出しのたびに数える）。経過も同じ理由で、
/// 始めた時刻だけ持って引き算は `view_detail` がする——`Event` は 1 回きりなので、値を載せると
/// そこで止まる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryPhase {
    /// 積まれていて、まだ始まっていない。**モデル名は持たない**（理由は
    /// `summarize::SummarizeEntry::Queued`——取り出すまで何で走るかが決まらない）。
    Queued,
    /// 生成中。`started` は経過を出すのに使う（引き算は `view_detail`）。
    Summarizing {
        model_label: String,
        started: std::time::Instant,
    },
    /// 書き終わった。
    Done,
    Failed {
        reason: crate::reading_pane::SummarizeFailure,
    },
}

/// 1 件の文字起こしジョブ（通番と相）。**議事録とは別に持つ**（`AppState::summaries` の doc）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    pub id: JobId,
    pub phase: JobPhase,
}

/// 議事録ジョブ 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryJob {
    pub id: JobId,
    pub phase: SummaryPhase,
}

/// 選択中の録音の、**読み込み済みの中身から分かること**。
///
/// **本文（セグメント）は持たない**（#188）。ここが答えるのは「ディスクに残っている
/// 文字起こしの様子」だけで、それに要るのは下の 2 つ。本文は表示へ流すだけなので shell に残る
/// （`Transcript` の移設は段階 03 以降。`docs/plans/done/20260829-core-shell-layers.md`）。
///
/// **`Debug` は derive しない**（`AppState` と同じ理由）。
#[derive(Clone, PartialEq, Eq)]
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
///
/// **`Debug` は derive しない**: 持っているのは録音のフルパスで、`{:?}` でログへ出すと
/// ユーザー名が漏れる（`docs/rules/security.md`。ログに出すのはファイル名だけ）。
/// `RecordingSession` と同じ理由・同じ形で、**ファイル名だけ出す `Debug`** を手で書く。
#[derive(Default)]
pub struct AppState {
    /// いま選んでいる録音（`None` は未選択）。**識別子だけ**。
    selected: Option<PathBuf>,
    /// 選択中の録音の読み込み結果。**1 件だけ**持つ。
    loaded: Option<Loaded>,
    /// 文字起こしのジョブ（走っているもの・走り終わった記録）。
    jobs: HashMap<PathBuf, Job>,
    /// 議事録のジョブ。
    ///
    /// **文字起こしと 1 つのマップに畳まない**（#189）。同じ録音で 2 つが同時に在るのが
    /// **定常状態**だから——自動議事録は「文字起こしが終わった」直後に積まれるので、文字起こしの
    /// 走り終わった記録（`Done { shortfall }`。#176 で持たせた）と議事録の実行が数分間ずっと
    /// 同居する。1 件で持つと、その間ずっと一覧が「transcribing」と言い、部分文字起こしの警告も
    /// 消える。
    summaries: HashMap<PathBuf, SummaryJob>,
}

/// パスの**ファイル名だけ**を `Debug` に出すための包み（`docs/rules/security.md`）。
///
/// 名前が取れないパス（末尾が `..` など）は伏せる。core でパスを持つ型はこれを通す。
pub(crate) struct ShownPath<'a>(pub &'a std::path::Path);

impl std::fmt::Debug for ShownPath<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.file_name().fmt(f)
    }
}

impl std::fmt::Debug for Loaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            dir,
            generation,
            has_readable_segments,
            shortfall,
        } = self;
        f.debug_struct("Loaded")
            .field("dir", &ShownPath(dir))
            .field("generation", generation)
            .field("has_readable_segments", has_readable_segments)
            .field("shortfall", shortfall)
            .finish()
    }
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // **分解してから組む**（フィールドを足すと割れる）。
        let Self {
            selected,
            loaded,
            jobs,
            summaries,
        } = self;
        f.debug_struct("AppState")
            .field("selected", &selected.as_deref().map(ShownPath))
            .field("loaded", loaded)
            .field(
                "jobs",
                &jobs
                    .iter()
                    .map(|(dir, job)| (ShownPath(dir), job))
                    .collect::<Vec<_>>(),
            )
            .field(
                "summaries",
                &summaries
                    .iter()
                    .map(|(dir, job)| (ShownPath(dir), job))
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
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

    /// この録音の議事録ジョブ（無ければ `None`）。
    pub fn summary(&self, dir: &std::path::Path) -> Option<&SummaryJob> {
        self.summaries.get(dir)
    }

    /// 議事録ジョブの一覧（shell が差分を取るために読む。`crate::view` が順番を数えるのにも）。
    pub fn summaries(&self) -> &HashMap<PathBuf, SummaryJob> {
        &self.summaries
    }

    /// **テストが状態を組むための口**（本番は `update` を通す）。
    ///
    /// `#[cfg(test)]` なので出荷バイナリには入らない——`pub` のまま残すと「状態を変える口は
    /// `update` だけ」が約束でしか守られなくなる。
    #[cfg(test)]
    pub fn for_test(selected: Option<PathBuf>, jobs: HashMap<PathBuf, Job>) -> Self {
        Self {
            selected,
            loaded: None,
            jobs,
            summaries: HashMap::new(),
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

    /// 議事録ジョブを置く／落とす（`None` は**エントリが消えた**）。`set_job` と対称。
    pub(crate) fn set_summary(&mut self, dir: PathBuf, job: Option<SummaryJob>) {
        match job {
            Some(job) => {
                self.summaries.insert(dir, job);
            }
            None => {
                self.summaries.remove(&dir);
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
