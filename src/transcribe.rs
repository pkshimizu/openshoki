//! 録音停止後の自動文字起こし（whisper.cpp / オンデバイス）。
//!
//! 保存済みの各音源 MP3（`mic.mp3` / `system.mp3`）を 16kHz/モノラル/f32 PCM へデコード＋
//! リサンプルし、`whisper-rs`（whisper.cpp）でセグメント（開始/終了秒＋テキスト）を得て、
//! 音源と同じセッションディレクトリへ `<音源名>.json`（Unix では 0600）として保存する。
//! 機微データを外部送信しないため、認識はオンデバイスに限定する（`docs/CONTEXT.md`）。
//!
//! whisper は CPU 集約で秒〜分オーダーかかるため、1 本のバックグラウンドワーカースレッド＋
//! キュー（`mpsc`）で逐次処理する。メインスレッド（Slint ループ）はジョブを投げるだけで
//! ブロックしない。モデル未指定/欠如・デコード失敗・whisper 失敗は握りつぶさずログし、
//! 他音源・アプリ・録音を巻き込まない（`docs/rules/error-handling.md`）。音源を最後まで
//! 読めなかったときは、**読めた範囲を文字起こしして保存する**（#164。どこまで読めたかは
//! `TranscribeFailure::Files` に載り、読む領域が説明する）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Resampler};
use serde::Serialize;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymphoniaError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// 失敗の種別は、文言表（網羅 match）の隣に置くために `reading_pane` が持っている。
/// ただし値を作るのはこのモジュールなので、読む人が探す場所はここでもある——同じ名前で
/// 引けるように再エクスポートしておく。
pub use crate::reading_pane::{FailedSource, TranscribeFailure};
use crate::reading_pane::{KeptFromSource, ShortfallMarks, TranscriptShortfall};

/// whisper が入力に取るサンプルレート（Hz）。これ以外のレートの音声はここへリサンプルする。
const WHISPER_SAMPLE_RATE: usize = 16_000;

/// whisper のタイムスタンプの単位（センチ秒 = 10ms）を秒に直す係数。
const CENTISECONDS_PER_SEC: f64 = 100.0;

/// リサンプラへ渡すチャンクサイズ（フレーム）。全体は `process_all` が繰り返し処理するため、
/// 品質と遅延に効く FFT ブロックの基準値として妥当な既定を選ぶ（リアルタイム性は不要）。
const RESAMPLE_CHUNK_FRAMES: usize = 1024;

/// 文字起こしジョブ。1 回の録音停止で保存された音源ファイル群と、設定のスナップショット。
/// 設定はジョブ投入時点の値を固定で持つ（処理中に設定が変わっても影響しない）。
pub struct TranscribeJob {
    /// 録音セッションのディレクトリ。状態表示（`TranscribeStatus`）のキーに使う。
    pub session_dir: PathBuf,
    /// 対象の音声ファイル（セッションディレクトリ内の `mic.mp3` / `system.mp3`）。
    pub audio_paths: Vec<PathBuf>,
    /// 使用する内蔵モデルの識別子（設定 `whisper_model`）。カタログ外は既定へフォールバック。
    pub model_id: String,
    /// whisper モデルの上書きパス（設定 `whisper_model_path`）。`None` なら内蔵モデル
    /// （`model_id`）を使う（未取得なら処理時に自動ダウンロードされる。`src/model_download.rs`）。
    pub model_override: Option<PathBuf>,
    /// 認識言語（whisper の言語コード。例: `en` / `ja`）。`auto` は自動判定。
    pub language: String,
    /// 文字起こしが**全音源成功した**ときに続けて投入する議事録要約の依頼（設定
    /// `auto_summarize` が OFF なら `None`）。要約は文字起こし結果を入力にするので、
    /// このセッションの文字起こしが終わってから投入する。
    ///
    /// 投入先は別スレッドの逐次ワーカーなので、**別セッションの**文字起こしとは並走しうる。
    /// whisper と LLM のピークを重ねないための直列化は `crate::inference_slot` が担う。
    pub summarize: Option<crate::summarize::SummarizeJob>,
}

/// セッション単位の進行状況と、**読む領域に出す中身**（#154。enum 化は #159）。
///
/// **状態ごとに、その状態でだけ意味のあるものを持つ**。以前は `status` と `Option` を並べて
/// いたが、`Done` なのに理由がある、`Failed` なのに進捗がある、といった組み合わせを型が許して
/// しまい、正しさが実行時のガードとテスト頼みになっていた（`ProgressSink::report` の
/// 「進行中のときだけ書く」など）。
///
/// **モデル名を持つのは走っている間だけ**。読む領域が「何が動いているか」を言うのはそこで、
/// 終わったあとに出しても読み手の役に立たない（`SummarizeEntry` と同じ基準。要るように
/// なったら足す）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscribeState {
    /// 投入済み（キュー待ちを含む）または処理中。
    Transcribing {
        model_label: String,
        /// 進捗の百分率（0〜100）。whisper が返し始めてから入るので、キュー待ちと読み込み中は
        /// `None`（そのときは割合を出さない）。**この `None` は実在する**。
        percent: Option<u8>,
    },
    /// 止めるよう伝えたが、ワーカーがまだ降りていない（#163）。降りたらエントリごと消えて、
    /// 表示は JSON の有無ベース（未実施／生成済み）へ戻る。
    ///
    /// **進行中と分ける**のは、Stop を押してから実際に降りるまでの間を「押しても何も起きて
    /// いない」に見せないため。降りるのは重い処理の切れ目（モデルの取得・推論スロットの待ち・
    /// モデルのロード・音源の切れ目・推論）なので、**待つ長さは押した場所で桁が変わる**——
    /// 推論中なら数百 ms、モデルのロード中なら十数秒、**モデルの取得や推論スロットの待ちに
    /// 入っていれば分オーダー**になる（待ちの最中は中断フラグを見ておらず、明けてから降りる。
    /// 待ちそのものを切る話は `#150` の領分）。ここに割合は持たない——止めると決めた後の
    /// 進捗は、読み手の判断に何も足さない。
    Stopping { model_label: String },
    /// 全音源の文字起こしが完了した。
    ///
    /// `shortfall` は**失敗ではないが録音と食い違っている**ことを表す（#176。壊れたパケットを
    /// 読み飛ばした音源があった）。**ここで持たないとディスクの印に負ける**——この状態は
    /// `transcript_pane_of` でディスクより優先されるので、持たせないと走った直後だけ
    /// 「Transcribed」と言ってしまう。
    Done {
        shortfall: Option<TranscriptShortfall>,
    },
    /// 少なくとも 1 音源が失敗した。
    Failed { reason: TranscribeFailure },
}

impl TranscribeState {
    /// 投入・処理開始時の状態（進捗はまだ無い）。
    fn starting(model_label: String) -> Self {
        Self::Transcribing {
            model_label,
            percent: None,
        }
    }

    /// 一覧の行や削除ガードが読む、粗い進行状況。
    pub fn status(&self) -> TranscribeStatus {
        match self {
            Self::Transcribing { .. } => TranscribeStatus::Transcribing,
            Self::Stopping { .. } => TranscribeStatus::Stopping,
            Self::Done { .. } => TranscribeStatus::Done,
            Self::Failed { .. } => TranscribeStatus::Failed,
        }
    }
}

/// 一覧の行が要る分だけの進行状況（#162）。`TranscribeState` からモデル名と理由を落としたもので、
/// **確保しない**。
///
/// 状態と割合をタプルで並べない——`(Done, Some(50))` のようなありえない組み合わせを型が許して
/// しまう（`docs/rules/coding-conventions.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscribeProgress {
    Transcribing {
        /// whisper が返し始めるまでは `None`（そのときは割合を出さない）。
        percent: Option<u8>,
    },
    /// 止めるよう伝えた後、まだ降りていない（#163）。
    Stopping,
    Done,
    Failed,
}

impl TranscribeProgress {
    /// 一覧の行が読む、粗い進行状況。
    pub fn status(self) -> TranscribeStatus {
        match self {
            Self::Transcribing { .. } => TranscribeStatus::Transcribing,
            Self::Stopping => TranscribeStatus::Stopping,
            Self::Done => TranscribeStatus::Done,
            Self::Failed => TranscribeStatus::Failed,
        }
    }

    /// 走っている間の割合（それ以外では `None`）。
    pub fn percent(self) -> Option<u8> {
        match self {
            Self::Transcribing { percent } => percent,
            Self::Stopping | Self::Done | Self::Failed => None,
        }
    }
}

/// セッション単位の文字起こしの進行状況。Library ウィンドウの状態表示に使う。
/// マップに載らないセッションの表示は「JSON の有無」で解決する（`docs/plans/done/` の #69 プラン）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscribeStatus {
    /// 投入済み（キュー待ちを含む）または処理中。
    Transcribing,
    /// 止めるよう伝えた後、ワーカーが降りるのを待っている（#163）。
    Stopping,
    /// 全音源の文字起こしが完了した。
    Done,
    /// 少なくとも 1 音源が失敗した（理由はログ。メモリのみで、再起動後は JSON の有無に基づく
    /// 表示へ戻る。再実行でクリアされる）。
    Failed,
}

/// 文字起こしのバックグラウンドワーカー。`submit` されたジョブを 1 本のスレッドで逐次処理する
/// （whisper は CPU 集約のため、録音が連続してもスレッドを増やさない）。
/// `Clone` で共有できる（後処理ワーカーからの自動投入と、Library ウィンドウからの
/// 手動再実行・状態表示が同じワーカー・同じ状態マップを使う）。
#[derive(Clone)]
pub struct TranscribeWorker {
    /// ワーカースレッドへの送信口。スレッド起動に失敗していたら `None`（文字起こしのみ縮退）。
    tx: Option<Sender<QueuedJob>>,
    /// 進行状況と、走っているジョブ（UI スレッドとワーカースレッドで共有）。
    queue: Arc<Mutex<QueueState>>,
}

/// セッションディレクトリ → 通番と進行状況のマップ。
///
/// **通番を持つ**のは、止めた直後に積み直したジョブを、先に走っていたジョブの後始末が
/// 消してしまわないようにするため（`SummarizeWorker` の `StatusMap` と同じ形）。マップに
/// 載っている通番と自分の通番が一致するときだけ、ワーカーは状態を書く。
type StatusMap = HashMap<PathBuf, (u64, TranscribeState)>;

/// ワーカーと UI が共有する状態。
struct QueueState {
    status: StatusMap,
    /// 次に投入するジョブへ渡す通番。
    next_seq: u64,
    /// いま走っているジョブ。**キュー待ちと処理中を見分ける唯一の手がかり**で
    /// （`TranscribeState::Transcribing` はどちらでも同じ）、止める口が
    /// 「フラグを立てて待つ」か「キューから外す」かを選ぶのに使う。
    running: Option<RunningJob>,
}

/// いま走っているジョブと、その中断フラグ。
struct RunningJob {
    session_dir: PathBuf,
    /// このジョブの通番。**`stop` はこれをマップへ書き戻す**——止めた時点で後続が積まれて
    /// いても、通番が走っている側に戻れば後続は `claim_job` が捨てる。
    seq: u64,
    /// 走らせているモデルの表示名。「止めています」の理由に出す。マップのエントリは後続に
    /// 差し替わっていることがあるので、**走っている側の名前はここから取る**。
    model_label: String,
    /// `true` になったら降りる。whisper の推論ループ（abort コールバック）と、
    /// 重い処理の切れ目の両方が見る。
    cancel: Arc<AtomicBool>,
}

impl QueueState {
    /// 次のジョブの通番を採る。**ロックの中でしか呼べない**（`&mut self`）ので、採番順と
    /// マップへの登録順がずれない（`SummarizeWorker` の `QueueState::next_seq` と同じ形）。
    fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// `session_dir` の最新のジョブが `seq` か。載っていなければ（取り消された）偽。
    fn is_current(&self, session_dir: &Path, seq: u64) -> bool {
        self.status
            .get(session_dir)
            .is_some_and(|(current, _)| *current == seq)
    }
}

/// キューへ流すジョブ。通番はワーカーが「自分が最新か」を確かめるために持ち回る。
struct QueuedJob {
    seq: u64,
    job: TranscribeJob,
}

/// `stop` の結果。押した側が、何が起きたかをログに書き分けられるようにする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopOutcome {
    /// 走っていたジョブに中断を伝えた。実際に降りるのはワーカーが気づいたとき。
    Stopping,
    /// キュー待ちだったジョブを取り消した（走り出す前なので即座に効く）。
    Cancelled,
    /// 走ってもキューにも載っていない（既に終わった・押される前に消えた）。
    NotRunning,
}

/// ジョブが使うモデルの表示名。上書き指定は**ファイル名だけ**にする（読む領域にそのまま出るので、
/// パスを漏らさない。`docs/rules/security.md`）。
fn job_model_label(job: &TranscribeJob) -> String {
    match &job.model_override {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Custom model".to_owned()),
        None => crate::whisper_model::spec_or_default(&job.model_id)
            .display_name
            .to_owned(),
    }
}

/// whisper の進捗を状態マップへ流す口。音源が複数あるジョブでは「何本目か」を足して
/// **ジョブ全体の割合**に均す（1 本目が終わった時点で 100% と出さない）。
#[derive(Clone)]
struct ProgressSink {
    queue: Arc<Mutex<QueueState>>,
    session_dir: PathBuf,
    /// このジョブの通番。**自分が最新のときだけ書く**（止めた直後に積み直された後続の
    /// 「文字起こし中」を、降りかけの古いジョブの進捗が上書きしないように）。
    seq: u64,
    /// この音源がジョブの何本目か（0 始まり）と、ジョブ全体の本数。
    index: usize,
    total: usize,
}

impl ProgressSink {
    /// whisper から来る 1 音源分の進捗（0〜100）を、ジョブ全体の割合として記録する。
    ///
    /// **進行中のエントリにしか書かない**。完了・失敗が先に書かれた後で遅れて届いた進捗が
    /// 状態を巻き戻さないようにする（whisper のコールバックは推論スレッドから来る）。
    ///
    /// **ここでパニックしない**こと（理由は `abort_if_stopped` の doc。同じ `extern "C"` の
    /// 境界で、whisper-rs のトランポリンは `catch_unwind` を挟まない）。いま安全なのは、
    /// ロックが poison を吸収し、`total == 0` を先に弾き、`as` が飽和キャストだから。
    /// `expect` や添字アクセスを足さないこと（`docs/rules/ffi.md`）。
    fn report(&self, file_percent: i32) {
        if self.total == 0 {
            return;
        }
        let within_file = f64::from(file_percent.clamp(0, 100)) / 100.0;
        let overall = ((self.index as f64 + within_file) / self.total as f64 * 100.0).round();
        let overall = overall.clamp(0.0, 100.0) as u8;
        let mut queue = lock_queue(&self.queue);
        // **型が「進行中のときだけ」を保証する**。完了・失敗のあとに遅れて届いた進捗は、
        // 書き込む先そのものが無い（以前はここが実行時のガードだった。#159）。止めるよう
        // 伝えた後（`Stopping`）も同じで、割合が戻って「まだ動いている」ようには見えない。
        if let Some((seq, TranscribeState::Transcribing { percent, .. })) =
            queue.status.get_mut(&self.session_dir)
            && *seq == self.seq
        {
            *percent = Some(overall);
        }
    }
}

/// この状態を「まだ終わっていないジョブ」として数えるか（`TranscribeWorker::has_pending_jobs`）。
///
/// **網羅 match**にしてあるので、状態を足したら扱いを書くまでコンパイルが通らない
/// （`docs/CONTEXT.md` にあるとおり、キュー待ちを分ける対称化は未対応＝将来足しうる。
/// `_ => false` にしておくと、その日にモデルの削除ガードが静かに外れる）。
fn counts_as_pending(status: TranscribeStatus) -> bool {
    match status {
        // 止めている最中も「まだ終わっていない」。ワーカーがまだファイルを触っているので、
        // ここで false にすると削除や再実行がその隙間に入る。
        TranscribeStatus::Transcribing | TranscribeStatus::Stopping => true,
        TranscribeStatus::Done | TranscribeStatus::Failed => false,
    }
}

impl TranscribeWorker {
    /// ワーカースレッドを起動する。スレッド生成に失敗しても常駐アプリは落とさず、
    /// 文字起こしだけを無効化してログを残す。
    ///
    /// スレッドは意図的に join しない（detach）: 文字起こしは数分かかりうるため、終了時に
    /// join するとアプリの終了がブロックされる。常駐終了時に処理中のジョブは中断される
    /// （ベストエフォート。#30 のスコープ）。
    ///
    /// `summarizer` は文字起こし成功後の要約投入に使う（このワーカーが所有し、停止フックから
    /// 直接投入しない。文字起こしの完了を待たずに要約を始めないための意図的な結合で、
    /// `PostProcessWorker` が `TranscribeWorker` を持つのと同じ形）。
    /// `slot` は要約ワーカーと共有する重い推論の実行権（`crate::inference_slot`）。
    pub fn start(
        downloader: crate::model_download::ModelDownloader,
        summarizer: crate::summarize::SummarizeWorker,
        slot: crate::inference_slot::InferenceSlot,
    ) -> Self {
        // whisper.cpp / GGML が stderr へ出す冗長な内部ログを止める（ログ backend の feature を
        // 有効にしていないため、フック先が無く事実上の無効化になる）。複数回呼んでも安全。
        whisper_rs::install_logging_hooks();
        let queue = Arc::new(Mutex::new(QueueState {
            status: HashMap::new(),
            next_seq: 0,
            running: None,
        }));
        let queue_for_worker = Arc::clone(&queue);
        let (tx, rx) = mpsc::channel::<QueuedJob>();
        let spawned = std::thread::Builder::new()
            .name("transcribe-worker".into())
            .spawn(move || {
                // 送信側（アプリ本体）が落ちてチャネルが閉じたら自然に終了する。
                while let Ok(QueuedJob { seq, mut job }) = rx.recv() {
                    let model_label = job_model_label(&job);
                    let cancel = Arc::new(AtomicBool::new(false));
                    // 走らせてよいかの判定と印を 1 つのロックで（理由は `claim_job` の doc）。
                    if !claim_job(
                        &queue_for_worker,
                        &job.session_dir,
                        seq,
                        &model_label,
                        &cancel,
                    ) {
                        // 止められたか、後続に追い越されたジョブ（`claim_job` の doc）。
                        println!("Skipping transcription because the job is no longer current");
                        continue;
                    }
                    // 文字起こし中のパニックでワーカースレッドを殺さない。死ぬと状態が
                    // `Transcribing` のまま残り、そのセッションは再起動まで Transcribe /
                    // Summarize / Delete がすべて無効になる（Library ウィンドウの
                    // `detail-files-in-use` / `detail-jobs-pending`）。失敗として記録し、
                    // 次のジョブは受け続ける（`SummarizeWorker` と同じ扱い）。
                    let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || run_job(&job, &downloader, &slot, &queue_for_worker, seq, &cancel),
                    )) {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            eprintln!(
                                "Skipping transcription because transcribing the session panicked"
                            );
                            JobOutcome::Failed(TranscribeFailure::Panicked)
                        }
                    };
                    // **止めた後に本物の失敗が重なっても、失敗としては出さない**。押した人に
                    // とって起きたことは「止めた」で、そこへ赤い「Transcription failed」を
                    // 出しても直しようがない（理由はログに残る）。
                    let outcome = if cancel.load(Ordering::Relaxed) {
                        outcome.stopped_instead_of_failed()
                    } else {
                        outcome
                    };
                    // 要約は「全音源の文字起こしに成功した」ときだけ続ける。部分的に失敗した
                    // 文字起こしから議事録を作ると、欠けたまま完成品に見えてしまう。止めた
                    // ジョブも同じで、続けると「止めたのに議事録が出てくる」ことになる。
                    let summarize = if outcome.keeps_summary() {
                        job.summarize.take()
                    } else {
                        None
                    };
                    apply_outcome(&queue_for_worker, &job.session_dir, seq, outcome);
                    // 状態マップのロックを放してから投入する（要約ワーカー側も同じ流儀で
                    // 自分の状態マップを触るため、ロックの入れ子を作らない）。
                    if let Some(summarize_job) = summarize {
                        summarizer.submit(summarize_job);
                    }
                }
            });
        match spawned {
            Ok(_handle) => Self {
                tx: Some(tx),
                queue,
            },
            Err(err) => {
                eprintln!(
                    "Disabling transcription because the worker thread failed to start: {err}"
                );
                Self { tx: None, queue }
            }
        }
    }

    /// ジョブを投入する。投入した時点でセッションを「文字起こし中」（キュー待ちを含む）として
    /// 記録する。ワーカーが動いていない場合はログのみ（録音自体は既に保存済み）。
    pub fn submit(&self, job: TranscribeJob) {
        let Some(tx) = &self.tx else {
            eprintln!("Skipping transcription because the transcription worker is not running");
            return;
        };
        let seq = {
            let mut queue = lock_queue(&self.queue);
            let seq = queue.next_seq();
            queue.status.insert(
                job.session_dir.clone(),
                (seq, TranscribeState::starting(job_model_label(&job))),
            );
            seq
        };
        // 送信失敗 = ワーカースレッドが（panic 等で）終了しレシーバが閉じた状態。
        // 記録した「文字起こし中」を取り消す（永遠に進行中表示のままにしない）。
        // ジョブは SendError から回収してキーの事前 clone を避ける。
        if let Err(mpsc::SendError(QueuedJob { job, .. })) = tx.send(QueuedJob { seq, job }) {
            eprintln!("Skipping transcription because the transcription worker is not running");
            let mut queue = lock_queue(&self.queue);
            // 自分が入れたエントリだけを消す（送信に失敗している間に積み直されていたら、
            // そちらの「文字起こし中」を消してしまう）。
            if queue.is_current(&job.session_dir, seq) {
                queue.status.remove(&job.session_dir);
            }
        }
    }

    /// 走っている（またはキュー待ちの）文字起こしを止める（#163）。
    ///
    /// **止めるのは失敗ではない**。降りたジョブは状態マップから消え、表示は JSON の有無ベース
    /// （未実施／生成済み）へ戻る。止めた後に本物の失敗が重なっても赤い失敗表示にはしない
    /// （`JobOutcome::stopped_instead_of_failed`）。
    ///
    /// **判定と実行は 1 回のロックでまとめる**。「走っているか」を別に問い合わせてから止めると、
    /// その隙間でワーカーがジョブを取り出し、「キュー待ちを取り消した」と答えたあとに走り出す
    /// （`SummarizeWorker::cancel_queued` と同じ理由）。
    ///
    /// 走っているジョブは即座には降りない。whisper の推論ループは abort コールバックで、
    /// 音源の切れ目は `run_job` が中断フラグを見て降りる。その間の表示は `Stopping`。
    #[must_use = "the caller decides what to log and when to redraw; dropping it hides a no-op"]
    pub fn stop(&self, session_dir: &Path) -> StopOutcome {
        let mut queue = lock_queue(&self.queue);
        let running = queue
            .running
            .as_ref()
            .filter(|running| running.session_dir == session_dir)
            .map(|running| {
                running.cancel.store(true, Ordering::Relaxed);
                (running.seq, running.model_label.clone())
            });
        if let Some((seq, model_label)) = running {
            // **エントリを走っているジョブへ書き戻す**。止めた時点で後続が積まれていても、
            // 通番が走っている側に戻るので後続は `claim_job` が捨てる（「走っているほうだけ
            // 止めて、後ろに積まれたほうが走り出す」を作らない）。降りたら
            // `apply_outcome(seq)` がこのエントリを消し、未実施／生成済みの表示へ戻る。
            //
            // 表示をここで `Stopping` へ移す理由は `TranscribeState::Stopping` の doc。
            queue.status.insert(
                session_dir.to_path_buf(),
                (seq, TranscribeState::Stopping { model_label }),
            );
            return StopOutcome::Stopping;
        }
        match queue
            .status
            .get(session_dir)
            .map(|(_, state)| state.status())
        {
            // キュー待ち（走っていないのに「文字起こし中」）は、その場でキューから外せる。
            // `mpsc` は積んだジョブを取り出せないので、エントリを消してワーカーに捨てさせる
            // （`StatusMap`）。
            Some(TranscribeStatus::Transcribing) => {
                queue.status.remove(session_dir);
                StopOutcome::Cancelled
            }
            _ => StopOutcome::NotRunning,
        }
    }

    /// セッションの進行状況。マップに載っていなければ `None`（表示側が JSON の有無で
    /// 「文字起こし前/完了」を解決する）。
    pub fn status_of(&self, session_dir: &Path) -> Option<TranscribeStatus> {
        // **`state_of` へ委譲しない**。これは一覧の全行が毎 tick 呼ぶ経路で、状態 1 つを読むのに
        // `model_label` と `reason` の確保を払うことになる。同じ 1 エントリを読むので、
        // 委譲しなくても状態と説明は食い違わない。
        lock_queue(&self.queue)
            .status
            .get(session_dir)
            .map(|(_, state)| state.status())
    }

    /// 一覧の行が要る分だけ（状態と進捗）を、**確保なしで**取る。
    ///
    /// `state_of` はモデル名まで clone するので、全行を毎 tick 回すこの経路には重い
    /// （`status_of` を `state_of` へ委譲しないのと同じ理由）。
    pub fn progress_of(&self, session_dir: &Path) -> Option<TranscribeProgress> {
        lock_queue(&self.queue)
            .status
            .get(session_dir)
            .map(|(_, state)| match state {
                TranscribeState::Transcribing { percent, .. } => {
                    TranscribeProgress::Transcribing { percent: *percent }
                }
                TranscribeState::Stopping { .. } => TranscribeProgress::Stopping,
                TranscribeState::Done { .. } => TranscribeProgress::Done,
                TranscribeState::Failed { .. } => TranscribeProgress::Failed,
            })
    }

    /// セッションの進行状況と、読む領域に出す中身（モデル名・進捗・失敗の理由）。
    ///
    /// **`status_of` はこれの一部を取り出したもの**なので、状態と説明が食い違わない
    /// （2 つのマップに分けると、片方だけ更新した瞬間にありえない組み合わせができる）。
    pub fn state_of(&self, session_dir: &Path) -> Option<TranscribeState> {
        lock_queue(&self.queue)
            .status
            .get(session_dir)
            .map(|(_, state)| state.clone())
    }

    /// 文字起こしのジョブが在るか（**キュー待ちを含む**。`TranscribeStatus::Transcribing` は
    /// `submit` の時点で入る）。モデル一覧の削除可否に使う（#117）。
    ///
    /// **どのモデルを使っているかは見ない**（ジョブは投入時点の設定を snapshot で持つので、
    /// 走っているジョブのモデルと現在の選択は違いうる）。whisper のモデルはジョブが読むので、
    /// ジョブが在る間は whisper 種別の行をまとめて削除不可にする（種別単位の粗い判定）。
    ///
    /// **範囲**: 数えるのは**投入済みのジョブ**だけ。後処理（`mixdown::PostProcessWorker` の
    /// 正規化）はまだ投入していないので数えず、その間はモデルを削除できる（消してもジョブは
    /// 失敗せず `ensure_model` が再取得する）。
    ///
    /// **限界**: ワーカースレッドがパニックで死ぬと状態が `Transcribing` のまま残るので
    /// （上の `catch_unwind` の doc）、その場合は再起動まで whisper のモデルを削除できない。
    pub fn has_pending_jobs(&self) -> bool {
        lock_queue(&self.queue)
            .status
            .values()
            .any(|(_, state)| counts_as_pending(state.status()))
    }

    /// セッションの進行状況の記録を破棄する（セッション削除時の掃除）。未登録なら何もしない。
    pub fn forget(&self, session_dir: &Path) {
        lock_queue(&self.queue).status.remove(session_dir);
    }
}

/// 状態マップのガードを取る。poison（ロック保持中のパニック）でも状態表示を止めないため、
/// ガードを取り出して続行する（`docs/rules/error-handling.md`）。
fn lock_queue(queue: &Mutex<QueueState>) -> MutexGuard<'_, QueueState> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// デキューしたジョブを走らせてよいかを決め、走らせるなら「処理中」と走っている印を立てる。
///
/// **判定と遷移を 1 つのクリティカルセクションで行う**のが要点。別々にすると、その隙間に
/// 入った `stop` が「キュー待ちを取り消した」と答えたあとでジョブが走り出す
/// （`SummarizeWorker` と同じ形）。マップに自分の通番が載っていなければ、止められたか
/// 後続に追い越されたジョブ（`StatusMap`）。
///
/// **エントリは書き換えない**。通番と状態の照合を通った時点で、そこに在るのは `submit` が
/// 入れた「文字起こし中（割合なし）」そのものだと分かっている（通番はジョブごとに 1 つで、
/// 進捗は自分の通番のときしか書かれない）。以前は「先行ジョブの完了が後続の処理中表示を
/// 上書きしたままにならないように」入れ直していたが、それは通番の照合が引き受けた。
fn claim_job(
    queue: &Mutex<QueueState>,
    session_dir: &Path,
    seq: u64,
    model_label: &str,
    cancel: &Arc<AtomicBool>,
) -> bool {
    let mut queue = lock_queue(queue);
    // **通番と状態の両方を見る**。通番だけだと、`stop` が走っているジョブへ書き戻した
    // `Stopping` を「自分のものだ」と見なして走り出せてしまう（状態を足したときに静かに
    // 壊れないための備え）。
    if !queue.is_current(session_dir, seq)
        || !matches!(
            queue.status.get(session_dir),
            Some((_, TranscribeState::Transcribing { .. }))
        )
    {
        return false;
    }
    queue.running = Some(RunningJob {
        session_dir: session_dir.to_path_buf(),
        seq,
        model_label: model_label.to_owned(),
        cancel: Arc::clone(cancel),
    });
    true
}

/// ジョブの結果を状態マップへ反映し、走っている印を外す。
///
/// **後から積まれたジョブの状態を上書きしない**。止めた直後に積み直されていれば、マップの
/// 通番はもう自分のものではない——そこへ書くと、キューに載っているのに Delete が開く。
///
/// 走っている印は通番に関係なく外す。外し忘れると、次に押した Stop が終わったジョブへ中断を
/// 伝えて空振りする。ここを通るのはワーカースレッドだけなので、印はまだ自分のもの。
fn apply_outcome(queue: &Mutex<QueueState>, session_dir: &Path, seq: u64, outcome: JobOutcome) {
    let mut queue = lock_queue(queue);
    queue.running = None;
    if !queue.is_current(session_dir, seq) {
        return;
    }
    match outcome {
        // 対象なしで何もしなかった場合と、止めた場合は「投入済み」の痕跡を消し、表示を
        // JSON の有無ベース（未実施／生成済み）へ戻す。止めたのは失敗ではないので、赤い
        // 失敗表示にはしない。
        JobOutcome::Skipped | JobOutcome::Stopped => {
            queue.status.remove(session_dir);
        }
        // **走り終わった印は必ず残す**（#176）。消して JSON の有無ベースへ戻すと、一度も
        // 文字起こししていなかった録音では `has_transcript` がまだ false のままなので、
        // 一覧が「not transcribed」に落ちて次の走査まで戻らない（`main` の tick は
        // 「ワーカーから降りて Done になった」ときだけ立て直す）。食い違いは印に載せる。
        JobOutcome::Done { shortfall } => {
            queue.status.insert(
                session_dir.to_path_buf(),
                (seq, TranscribeState::Done { shortfall }),
            );
        }
        JobOutcome::Failed(reason) => {
            queue.status.insert(
                session_dir.to_path_buf(),
                (seq, TranscribeState::Failed { reason }),
            );
        }
    }
}

/// 1 ジョブの処理結果（状態マップへの反映用）。
enum JobOutcome {
    /// 全音源の文字起こしに成功した。`shortfall` は**失敗として数えない食い違い**
    /// （#176。読み飛ばしのあった音源）——ここに来るのは `HasGaps` だけで、途中で終わった
    /// 音源は `Failed` へ行く。
    Done {
        shortfall: Option<TranscriptShortfall>,
    },
    /// 少なくとも 1 音源が失敗した（モデル準備の失敗を含む）。
    Failed(TranscribeFailure),
    /// 対象なしで何もしなかった。
    Skipped,
    /// 止められて降りた（#163）。**失敗ではない**ので、状態は消して未実施へ戻す。
    Stopped,
}

impl JobOutcome {
    /// この結果のあと、積んであった要約を続けてよいか。
    ///
    /// **全音源の文字起こしに成功したときだけ**。部分的に失敗した文字起こしから議事録を作ると、
    /// 欠けたまま完成品に見えてしまう。止めたジョブも同じで、続けると「止めたのに議事録が
    /// 出てくる」ことになる。
    ///
    /// **読み飛ばしがあっても続ける**（#176）。ここで止めると、押した「Transcribe, then write
    /// notes」が黙って消える（落ちたことを言う先が無い）うえ、読み飛ばしは一時的な読み取り
    /// 失敗でも起きるので 1 パケットで自動議事録が丸ごと止まる。欠けた入力から書いたことは
    /// ディスクの印が残し、議事録タブが先に言う（`SummaryPane::NotesFromPartialTranscript`）
    /// ——#175 が「途中で終わっている入力」に対して採ったのと同じ扱いに揃える。書いた議事録
    /// 自体に出典を残すのは #184。
    ///
    /// **ワイルドカードを置かない**（結果を足したら扱いを書くまで通らない）。
    fn keeps_summary(&self) -> bool {
        match self {
            Self::Done { .. } => true,
            Self::Failed(_) | Self::Skipped | Self::Stopped => false,
        }
    }

    /// 止められたジョブの結果を畳む。**失敗は「止めた」に丸める**。
    ///
    /// 止めた後に本物の失敗が重なる経路が実在する（モデルの取得やロードが失敗する、最後の
    /// 音源のデコードが失敗する）。そこで赤い「Transcription failed」を出しても、押した人に
    /// とって起きたことは「止めた」なので直しようがない。失敗の理由はログに残る。
    ///
    /// **ワイルドカードを置かない**（結果を足したら扱いを書くまで通らない）。
    fn stopped_instead_of_failed(self) -> Self {
        match self {
            Self::Failed(_) | Self::Stopped => Self::Stopped,
            Self::Done { shortfall } => Self::Done { shortfall },
            Self::Skipped => Self::Skipped,
        }
    }
}

/// 1 ジョブ（1 回の録音停止分）を処理する。モデルはジョブ内で 1 回だけロードして
/// 複数音源で使い回す（モデルのロードが重いため）。音源単位の失敗は他の音源へ波及させない。
fn run_job(
    job: &TranscribeJob,
    downloader: &crate::model_download::ModelDownloader,
    slot: &crate::inference_slot::InferenceSlot,
    queue: &Arc<Mutex<QueueState>>,
    seq: u64,
    cancel: &Arc<AtomicBool>,
) -> JobOutcome {
    if job.audio_paths.is_empty() {
        // 対象なしでモデル（数百 MB〜）をロードしない防御。通常は投入側が空を渡さない。
        return JobOutcome::Skipped;
    }
    // **重い処理の手前で毎回フラグを見る**。この先はモデルの取得（分オーダーになりうる）・
    // 推論スロットの待ち（要約 LLM が握っていれば分オーダー）・モデルのロードと、止められ
    // ないまま数分固まる区間が続く。#163 の動機がまさに「間違ったモデルで流し始めた」＝
    // ダウンロード中なので、そこで効かないと意味がない。
    if cancel.load(Ordering::Relaxed) {
        return JobOutcome::Stopped;
    }
    // モデルを解決する。上書き指定があればそれを、無ければ設定で選ばれた内蔵モデルを使う
    // （カタログ外の手編集値は既定へフォールバック。未取得ならここで自動ダウンロードする。
    // UI 起点のダウンロード中なら完了を待つ。ワーカースレッド上なので分オーダーかかっても
    // UI は塞がない）。
    let model_path = match &job.model_override {
        Some(path) => path.clone(),
        None => {
            let spec = crate::whisper_model::spec_or_default(&job.model_id);
            match downloader.ensure_model(spec) {
                Ok(path) => path,
                Err(err) => {
                    eprintln!(
                        "Skipping transcription because the Whisper model could not be prepared: {err}"
                    );
                    return JobOutcome::Failed(TranscribeFailure::ModelDownload);
                }
            }
        }
    };
    // 取得が終わった直後（ダウンロード自体の打ち切りは `ModelDownloader` の領分）。
    if cancel.load(Ordering::Relaxed) {
        return JobOutcome::Stopped;
    }
    if !model_path.is_file() {
        eprintln!(
            "Skipping transcription because the Whisper model file was not found: {}",
            model_path.display()
        );
        return JobOutcome::Failed(TranscribeFailure::ModelMissing);
    }
    let Some(model_path_str) = model_path.to_str() else {
        eprintln!("Skipping transcription because the Whisper model path is not valid UTF-8");
        return JobOutcome::Failed(TranscribeFailure::ModelUnreadable);
    };
    // ここから先が重い区間。要約 LLM と同時に走らせない（`crate::inference_slot`）。
    // モデルの準備（ダウンロード）はスロットの外で済ませてある。
    let _slot = slot.acquire();
    // スロットが空くまで待たされた後（要約 LLM が長く握っていることがある）。
    if cancel.load(Ordering::Relaxed) {
        return JobOutcome::Stopped;
    }
    let ctx = match WhisperContext::new_with_params(
        model_path_str,
        WhisperContextParameters::default(),
    ) {
        Ok(ctx) => ctx,
        Err(err) => {
            eprintln!(
                "Skipping transcription because loading the Whisper model failed ({}): {err}",
                model_path.display()
            );
            return JobOutcome::Failed(TranscribeFailure::ModelLoad);
        }
    };
    // モデルのロードが終わった直後（数百 MB〜数 GB を読むので秒〜十数秒かかる）。
    if cancel.load(Ordering::Relaxed) {
        return JobOutcome::Stopped;
    }
    // 音源ごとの結果を集める。**1 つのジョブ結果へまとめるのは `job_outcome`**（#176）——
    // ループの中で組み立てると、ワーカーを回さないと通らない繋ぎになり、テストから丸ごと
    // 呼べなくなる（`docs/rules/testing.md` の「繋いでいる関数は、呼べるなら丸ごと呼ぶ」）。
    let mut results: Vec<(String, FileOutcome)> = Vec::new();
    let total = job.audio_paths.len();
    for (index, path) in job.audio_paths.iter().enumerate() {
        // 音源の切れ目でも降りる。whisper の推論に入る前（デコード・リサンプル）で止められた
        // ときは abort コールバックが呼ばれないので、ここが受け口になる。
        if cancel.load(Ordering::Relaxed) {
            return JobOutcome::Stopped;
        }
        let name = audio_display_name(path);
        let progress = ProgressSink {
            queue: Arc::clone(queue),
            session_dir: job.session_dir.clone(),
            seq,
            index,
            total,
        };
        let outcome = match transcribe_file(&ctx, path, &model_path, job, progress, cancel) {
            Ok(outcome) => outcome,
            Err(err) => {
                eprintln!("Skipping transcription of {name} because it failed: {err}");
                FileOutcome::NotKept
            }
        };
        report_file_outcome(&name, &outcome);
        let stopped = matches!(outcome, FileOutcome::Stopped);
        results.push((name, outcome));
        if stopped {
            break;
        }
    }
    job_outcome(results)
}

/// 音源 1 本の結果をログに出す（進み具合が音源ごとに出るように、まとめる前に呼ぶ）。
///
/// **ファイル名だけを出す**（`docs/rules/security.md`）。名前を作るのは `audio_display_name`。
fn report_file_outcome(name: &str, outcome: &FileOutcome) {
    match outcome {
        FileOutcome::Kept {
            segments,
            shortfall: None,
            ..
        } => println!("Transcribed {name} ({segments} segments)"),
        FileOutcome::Kept {
            segments,
            shortfall: Some(shortfall),
            kept_upto,
        } => {
            if shortfall.stops_partway() {
                eprintln!(
                    "Keeping only the first {} of {name} ({segments} segments) because it could \
                     not be read further",
                    crate::reading_pane::format_elapsed(*kept_upto)
                );
            } else {
                // 読み飛ばしの件数は `decode_mp3_stream` が既に 1 行出している。ここは
                // 「残した結果が抜けている」ことだけを言う。
                println!("Transcribed {name} ({segments} segments) with gaps");
            }
        }
        FileOutcome::NotKept => eprintln!(
            "Nothing was kept from {name} because what is already there is no worse or nothing \
             could be recognized"
        ),
        FileOutcome::Stopped => eprintln!("Stopped before {name} was saved"),
    }
}

/// 音源ごとの結果を 1 つのジョブ結果へまとめる（#176）。
///
/// **ここが繋ぎの本体**。`run_job` は whisper のモデルを要求するのでテストから呼べないが、
/// この関数は呼べる——「読み飛ばした音源があったジョブは、失敗ではないが食い違いを持つ」
/// という判断を、丸ごと検査できる場所に置く（`docs/rules/testing.md`）。
///
/// **ワイルドカードを置かない**（結果を足したら扱いを書くまで通らない）。
fn job_outcome(results: Vec<(String, FileOutcome)>) -> JobOutcome {
    // 最後まで行かなかった音源を集める（文にするのは読む領域の仕事。`TranscribeFailure`）。
    let mut failed: Vec<FailedSource> = Vec::new();
    // **読める文字起こしを残せたか**を覚える（本数ではない。`TranscribeFailure::Files`）。
    // 1 件も認識できなかった音源は数えない——残っていると言っても、開く行が無い。
    let mut kept_other_sources = false;
    // 全音源をまとめた食い違い。**失敗として数えない食い違い（読み飛ばし）だけがここに残る**
    // ——途中で終わった音源は `failed` へ行き、ジョブごと `Failed` になる。
    let mut shortfall: Option<TranscriptShortfall> = None;
    for (name, outcome) in results {
        match outcome {
            // 止められたのは失敗ではない。**他の音源の結果より優先する**（止めた人にとって
            // 起きたことは「止めた」なので、赤い失敗を出しても直しようがない）。
            FileOutcome::Stopped => return JobOutcome::Stopped,
            FileOutcome::Kept {
                segments,
                shortfall: None,
                ..
            } => kept_other_sources |= segments > 0,
            FileOutcome::Kept {
                segments,
                shortfall: Some(of_source),
                kept_upto,
            } => {
                if of_source.stops_partway() {
                    // 音源を最後まで読めなかった（#164）。読めた範囲は保存済みだが、**この
                    // 音源は最後まで行っていない**ので失敗として数える。
                    //
                    // **抜けがあるときは位置を言わない**（#176）——`kept_upto` は読み飛ばした
                    // ぶん前へ詰まっていて、音声の位置ではない。
                    let kept = if of_source.has_gaps() {
                        KeptFromSource::SomeWithGaps
                    } else {
                        KeptFromSource::Upto(kept_upto)
                    };
                    failed.push(FailedSource::new(name, kept));
                } else {
                    kept_other_sources |= segments > 0;
                    // **読む行が無いなら食い違いを言わない**（#176）。言うと、開いても何も
                    // 現れない `Show partial` を出すことになる（`kept_partial` の保証と同じ）。
                    if segments > 0 {
                        shortfall = Some(TranscriptShortfall::with_gaps(shortfall));
                    }
                }
            }
            // 読めた範囲があっても残さなかった（`is_worth_writing` が理由を持つ）。
            // この音源からは何も残らないので、途中結果としては数えない。
            FileOutcome::NotKept => failed.push(FailedSource::new(name, KeptFromSource::Nothing)),
        }
    }
    if failed.is_empty() {
        JobOutcome::Done { shortfall }
    } else {
        JobOutcome::Failed(TranscribeFailure::Files {
            failed,
            kept_other_sources,
        })
    }
}

/// 1 音源の処理結果。**「止められた」を `Err` に混ぜない**——混ぜると、止めたことが失敗の
/// 一覧（`TranscribeFailure::Files`）へ載って赤く出る。
enum FileOutcome {
    /// 文字起こしを保存した（#164 / #176）。
    Kept {
        segments: usize,
        /// 録音との食い違い（`None` は最後まで読めて読み飛ばしも無かった）。
        shortfall: Option<TranscriptShortfall>,
        /// 保存した範囲の終わり。**読み飛ばしがあると音声の位置ではない**（抜けたぶん前へ
        /// 詰まる）ので、出すかどうかは `job_outcome` が `shortfall` を見て決める。
        kept_upto: Duration,
    },
    /// **保存しなかった**（#164 / #176。理由は `is_worth_writing`）。この音源からは何も残らない。
    NotKept,
    /// 止められたので保存せずに降りた。
    Stopped,
}

/// 録音と食い違う文字起こしを、保存する価値があるか（#164 / #176）。
///
/// **食い違いのある結果はすべてここを通る**（#176）。`complete` だけで無条件に書いていた頃は、
/// 一度できた完成品を、あとから壊れたファイルの「抜けた」結果が黙って上書きできた。
///
/// **保存しない条件をここ 1 箇所に置く**ので、呼び出し側は理由を数え上げずに済む。
///
/// **「在るかどうか」では弾かない**。存在だけで弾くと、同じ音源をやり直すたびに `NotKept` へ
/// 落ちて、途中結果を伏せる仕組み（`kept_partial`）が Try again 一発で外れる。
///
/// 判断は 3 段:
///
/// 1. **途中で終わっていて、1 件も認識できていない**なら書かない（#164）。書いても読む行が
///    無く、それでも「残っている」と言うと、押しても何も現れない `Show partial` を出すことに
///    なる（`TranscribeFailure::kept_partial`）。**抜けているだけのものは別**（#176）——
///    最後まで読めているので、行が無いのは「話していなかった」でもありうる。
/// 2. すでに在るものが**届いているほう**なら置き換えない。食い違いが無ければ完成品なので
///    無条件に守り（#175）、食い違いがあっても「最後まで届いている」ものを「途中で終わって
///    いる」もので潰さない（#176）。音源がもう最後まで読めない以上、上書きは取り返しがつかない。
/// 3. **どちらも途中で終わっているときだけ**、どこまで届いているかで比べる。比べるのは JSON へ
///    書く値そのもの（`read_upto_secs`）——表示用に秒へ丸めた値で比べると、やり直しが必ず
///    「前のほうが長い」に転ぶ。**読み飛ばしを跨いで比べない**（#176）: `duration_secs` は
///    得られたサンプル数から出すので、抜けたぶん短くなり、音声の位置ではなくなる。
fn is_worth_writing(
    shortfall: TranscriptShortfall,
    segments: usize,
    transcript_path: &Path,
    read_upto_secs: f64,
) -> bool {
    if segments == 0 && shortfall.stops_partway() {
        return false;
    }
    // まだ何も無い（または読めない）ので、書いて困るものが無い。
    let Some(stored) = crate::transcript::stored_reach(transcript_path) else {
        return true;
    };
    // **食い違いの無い保存物は降格させない**（#175）。長さが同じでも「最後まで読めた」が
    // 「途中で終わった」「抜けている」に変わるのは降格で、やり直しでは戻らない。
    let Some(stored_shortfall) = stored.shortfall else {
        return false;
    };
    // 在るほうは最後まで届いていて、新しいほうは届いていない。潰さない。
    if !stored_shortfall.stops_partway() && shortfall.stops_partway() {
        return false;
    }
    // どちらかが最後まで届いているなら、長さでは比べない（比べても意味が揃わない）。
    if !stored_shortfall.stops_partway() || !shortfall.stops_partway() {
        return true;
    }
    // どちらも途中で終わっている。長さが読めなければ守るものが分からないので、書いてよい。
    stored
        .duration_secs
        .is_none_or(|existing| existing <= read_upto_secs)
}

/// whisper の推論ループから降りるための abort コールバック。`true` を返すと whisper.cpp が
/// グラフの計算を打ち切る。
///
/// **`FullParams::set_abort_callback_safe` は使えない**（whisper-rs 0.16.0 のバグ）。あちらは
/// 閉包を `Box<Box<dyn FnMut() -> bool>>` として確保しておきながら、トランポリンを閉包の具体型で
/// 単相化する（`trampoline::<F>`）。C 側から返ってくるポインタが指すのは外側の `Box` なので、
/// `*mut F` として読むと別の型を読むことになる。進捗側（`set_progress_callback_safe`）は
/// `trampoline::<Box<dyn FnMut(i32)>>` と正しく書かれていて、abort 側だけが取り違えている。
/// クレートを上げるときに直っているか確かめること。
///
/// **ここでパニックしないこと**。呼び出し元は whisper.cpp の C フレームで、`extern "C"` の
/// 境界を巻き戻しが越えようとするとプロセスが abort する（未定義動作ではないが、常駐アプリを
/// その場で失う。`docs/rules/ffi.md`）。`load` はパニックしない。
///
/// # Safety
///
/// `user_data` は生きている `AtomicBool` を指しているか、ヌルであること。呼び出し側
/// （`transcribe_file`）は `Arc<AtomicBool>` を `full()` の呼び出しより長く持ち続ける。
unsafe extern "C" fn abort_if_stopped(user_data: *mut std::ffi::c_void) -> bool {
    if user_data.is_null() {
        return false;
    }
    // Safety: 呼び出し側の契約（上）により、生きている `AtomicBool` を指す。
    let cancel = unsafe { &*(user_data as *const AtomicBool) };
    cancel.load(Ordering::Relaxed)
}

/// 音源を指す**ファイル名だけ**を取り出す。ログにも読む領域にも出るので、ディレクトリ成分を
/// 混ぜない（`docs/rules/security.md`）。**この保証を作っているのはここだけ**なので、テストで
/// 固定してある。
fn audio_display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_owned())
}

/// 1 音源を文字起こしして `<音源名>.json` に保存する。
/// `model_path` は `run_job` で解決済みのモデル（JSON の `model` フィールド用）。
///
/// **音源を最後まで読めたかどうかで戻り値が分かれる**（#164）。読めた範囲だけを保存した
/// ときは `Partial` を返し、呼び出し側がそれを失敗として数える。
fn transcribe_file(
    ctx: &WhisperContext,
    audio_path: &Path,
    model_path: &Path,
    job: &TranscribeJob,
    progress: ProgressSink,
    cancel: &Arc<AtomicBool>,
) -> Result<FileOutcome, Box<dyn std::error::Error>> {
    let source = audio_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("audio")
        .to_owned();

    // 中間バッファ（インターリーブ全量→モノラル）は各段階へ move で渡し、次の段階を作った時点で
    // 解放する。長時間録音では中間バッファが GB 級になり、秒〜分オーダーの whisper 推論中に
    // 抱え続けるとメモリピークが跳ね上がるため（推論中に生きるのは 16kHz モノラルの `pcm` のみ）。
    let DecodedAudio {
        samples,
        sample_rate,
        channels,
        shortfall,
    } = decode_mp3(audio_path)?;
    let mono = downmix_to_mono(samples, channels);
    let pcm = resample_to_whisper_rate(mono, sample_rate)?;
    let duration_secs = pcm.len() as f64 / WHISPER_SAMPLE_RATE as f64;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    // ターミナルへの whisper 自身の逐次出力は使わない（結果は JSON に保存する）。
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    params.set_translate(false);
    // 読む領域に割合を出すため、whisper の進捗を状態マップへ流す（#154）。コールバックは推論
    // スレッドから来るので、`ProgressSink` 側で「進行中のエントリにだけ書く」ことを守る。
    //
    // **このクロージャは解放されない**（whisper-rs 0.16 は `Box::into_raw` で C 側へ預けたきり、
    // 落とす持ち主を持たない）。音源 1 本につき数十バイトずつ積むので、重いものや機微なものを
    // 捕まえないこと。クレートを上げるときに直っているか確かめる。
    params.set_progress_callback_safe(move |percent| progress.report(percent));
    // 言語は設定 TOML（手編集されうる信頼境界外）由来。whisper-rs の set_language は NUL バイトを
    // 含む文字列で panic するため（内部の CString::new が expect）、ここで弾いて whisper の既定
    // （en）へフォールバックする。未知の言語コードは whisper.cpp 側が検証して full() が Err を
    // 返すので、ここでは NUL だけ防げばよい。`auto` は whisper.cpp が自動判定として解釈する
    // 特別値のため、そのまま渡す（set_language を呼ばないと whisper の既定 en になり、
    // 自動判定にはならない）。
    if job.language.contains('\0') {
        eprintln!("Ignoring the configured transcription language because it contains a NUL byte");
    } else {
        params.set_language(Some(&job.language));
    }

    // 推論ループから降りる口（#163）。`params` はこのあと `full` へ move されるが、
    // C 側へ渡すのは `cancel` が指す `AtomicBool` のアドレスで、それは `full` が返るまで
    // このスコープの `Arc` が生かしている。
    //
    // Safety: `abort_if_stopped` の契約どおり、生きている `AtomicBool` を渡す。
    unsafe {
        params.set_abort_callback(Some(abort_if_stopped));
        params.set_abort_callback_user_data(Arc::as_ptr(cancel) as *mut std::ffi::c_void);
    }

    let mut state = ctx.create_state()?;
    let full_result = state.full(params, &pcm);
    // **中断フラグを先に見る**。打ち切られた `full` が `Ok` を返すか `Err` を返すかは
    // whisper.cpp 側の都合なので、どちらでも「止められた」を優先する（`Err` のまま流すと
    // 失敗として赤く出る。`Ok` のまま流すと途中までの結果を完成品として保存する）。
    if cancel.load(Ordering::Relaxed) {
        // 打ち切りの `Err` は失敗として扱わないが、黙って捨てない
        // （`docs/rules/error-handling.md`）。止めた瞬間に本物の失敗が重なっていたら、
        // ここだけが痕跡になる。
        if let Err(err) = full_result {
            eprintln!("Ignoring the Whisper error because the transcription was stopped: {err}");
        }
        return Ok(FileOutcome::Stopped);
    }
    full_result?;

    let segments = collect_segments(&state);
    let marks = TranscriptShortfall::marks(shortfall);
    let result = Transcription {
        source,
        model: model_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        language: job.language.clone(),
        duration_secs,
        complete: marks.reached_the_end,
        gapped: marks.gapped,
        segments,
    };
    let json_path = audio_path.with_extension("json");
    let segments = result.segments.len();
    // 読む領域に出すのは秒まで（`format_elapsed`）。**サンプル数から整数で出す**ので、
    // 保存の可否を決める `duration_secs` とは別の値になる（用途が違うので分けてある）。
    let kept_upto = Duration::from_secs(pcm.len() as u64 / WHISPER_SAMPLE_RATE as u64);
    // **保存する値そのものを見て分かれる**（#175）。手元の `shortfall` を読むと、JSON には
    // 「完成」と書いたのに呼び出し側へは食い違いを返す、という組み合わせを書ける。
    let Some(shortfall) = result.shortfall() else {
        write_transcription(&json_path, &result)?;
        return Ok(FileOutcome::Kept {
            segments,
            shortfall: None,
            kept_upto,
        });
    };
    // 録音と食い違う結果（#164 / #176）。**残す価値があるときだけ**保存する。判断に渡すのは
    // **JSON へ書く値そのもの**——表示用に丸めた値で比べると、やり直しが必ず「前のほうが
    // 長い」に転ぶ（`is_worth_writing` の doc）。
    if !is_worth_writing(shortfall, segments, &json_path, duration_secs) {
        eprintln!(
            "Not saving the transcript of {} because what is already there is no worse",
            audio_display_name(audio_path)
        );
        return Ok(FileOutcome::NotKept);
    }
    write_transcription(&json_path, &result)?;
    Ok(FileOutcome::Kept {
        segments,
        shortfall: Some(shortfall),
        kept_upto,
    })
}

/// 文字起こし結果の保存形式。録音一覧ビュー（`src/transcript.rs`）が読む契約なので、`segments` の
/// `start` / `end`（秒）/ `text` の形は互換を保って変更する。
#[derive(Debug, Serialize)]
struct Transcription {
    /// 音源の別（`mic` / `system`。音声ファイル名の拡張子抜き）。
    source: String,
    /// 使用した whisper モデルのファイル名。
    model: String,
    /// 認識言語。自動判定は `auto`。
    language: String,
    /// 音源を**最後まで読めたか**（#175）。`false` は `duration_secs` までで打ち切った途中結果。
    ///
    /// **この欄が無い古い JSON は「最後まで読めた」として読む**（読む側の `serde(default)`）。
    /// 打ち切って残す仕組みが入ったのは #164 なので、それ以前の出力は最後まで読めたときにしか
    /// 作られていない。
    complete: bool,
    /// 壊れたパケットを**読み飛ばした**か（#176）。`true` は中身が抜けていて、抜けたぶん以降の
    /// 時刻が本来より早いことを表す。`complete` とは独立——最後まで読めていても抜けはありうる。
    ///
    /// **読む側と同じ関数で組み直す**（`Transcription::shortfall` / `TranscriptFile::shortfall`）
    /// ので、片方だけ極性が反転する壊れ方が無い。
    gapped: bool,
    /// **どこまでの音源から作られたか**（秒）。最後まで読めた音源では音声全体の長さになり、
    /// 途中で読めなくなった音源では**その打ち切り位置**になる（#164）。読む側
    /// （`transcript::stored_reach`）は、途中結果を上書きしてよいかの判断に
    /// この値を使う。
    duration_secs: f64,
    /// 発話セグメント（時刻順）。
    segments: Vec<Segment>,
}

impl Transcription {
    /// **書いた値そのものから**録音との食い違いを組み直す（#175 / #176）。呼び出し側の分岐が
    /// これを通ることで、JSON には「完成」と書いたのに戻り値では食い違いを言う、という
    /// 組み合わせを書けなくなる（`docs/rules/coding-conventions.md` の「値を 2 度読まず、
    /// 書いた値そのもので分岐する」）。読む側の対は `transcript::TranscriptFile::shortfall`。
    fn shortfall(&self) -> Option<TranscriptShortfall> {
        TranscriptShortfall::from_marks(ShortfallMarks {
            reached_the_end: self.complete,
            gapped: self.gapped,
        })
    }
}

/// 1 発話セグメント。時刻はセッション先頭からの秒。
#[derive(Debug, Serialize)]
struct Segment {
    start: f64,
    end: f64,
    text: String,
}

/// whisper の認識結果からセグメント列を集める。テキストの不正な UTF-8 は置換文字（U+FFFD）に
/// 置き換えられ（`to_str_lossy`）、（稀な）ヌルポインタ取得の失敗時のみ空文字にして続行する
/// （1 セグメントのために全体を失敗させない）。
fn collect_segments(state: &whisper_rs::WhisperState) -> Vec<Segment> {
    (0..state.full_n_segments())
        .filter_map(|i| state.get_segment(i))
        .map(|segment| Segment {
            start: centiseconds_to_secs(segment.start_timestamp()),
            end: centiseconds_to_secs(segment.end_timestamp()),
            text: segment
                .to_str_lossy()
                .map(|text| text.trim().to_owned())
                .unwrap_or_default(),
        })
        .collect()
}

/// whisper のタイムスタンプ（センチ秒）を秒へ変換する。
fn centiseconds_to_secs(centiseconds: i64) -> f64 {
    centiseconds as f64 / CENTISECONDS_PER_SEC
}

/// デコード済み音声（インターリーブ f32 PCM）。
struct DecodedAudio {
    samples: Vec<f32>,
    sample_rate: u32,
    channels: usize,
    /// この音源と録音の食い違い（#164 / #176）。`None` は最後まで読めて読み飛ばしも無かった。
    shortfall: Option<TranscriptShortfall>,
}

/// MP3 をデコードしてインターリーブ f32 PCM を得る。
///
/// 対象は自アプリが保存した録音ファイルだが、保存後にユーザーが差し替え・破損させる可能性は
/// あるため、途中のパケットのデコード失敗はスキップして読める部分だけを使う（symphonia の
/// 推奨に従う）。1 サンプルも得られなければエラー。
///
/// **途中で読めなくなっても、そこまでを捨てない**（#164）。読めた範囲を返し、録音とどう
/// 食い違っているかを `shortfall` で伝える。1 時間の会議が 55 分目で読めなくなっていると
/// き、55 分ぶんを捨てる理由は無い。
fn decode_mp3(path: &Path) -> Result<DecodedAudio, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
    decode_mp3_stream(stream, &audio_display_name(path))
}

/// `decode_mp3` の本体。**入力ストリームを引数で受ける**のは、「途中で読めなくなった」を
/// 決定的に作れるようにするため（`docs/rules/testing.md` の「重い処理そのものを引数で受ける」）。
/// 実ファイルでは再現できない——MP3 は末尾を切っても symphonia がストリーム終端として扱い、
/// 短い音源として正常にデコードされる。
///
/// `name` はログに出す**ファイル名だけ**（`docs/rules/security.md`）。
fn decode_mp3_stream(
    stream: MediaSourceStream,
    name: &str,
) -> Result<DecodedAudio, Box<dyn std::error::Error>> {
    let mut hint = Hint::new();
    hint.with_extension("mp3");

    let mut format = symphonia::default::get_probe().probe(
        &hint,
        stream,
        FormatOptions::default(),
        MetadataOptions::default(),
    )?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or("no audio track found")?;
    let codec_params = track
        .codec_params
        .as_ref()
        .ok_or("missing codec parameters")?
        .audio()
        .ok_or("not an audio codec")?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(codec_params, &AudioDecoderOptions::default())?;
    let track_id = track.id;

    let mut samples: Vec<f32> = Vec::new();
    let mut sample_rate = 0u32;
    let mut channels = 0usize;
    // 録音とどう食い違ったか（#176）。**重ねる操作は 1 つずつ名前を持つ**ので、
    // 「途中で終わった」と「中が抜けた」を取り違えて組む書き方が無い。
    let mut shortfall: Option<TranscriptShortfall> = None;
    // 壊れていて読み飛ばしたパケットの数（下の `continue` の doc）。ログにだけ出す。
    let mut skipped_packets = 0usize;
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break, // ストリーム終端。
            // **「そこから先が読めなかった」と言い切れる種別だけ**を打ち切りにする（#164）。
            // 正常な終端は `Ok(None)` で来るので、ここへ来る `IoError` は本当に読めなかった
            // とき（途中で切れた・デバイスが応答しない）。読む領域に出るのも
            // `could not be read past 04:12` で、原因まで断定はしていない。
            //
            // 他の種別（`Unsupported` / `LimitError` など）は「音源がそこで終わった」ではない
            // ので従来どおり伝播する。**理由は捨てない**（`docs/rules/error-handling.md`）——
            // これが唯一の痕跡になる。
            Err(SymphoniaError::IoError(err)) => {
                eprintln!("Stopping the decode of {name} because it could not be read on: {err}");
                shortfall = Some(TranscriptShortfall::with_stop(shortfall));
                break;
            }
            Err(err) => return Err(err.into()),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buffer) => {
                let spec = buffer.spec();
                if channels == 0 {
                    // 最初のデコード成功パケットの形式で固定する。
                    sample_rate = spec.rate();
                    channels = spec.channels().count();
                } else if spec.rate() != sample_rate || spec.channels().count() != channels {
                    // 途中でレート/チャンネル数が変わるファイルは、無検知で連結すると
                    // フレーム境界がずれて壊れた音声になる。ここまでを返して打ち切る（#164）。
                    eprintln!(
                        "Stopping the decode of {name} because the audio parameters changed \
                         mid-stream"
                    );
                    shortfall = Some(TranscriptShortfall::with_stop(shortfall));
                    break;
                }
                // 中間バッファを介さず samples の末尾へ直接書き、全量の二重コピーを避ける。
                let base = samples.len();
                samples.resize(base + buffer.samples_interleaved(), 0.0);
                buffer.copy_to_slice_interleaved(&mut samples[base..]);
            }
            // 壊れたパケットはスキップして続行（symphonia の推奨ハンドリング）。
            //
            // **「途中で終わった」とは別の食い違いとして数える**（#176）。ここは最後まで
            // 読めていて途中が抜けているだけなので、「could not be read past ◯◯」では事実と
            // 違う。抜けたぶん以降のサンプルは前へ詰まるので時刻もずれる——だからこそ
            // 完成品と区別する必要がある（#164 は数えてログに出すだけだった）。
            //
            // **`DecodeError` と `IoError` を分けない**。前者は音源が壊れている、後者は
            // 読み取りが一時的に失敗した、という違いはあるが、**残った文字起こしにとっては
            // どちらも同じ「抜け」**。読む領域も、やり直しで直るかを断定しない文言にしてある
            // （`TranscriptShortfall::HasGaps`）。
            //
            // 数はまとめて 1 行だけログに出す（パケットごとには出さない。1 音源で数千回に
            // なりうる）。
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => {
                skipped_packets += 1;
                continue;
            }
            Err(err) => return Err(err.into()),
        }
    }
    if skipped_packets > 0 {
        eprintln!(
            "Continuing with {name} because {skipped_packets} broken packets could be skipped"
        );
        shortfall = Some(TranscriptShortfall::with_gaps(shortfall));
    }
    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return Err("no audio samples could be decoded".into());
    }
    Ok(DecodedAudio {
        samples,
        sample_rate,
        channels,
        shortfall,
    })
}

/// インターリーブ PCM をチャンネル平均でモノラルへ落とす純粋関数。
/// 末尾にチャンネル数へ満たない端数サンプルがあれば捨てる（1 フレーム未満の欠けは無視できる）。
/// 入力は move で受け、モノラルはコピーせずそのまま返す。複数チャンネルでは元バッファを
/// この関数内で解放する（長時間録音の全量コピー・二重保持を避ける）。
fn downmix_to_mono(samples: Vec<f32>, channels: usize) -> Vec<f32> {
    if channels <= 1 {
        return samples;
    }
    samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().sum::<f32>() / channels as f32)
        .collect()
}

/// モノラル PCM を whisper のサンプルレート（16kHz）へリサンプルする。
/// 入力は move で受け、元がすでに 16kHz ならコピーせずそのまま返す。リサンプル時は元バッファを
/// この関数内で解放する。品質はアンチエイリアス込みの FFT リサンプラ（rubato）に任せる。
fn resample_to_whisper_rate(
    mono: Vec<f32>,
    sample_rate: u32,
) -> Result<Vec<f32>, Box<dyn std::error::Error>> {
    if sample_rate as usize == WHISPER_SAMPLE_RATE {
        return Ok(mono);
    }
    let mut resampler = Fft::<f32>::new(
        sample_rate as usize,
        WHISPER_SAMPLE_RATE,
        RESAMPLE_CHUNK_FRAMES,
        1,
        FixedSync::Input,
    )?;
    let input = InterleavedSlice::new(&mono, 1, mono.len())?;
    let output = resampler.process_all(&input, mono.len(), None)?;
    Ok(output.take_data())
}

/// 文字起こし結果を JSON で保存する。録音と同じ機微データなので所有者のみ読み書き可で作る
/// （`crate::private_file`。やり直しで既存ファイルを上書きする経路があるため、モードを
/// 揃え直す必要がある）。
fn write_transcription(
    path: &Path,
    result: &Transcription,
) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(result)?;
    crate::private_file::write(path, json.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::summarize::{SummarizeWorker, SummaryEngine};

    /// 音源の名前は**ファイル名だけ**になる。この保証は `audio_display_name` の 1 箇所が
    /// 作っていて、そのまま失敗の理由として画面に出る（`docs/rules/security.md`）。
    #[test]
    fn audio_display_name_drops_the_directories() {
        assert_eq!(
            audio_display_name(Path::new(
                "/Users/someone/Recordings/20260810-140200/mic.mp3"
            )),
            "mic.mp3"
        );
        // 取り出せない形でも、パスを落として当たり障りのない名前にする。
        assert_eq!(audio_display_name(Path::new("/")), "audio");
        assert_eq!(audio_display_name(Path::new("..")), "audio");
    }

    /// モデル名も**ファイル名だけ**になる。上書き指定は任意のパスを取れて、その値は走っている
    /// 間の本文（`{model} is running on this Mac…`）としてそのまま画面に出る
    /// （`docs/rules/security.md`。`audio_display_name` と対）。
    #[test]
    fn job_model_label_drops_the_directories() {
        let job = |model_override: Option<&str>| TranscribeJob {
            session_dir: PathBuf::from("/tmp/shoki-label"),
            audio_paths: Vec::new(),
            model_id: crate::whisper_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: model_override.map(PathBuf::from),
            language: "en".to_owned(),
            summarize: None,
        };

        assert_eq!(
            job_model_label(&job(Some("/Users/someone/models/ggml-medium.bin"))),
            "ggml-medium.bin"
        );
        // 取り出せない形は当たり障りのない名前へ落とす（パスを出さない）。
        assert_eq!(job_model_label(&job(Some("/"))), "Custom model");
        // 上書きが無ければカタログの表示名。
        assert_eq!(
            job_model_label(&job(None)),
            crate::whisper_model::default_spec().display_name
        );
    }

    /// テスト用の状態（状態だけ指定し、ペイロードは既定で埋める）。
    fn test_state(status: TranscribeStatus) -> TranscribeState {
        match status {
            TranscribeStatus::Transcribing => TranscribeState::Transcribing {
                model_label: "Small".to_owned(),
                percent: None,
            },
            TranscribeStatus::Stopping => TranscribeState::Stopping {
                model_label: "Small".to_owned(),
            },
            TranscribeStatus::Done => TranscribeState::Done { shortfall: None },
            TranscribeStatus::Failed => TranscribeState::Failed {
                reason: TranscribeFailure::ModelMissing,
            },
        }
    }

    /// テスト用の空のキュー（ワーカースレッドを立てずに状態だけを見る）。
    fn test_queue() -> Arc<Mutex<QueueState>> {
        Arc::new(Mutex::new(QueueState {
            status: StatusMap::new(),
            next_seq: 0,
            running: None,
        }))
    }

    /// テスト用の要約ワーカー。ジョブが渡ったかどうかは状態マップの有無で判定する。
    fn summarize_worker() -> SummarizeWorker {
        SummarizeWorker::start(
            crate::model_download::ModelDownloader::new(),
            crate::inference_slot::InferenceSlot::new(),
        )
    }

    /// テスト用の要約依頼（存在しないモデル上書きパスなので、実行されても即失敗する）。
    fn summarize_job(session_dir: &Path) -> crate::summarize::SummarizeJob {
        crate::summarize::SummarizeJob {
            session_dir: session_dir.to_path_buf(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(session_dir.join("missing-model.gguf")),
            language: "en".to_owned(),
            existing_is_stale: true,
        }
    }

    /// 削除ガードが読む述語（`has_pending_jobs`）が数える状態を、**全バリアント**で固定する。
    /// `Transcribing` は `submit` の時点で入るので、キュー待ちのジョブも守られる。
    #[test]
    fn counts_as_pending_covers_all_states() {
        assert!(counts_as_pending(TranscribeStatus::Transcribing));
        assert!(counts_as_pending(TranscribeStatus::Stopping));
        assert!(!counts_as_pending(TranscribeStatus::Done));
        assert!(!counts_as_pending(TranscribeStatus::Failed));
    }

    /// ワーカー越しでも同じ判定が効くこと（状態マップを直接組んで、whisper を走らせずに見る）。
    #[test]
    fn has_pending_jobs_reads_the_status_map() {
        let worker = TranscribeWorker::start(
            crate::model_download::ModelDownloader::new(),
            crate::summarize::SummarizeWorker::start(
                crate::model_download::ModelDownloader::new(),
                crate::inference_slot::InferenceSlot::new(),
            ),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = std::path::PathBuf::from("/tmp/shoki-transcribe-pending");
        assert!(!worker.has_pending_jobs(), "an empty queue is not pending");

        lock_queue(&worker.queue)
            .status
            .insert(dir.clone(), (0, TranscribeState::starting("Small".into())));
        assert!(
            worker.has_pending_jobs(),
            "Transcribing counts as a pending job"
        );
        // 止めている最中も数える（まだワーカーがファイルを触りうる）。
        lock_queue(&worker.queue).status.insert(
            dir.clone(),
            (
                0,
                TranscribeState::Stopping {
                    model_label: "Small".into(),
                },
            ),
        );
        assert!(
            worker.has_pending_jobs(),
            "Stopping counts as a pending job"
        );
        // 終わったジョブは数えない（消してよい）。
        for status in [TranscribeStatus::Done, TranscribeStatus::Failed] {
            lock_queue(&worker.queue)
                .status
                .insert(dir.clone(), (0, test_state(status)));
            assert!(
                !worker.has_pending_jobs(),
                "{status:?} must not count as a pending job"
            );
        }
    }

    /// キュー待ちのジョブは、走り出す前ならその場で外せる（#163）。**状態は消える**——
    /// 止めたのは失敗ではないので、表示は JSON の有無ベース（未実施／生成済み）へ戻る。
    #[test]
    fn stop_drops_a_job_that_has_not_started() {
        let worker = TranscribeWorker::start(
            crate::model_download::ModelDownloader::new(),
            summarize_worker(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = PathBuf::from("/tmp/shoki-stop-queued");
        // ワーカーが取り出す前の状態を作る（走っている印は立てない）。
        lock_queue(&worker.queue)
            .status
            .insert(dir.clone(), (0, TranscribeState::starting("Small".into())));

        assert_eq!(worker.stop(&dir), StopOutcome::Cancelled);
        assert_eq!(
            worker.status_of(&dir),
            None,
            "a cancelled job must not leave a status behind"
        );
        assert!(
            !worker.has_pending_jobs(),
            "a cancelled job must stop blocking Delete and Transcribe"
        );
        // 2 回目は何も無い。
        assert_eq!(worker.stop(&dir), StopOutcome::NotRunning);
    }

    /// 走っているジョブには中断を伝えるだけで、状態は消さない（#163）。降りるまでの間は
    /// `Stopping` で、**保留として数え続ける**（ワーカーがまだ JSON を触りうる）。
    #[test]
    fn stop_flags_a_running_job_and_shows_stopping() {
        let worker = TranscribeWorker::start(
            crate::model_download::ModelDownloader::new(),
            summarize_worker(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = PathBuf::from("/tmp/shoki-stop-running");
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut queue = lock_queue(&worker.queue);
            queue
                .status
                .insert(dir.clone(), (0, TranscribeState::starting("Small".into())));
            queue.running = Some(RunningJob {
                session_dir: dir.clone(),
                seq: 0,
                model_label: "Small".to_owned(),
                cancel: Arc::clone(&cancel),
            });
        }

        assert_eq!(worker.stop(&dir), StopOutcome::Stopping);
        assert!(
            cancel.load(Ordering::Relaxed),
            "the running job must be told to come down"
        );
        assert_eq!(worker.status_of(&dir), Some(TranscribeStatus::Stopping));
        assert!(
            worker.has_pending_jobs(),
            "the worker is still holding the files until it comes down"
        );
        // 使っているモデルは「止めています」の理由に出るので、持ち越す。
        assert_eq!(
            worker.state_of(&dir),
            Some(TranscribeState::Stopping {
                model_label: "Small".to_owned(),
            })
        );
    }

    /// 別のセッションが走っていても、そのジョブは止めない。
    #[test]
    fn stop_only_touches_the_session_it_was_asked_about() {
        let worker = TranscribeWorker::start(
            crate::model_download::ModelDownloader::new(),
            summarize_worker(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let running = PathBuf::from("/tmp/shoki-stop-other-running");
        let asked = PathBuf::from("/tmp/shoki-stop-other-asked");
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut queue = lock_queue(&worker.queue);
            queue.status.insert(
                running.clone(),
                (0, TranscribeState::starting("Small".into())),
            );
            queue.running = Some(RunningJob {
                session_dir: running.clone(),
                seq: 0,
                model_label: "Small".to_owned(),
                cancel: Arc::clone(&cancel),
            });
        }

        assert_eq!(worker.stop(&asked), StopOutcome::NotRunning);
        assert!(
            !cancel.load(Ordering::Relaxed),
            "stopping one session must not come down on another"
        );
        assert_eq!(
            worker.status_of(&running),
            Some(TranscribeStatus::Transcribing)
        );
    }

    /// 止めた直後に積み直したジョブの状態を、降りかけの古いジョブの後始末が上書きしない
    /// （#163）。**ワーカーの後始末そのもの**を通す——通番の照合を別に確かめても、
    /// 後始末がそれを使っていなければ意味がない。
    #[test]
    fn a_finished_job_does_not_overwrite_the_one_that_replaced_it() {
        let queue = test_queue();
        let dir = PathBuf::from("/tmp/shoki-stop-resubmit");
        // 通番 0 のジョブが走っている。止められて、通番 1 で積み直された。
        lock_queue(&queue)
            .status
            .insert(dir.clone(), (1, TranscribeState::starting("Medium".into())));
        lock_queue(&queue).running = Some(RunningJob {
            session_dir: dir.clone(),
            seq: 0,
            model_label: "Small".to_owned(),
            cancel: Arc::new(AtomicBool::new(false)),
        });

        // 古いジョブがいま降りてきた。
        apply_outcome(&queue, &dir, 0, JobOutcome::Stopped);

        assert_eq!(
            lock_queue(&queue)
                .status
                .get(&dir)
                .map(|(seq, state)| (*seq, state.clone())),
            Some((1, TranscribeState::starting("Medium".into()))),
            "the job that replaced the stopped one must keep its own status"
        );
        assert!(
            lock_queue(&queue).running.is_none(),
            "the running mark must come off even when the status belongs to someone else"
        );
    }

    /// 自分が最新なら、結果はそのまま入る（上の照合が**常に**偽になっていないこと）。
    #[test]
    fn a_finished_job_writes_its_own_outcome() {
        let queue = test_queue();
        let dir = PathBuf::from("/tmp/shoki-outcome");
        lock_queue(&queue)
            .status
            .insert(dir.clone(), (3, TranscribeState::starting("Small".into())));

        apply_outcome(&queue, &dir, 3, JobOutcome::Done { shortfall: None });
        assert_eq!(
            lock_queue(&queue)
                .status
                .get(&dir)
                .map(|(_, state)| state.clone()),
            Some(TranscribeState::Done { shortfall: None })
        );

        // 止めたジョブは記録ごと消える（未実施／生成済みの表示へ戻す）。
        lock_queue(&queue)
            .status
            .insert(dir.clone(), (4, TranscribeState::starting("Small".into())));
        apply_outcome(&queue, &dir, 4, JobOutcome::Stopped);
        assert_eq!(lock_queue(&queue).status.get(&dir), None);
    }

    /// 取り消されたジョブはワーカーが取り出しても走らせない（#163）。**キューから外す**手段が
    /// これしかない（`mpsc` は積んだジョブを取り出せない）ので、ここが通らないと
    /// 「取り消した」と答えた後にジョブが走る。
    #[test]
    fn a_cancelled_job_is_dropped_when_the_worker_picks_it_up() {
        let queue = test_queue();
        let dir = PathBuf::from("/tmp/shoki-claim");
        let cancel = Arc::new(AtomicBool::new(false));

        // 状態マップに載っていない = 取り消された（または追い越された）。
        assert!(
            !claim_job(&queue, &dir, 0, "Small", &cancel),
            "a job that is no longer in the map must not run"
        );
        assert!(
            lock_queue(&queue).running.is_none(),
            "a job that was not claimed must not look like it is running"
        );

        // 載っていれば走り出し、走っている印が立つ（Stop の宛先になる）。
        lock_queue(&queue)
            .status
            .insert(dir.clone(), (0, TranscribeState::starting("Small".into())));
        assert!(claim_job(&queue, &dir, 0, "Small", &cancel));
        assert_eq!(
            lock_queue(&queue)
                .running
                .as_ref()
                .map(|running| running.session_dir.clone()),
            Some(dir)
        );
    }

    /// 走っているジョブの後ろに積み直されたジョブが在っても、**両方止まる**（#163）。
    ///
    /// 走っている側にだけ中断を伝えて後続を放置すると、Stop を押したのに後続がそのまま走り
    /// 出す（表示も `Stopping…` から `Transcribing…` へ戻る）。通番を走っている側へ書き戻す
    /// ことで、後続は `claim_job` が捨てる。
    #[test]
    fn stop_also_drops_a_job_queued_behind_the_running_one() {
        let worker = TranscribeWorker::start(
            crate::model_download::ModelDownloader::new(),
            summarize_worker(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = PathBuf::from("/tmp/shoki-stop-successor");
        let cancel = Arc::new(AtomicBool::new(false));
        {
            let mut queue = lock_queue(&worker.queue);
            // 通番 0 が走っていて、通番 1 が積み直されている（マップは後続のもの）。
            queue.running = Some(RunningJob {
                session_dir: dir.clone(),
                seq: 0,
                model_label: "Small".to_owned(),
                cancel: Arc::clone(&cancel),
            });
            queue
                .status
                .insert(dir.clone(), (1, TranscribeState::starting("Medium".into())));
        }

        assert_eq!(worker.stop(&dir), StopOutcome::Stopping);
        assert!(
            cancel.load(Ordering::Relaxed),
            "the running job must be told to come down"
        );
        // 走っているモデルの名前で「止めています」を出す（マップに載っていたのは後続の名前）。
        assert_eq!(
            worker.state_of(&dir),
            Some(TranscribeState::Stopping {
                model_label: "Small".to_owned(),
            })
        );
        // 後続はワーカーが取り出しても走らない。
        assert!(
            !claim_job(
                &worker.queue,
                &dir,
                1,
                "Medium",
                &Arc::new(AtomicBool::new(false))
            ),
            "the job queued behind the stopped one must not start"
        );
        // 走っていたジョブが降りたら、記録ごと消えて未実施／生成済みの表示へ戻る。
        apply_outcome(&worker.queue, &dir, 0, JobOutcome::Stopped);
        assert_eq!(worker.status_of(&dir), None);
    }

    /// 止めるよう伝えたエントリを、ワーカーが「自分のものだ」と見なして走り出さないこと。
    /// 通番だけの照合では通ってしまう組み合わせなので、状態も見ている。
    #[test]
    fn a_job_that_was_told_to_stop_is_not_started() {
        let queue = test_queue();
        let dir = PathBuf::from("/tmp/shoki-claim-stopping");
        lock_queue(&queue).status.insert(
            dir.clone(),
            (
                7,
                TranscribeState::Stopping {
                    model_label: "Small".to_owned(),
                },
            ),
        );
        assert!(!claim_job(
            &queue,
            &dir,
            7,
            "Small",
            &Arc::new(AtomicBool::new(false))
        ));
    }

    /// 一覧の行が読む経路でも、止めている最中は割合を出さない（#163）。
    #[test]
    fn the_row_shows_no_percentage_while_stopping() {
        let worker = TranscribeWorker::start(
            crate::model_download::ModelDownloader::new(),
            summarize_worker(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = PathBuf::from("/tmp/shoki-progress-stopping");
        lock_queue(&worker.queue).status.insert(
            dir.clone(),
            (
                0,
                TranscribeState::Stopping {
                    model_label: "Small".to_owned(),
                },
            ),
        );
        let progress = worker.progress_of(&dir).expect("the entry should exist");
        assert_eq!(progress, TranscribeProgress::Stopping);
        assert_eq!(progress.status(), TranscribeStatus::Stopping);
        assert_eq!(
            progress.percent(),
            None,
            "the progress after deciding to stop tells the reader nothing"
        );
    }

    /// 止めたジョブから議事録を続けない（#163）。**全結果を網羅**で固定する——ここが緩むと
    /// 「止めたのに議事録が出てくる」が静かに戻る。
    #[test]
    fn only_a_complete_transcription_keeps_its_summary() {
        assert!(JobOutcome::Done { shortfall: None }.keeps_summary());
        assert!(!JobOutcome::Stopped.keeps_summary());
        assert!(!JobOutcome::Skipped.keeps_summary());
        assert!(
            !JobOutcome::Failed(TranscribeFailure::Panicked).keeps_summary(),
            "a partial transcription would make an incomplete summary look finished"
        );
        // 途中まで読めた音源も同じ（#164）。読める結果が残っていても、欠けたものを入力に
        // すると「欠けたまま完成品に見える議事録」ができる。
        assert!(
            !JobOutcome::Failed(TranscribeFailure::Files {
                failed: vec![FailedSource::new(
                    "mic.mp3",
                    KeptFromSource::Upto(Duration::from_secs(252))
                )],
                kept_other_sources: true,
            })
            .keeps_summary(),
            "notes must not be written from a transcript that stops partway"
        );
        // **読み飛ばしは止めない**（#176）。止めると押した「Transcribe, then write notes」が
        // 黙って消えるうえ、一時的な読み取り失敗 1 件で自動議事録が丸ごと止まる。欠けた入力
        // から書いたことは、ディスクの印と議事録タブの言い分が伝える（#175 と同じ扱い）。
        assert!(
            JobOutcome::Done {
                shortfall: Some(TranscriptShortfall::HasGaps)
            }
            .keeps_summary(),
            "gaps warn on the Notes tab instead of dropping the request"
        );
    }

    /// 止めた後に本物の失敗が重なっても、失敗としては出さない（#163）。**全結果を網羅**で
    /// 固定する——ここが緩むと「止めたのに赤くなる」が静かに戻る。
    #[test]
    fn a_failure_that_lands_after_a_stop_is_reported_as_stopped() {
        assert!(matches!(
            JobOutcome::Failed(TranscribeFailure::ModelDownload).stopped_instead_of_failed(),
            JobOutcome::Stopped
        ));
        assert!(matches!(
            JobOutcome::Stopped.stopped_instead_of_failed(),
            JobOutcome::Stopped
        ));
        // 止めるより先に終わっていたものは、そのまま（食い違いも保つ）。
        assert!(matches!(
            JobOutcome::Done { shortfall: None }.stopped_instead_of_failed(),
            JobOutcome::Done { shortfall: None }
        ));
        assert!(matches!(
            JobOutcome::Done {
                shortfall: Some(TranscriptShortfall::HasGaps)
            }
            .stopped_instead_of_failed(),
            JobOutcome::Done {
                shortfall: Some(TranscriptShortfall::HasGaps)
            }
        ));
        assert!(matches!(
            JobOutcome::Skipped.stopped_instead_of_failed(),
            JobOutcome::Skipped
        ));
    }

    /// 止められたジョブは、**重い準備を始める前に**降りる（#163）。モデルの取得・推論スロット
    /// 待ち・モデルのロードはどれも分オーダーになりうるので、そこへ入ってからでは遅い。
    ///
    /// 存在しないモデル上書きパスを渡してあるので、降りずに進めば `Failed` になる——
    /// `Stopped` が返ることが、準備より手前で降りた証拠になる。
    #[test]
    fn a_stopped_job_comes_down_before_it_loads_anything() {
        let dir = PathBuf::from("/tmp/shoki-stop-before-loading");
        let outcome = run_job(
            &TranscribeJob {
                session_dir: dir.clone(),
                audio_paths: vec![dir.join("mic.mp3")],
                model_id: crate::whisper_model::DEFAULT_MODEL_ID.to_owned(),
                model_override: Some(PathBuf::from("/tmp/shoki-no-such-model.bin")),
                language: "en".to_owned(),
                summarize: None,
            },
            &crate::model_download::ModelDownloader::new(),
            &crate::inference_slot::InferenceSlot::new(),
            &test_queue(),
            0,
            &Arc::new(AtomicBool::new(true)),
        );
        assert!(
            matches!(outcome, JobOutcome::Stopped),
            "a job that was stopped must not load the model"
        );
    }

    /// 降りかけの古いジョブから遅れて届いた進捗が、積み直した新しいジョブの表示を動かさない
    /// （#163）。通番が違えば書かない。
    #[test]
    fn progress_sink_ignores_a_job_that_was_replaced() {
        let queue = test_queue();
        let dir = PathBuf::from("/tmp/shoki-progress-replaced");
        lock_queue(&queue)
            .status
            .insert(dir.clone(), (1, TranscribeState::starting("Medium".into())));
        ProgressSink {
            queue: Arc::clone(&queue),
            session_dir: dir.clone(),
            seq: 0,
            index: 0,
            total: 1,
        }
        .report(80);
        assert_eq!(
            lock_queue(&queue)
                .status
                .get(&dir)
                .map(|(_, state)| state.clone()),
            Some(TranscribeState::starting("Medium".into())),
            "the replaced job must not write into the entry that replaced it"
        );
    }

    /// 手動再実行・状態表示の土台となる状態マップのライフサイクルを、whisper モデルなしで
    /// 検証する。存在しないモデル上書きパスを渡すと、ネットワークに触れず即 Failed になる。
    #[test]
    fn submit_tracks_status_until_failure() {
        let worker = TranscribeWorker::start(
            crate::model_download::ModelDownloader::new(),
            summarize_worker(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = std::env::temp_dir().join(format!("shoki-status-{}", std::process::id()));
        worker.submit(TranscribeJob {
            session_dir: dir.clone(),
            audio_paths: vec![dir.join("mic.mp3")],
            model_id: "small".to_owned(),
            model_override: Some(dir.join("missing-model.bin")),
            language: "en".to_owned(),
            summarize: None,
        });
        // 投入直後は「文字起こし中」（ワーカーが速ければもう Failed でもよい）。
        assert!(matches!(
            worker.status_of(&dir),
            Some(TranscribeStatus::Transcribing) | Some(TranscribeStatus::Failed)
        ));
        // 最終的に Failed へ収束する。無限ポーリングにしない（`docs/rules/error-handling.md`）。
        for _ in 0..200 {
            if worker.status_of(&dir) == Some(TranscribeStatus::Failed) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(worker.status_of(&dir), Some(TranscribeStatus::Failed));
        // 読む領域は理由を出す（#154）。
        let state = worker
            .state_of(&dir)
            .expect("the session should have a state");
        // **理由は種別で持つ**（文言は読む領域が組む。#159）。
        assert_eq!(
            state,
            TranscribeState::Failed {
                reason: TranscribeFailure::ModelMissing,
            },
            "a failed job must carry why it failed"
        );
        // セッション削除時の掃除（forget）で記録が消える。
        worker.forget(&dir);
        assert_eq!(worker.status_of(&dir), None);
        assert_eq!(worker.state_of(&dir), None);
    }

    /// 進捗は**ジョブ全体**の割合になる（1 本目が終わった時点で 100% と出さない）。
    #[test]
    fn progress_sink_averages_across_audio_files() {
        let queue = test_queue();
        let dir = PathBuf::from("/tmp/shoki-progress");
        lock_queue(&queue)
            .status
            .insert(dir.clone(), (0, TranscribeState::starting("Small".into())));
        let percent_now = || match lock_queue(&queue).status.get(&dir) {
            Some((_, TranscribeState::Transcribing { percent, .. })) => *percent,
            other => panic!("the entry should still be transcribing, got {other:?}"),
        };

        let first = ProgressSink {
            queue: Arc::clone(&queue),
            session_dir: dir.clone(),
            seq: 0,
            index: 0,
            total: 2,
        };
        first.report(0);
        assert_eq!(percent_now(), Some(0));
        first.report(100);
        assert_eq!(
            percent_now(),
            Some(50),
            "finishing the first file is half of the job"
        );

        let second = ProgressSink {
            index: 1,
            ..first.clone()
        };
        second.report(50);
        assert_eq!(percent_now(), Some(75));
        // whisper から範囲外が来ても 0〜100 に収める。
        second.report(400);
        assert_eq!(percent_now(), Some(100));
        second.report(-1);
        assert_eq!(percent_now(), Some(50));
    }

    /// 終わった後に遅れて届いた進捗が、完了・失敗の表示を巻き戻さないこと。
    #[test]
    fn progress_sink_ignores_sessions_that_already_finished() {
        let queue = test_queue();
        let dir = PathBuf::from("/tmp/shoki-progress-late");
        lock_queue(&queue)
            .status
            .insert(dir.clone(), (0, test_state(TranscribeStatus::Failed)));
        ProgressSink {
            queue: Arc::clone(&queue),
            session_dir: dir.clone(),
            seq: 0,
            index: 0,
            total: 1,
        }
        .report(80);
        let state = lock_queue(&queue)
            .status
            .get(&dir)
            .map(|(_, state)| state.clone())
            .expect("the entry should exist");
        // **型が巻き戻しを塞ぐ**（#159）。`Failed` には進捗を入れる場所そのものが無い。
        assert!(matches!(state, TranscribeState::Failed { .. }));
    }

    /// 対象音源なし（Skipped）の投入は状態を残さない（「文字起こし中」のまま固まらない）。
    /// あわせて、成功していない文字起こしから要約が始まらないことを確認する。
    #[test]
    fn submit_with_no_audio_clears_status() {
        let summarizer = summarize_worker();
        let worker = TranscribeWorker::start(
            crate::model_download::ModelDownloader::new(),
            summarizer.clone(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = std::env::temp_dir().join(format!("shoki-skip-{}", std::process::id()));
        // 文字起こし JSON を置いておく。こうしておくと、もし要約が投入されてしまった場合は
        // 「対象なしで状態を消す」経路ではなく Failed（＝消えない痕跡）に落ちるので、
        // 下の「投入されていない」判定がタイミングに左右されなくなる。
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
        std::fs::write(
            dir.join("mic.json"),
            r#"{"segments":[{"start":0.0,"end":1.0,"text":"hello"}]}"#,
        )
        .expect("the transcript should be writable");

        worker.submit(TranscribeJob {
            session_dir: dir.clone(),
            audio_paths: Vec::new(),
            model_id: "small".to_owned(),
            model_override: None,
            language: "en".to_owned(),
            // 要約の依頼は添えるが、文字起こしが成功していないので投入されてはいけない。
            summarize: Some(summarize_job(&dir)),
        });
        for _ in 0..200 {
            if worker.status_of(&dir).is_none() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(worker.status_of(&dir), None);
        // 要約ワーカーは触られていない（投入されていれば submit が同期的に Queued を
        // 記録し、その後 Failed になる。どちらも消えない）。状態が「付かないこと」の確認なので、
        // 文字起こし側の完了から少しだけ猶予を置いて見る。
        for _ in 0..20 {
            assert_eq!(
                summarizer.status_of(&dir),
                None,
                "a skipped transcription must not start a summary"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn downmix_passes_through_mono() {
        let samples = vec![0.1, -0.2, 0.3];
        assert_eq!(downmix_to_mono(samples.clone(), 1), samples);
    }

    #[test]
    fn downmix_averages_stereo_frames() {
        // (0.2+0.4)/2=0.3, (-0.5+0.5)/2=0.0
        let samples = vec![0.2, 0.4, -0.5, 0.5];
        assert_eq!(downmix_to_mono(samples, 2), vec![0.3, 0.0]);
    }

    #[test]
    fn downmix_drops_trailing_partial_frame() {
        // 端数の 0.9 は 1 フレームに満たないため捨てる。
        let samples = vec![0.2, 0.4, 0.9];
        assert_eq!(downmix_to_mono(samples, 2), vec![0.3]);
    }

    #[test]
    fn centiseconds_convert_to_secs() {
        assert_eq!(centiseconds_to_secs(0), 0.0);
        assert_eq!(centiseconds_to_secs(150), 1.5);
        assert_eq!(centiseconds_to_secs(12_345), 123.45);
    }

    #[test]
    fn resample_passes_through_16khz() {
        let mono = vec![0.5f32; 1600];
        let out =
            resample_to_whisper_rate(mono.clone(), 16_000).expect("resampling should succeed");
        assert_eq!(out, mono);
    }

    #[test]
    fn resample_48khz_preserves_length_ratio_and_energy() {
        // 440Hz サイン波 1 秒。48kHz→16kHz は 1/3 の長さになり、可聴帯域の信号なので
        // エネルギー（RMS ≈ 振幅/√2）もほぼ保たれる（全ゼロ入力だと長さしか検証できない）。
        let amplitude = 0.5f32;
        let mono: Vec<f32> = (0..48_000)
            .map(|i| {
                let t = i as f32 / 48_000.0;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * amplitude
            })
            .collect();
        let out = resample_to_whisper_rate(mono, 48_000).expect("resampling should succeed");

        // 長さ: process_all は開始遅延をトリムするため、ほぼ厳密に 1/3 になる。
        let expected = 16_000usize;
        let diff = out.len().abs_diff(expected);
        assert!(
            diff <= expected / 100,
            "expected ~{expected} samples, got {}",
            out.len()
        );

        // エネルギー: サイン波の RMS は振幅/√2。リサンプル後も 5% 以内で保たれる。
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        let expected_rms = amplitude / 2.0f32.sqrt();
        assert!(
            (rms - expected_rms).abs() < expected_rms * 0.05,
            "expected RMS ~{expected_rms}, got {rms}"
        );
    }

    /// 指定のレート・秒数の 440Hz サイン波（モノラル）を MP3 にエンコードする。デコードの
    /// テストは実データが要る（合成 PCM を直接渡すと MP3 のフレーム構造を通らない）。
    fn sine_mp3(sample_rate: u32, seconds: u32) -> Vec<u8> {
        let pcm: Vec<i16> = (0..(sample_rate * seconds) as usize)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                ((2.0 * std::f32::consts::PI * 440.0 * t).sin() * 8000.0) as i16
            })
            .collect();
        let mut builder =
            mp3lame_encoder::Builder::new().expect("creating the LAME builder should succeed");
        builder.set_num_channels(1).expect("channels");
        builder.set_sample_rate(sample_rate).expect("sample rate");
        let mut encoder = builder
            .build()
            .expect("building the encoder should succeed");
        let mut mp3 = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.len()));
        encoder
            .encode_to_vec(mp3lame_encoder::MonoPcm(&pcm), &mut mp3)
            .expect("encoding should succeed");
        mp3.reserve(mp3lame_encoder::max_required_buffer_size(0));
        encoder
            .flush_to_vec::<mp3lame_encoder::FlushNoGap>(&mut mp3)
            .expect("flushing should succeed");
        mp3
    }

    /// 途中から読めなくなるストリーム。`limit` バイトを渡したあとは I/O エラーを返す。
    ///
    /// 実ファイルでは「途中で読めなくなった」を作れない（`decode_mp3_stream` の doc）ので、
    /// 継ぎ目にこれを流し込む。
    struct StopsReadingAfter {
        bytes: std::io::Cursor<Vec<u8>>,
        /// これ以上は渡さないバイト数。**ちょうどここで切る**——1 ブロックぶん超えて渡すと、
        /// 名前と挙動がずれる。
        limit: u64,
    }

    impl std::io::Read for StopsReadingAfter {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let left = self.limit.saturating_sub(self.bytes.position());
            if left == 0 {
                return Err(std::io::Error::other("the audio could not be read further"));
            }
            let allowed = usize::try_from(left).unwrap_or(usize::MAX).min(buf.len());
            self.bytes.read(&mut buf[..allowed])
        }
    }

    impl std::io::Seek for StopsReadingAfter {
        /// `is_seekable()` が false なので symphonia はここを呼ばない
        /// （`MediaSourceStream` が自前のバッファ内で巻き戻す）。呼ばれたら、渡した
        /// バイト数を数えている `limit` の意味が崩れるので、素直に断る。
        fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
            Err(std::io::Error::from(std::io::ErrorKind::Unsupported))
        }
    }

    impl symphonia::core::io::MediaSource for StopsReadingAfter {
        fn is_seekable(&self) -> bool {
            false
        }

        fn byte_len(&self) -> Option<u64> {
            None
        }
    }

    /// 食い違いのある結果を保存してよいかの判断（#164 / #176）。**すでに在るもののほうが
    /// 届いていれば置き換えない**のと、**開いても何も出ない途中結果を作らない**の 2 つを固定する。
    ///
    /// 前者が破れると、音源がもう最後まで読めない状況で前回の結果が失われる（取り返しが
    /// つかない）。後者が破れると、押しても何も現れない `Show partial` が出る。
    ///
    /// **同じところまでしか読めなかったときは置き換える**——ここを「在るかどうか」で弾くと、
    /// Try again 一発で途中結果を伏せる仕組みが外れる（`FileOutcome::NotKept` に落ちて
    /// `kept` が `Nothing` になるため）。
    #[test]
    fn a_transcript_with_a_shortfall_is_only_written_when_it_does_not_lose_ground() {
        let dir = std::env::temp_dir().join(format!("shoki-worth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the temp dir should succeed");
        let json_path = dir.join("mic.json");
        // **端数のある実長を使う**——書き込み経路が作るのは `pcm.len() / 16000` の f64 で、
        // 整数秒になるのはサンプル数が 16000 の倍数のときだけ。整数で試すと、表示用に
        // 丸めた値で比べてしまう間違いを見逃す。
        let read_upto_secs = 252.3125_f64;
        let stored = |json: &str| {
            std::fs::write(&json_path, json)
                .expect("writing the existing transcript should succeed");
        };
        let cut = TranscriptShortfall::StopsPartway;
        let gaps = TranscriptShortfall::HasGaps;

        assert!(
            is_worth_writing(cut, 3, &json_path, read_upto_secs),
            "the first partial transcript of a source is worth keeping"
        );
        assert!(
            !is_worth_writing(cut, 0, &json_path, read_upto_secs),
            "a transcript that stops partway with no lines would leave nothing to open"
        );
        // **抜けているだけのものは別**（#176）。最後までは読めているので、行が無いのは
        // 「話していなかった」でもありうる——ここを弾くと、静かな録音がやり直しのたびに
        // 「could not be transcribed」になる。
        assert!(
            is_worth_writing(gaps, 0, &json_path, read_upto_secs),
            "a gapped transcript still covers the whole recording"
        );

        // 同じところまでしか読めなかったやり直し（Try again）。中身は同じなので置き換える
        // ——ここが false に転ぶと、途中結果を伏せる仕組みが Try again 一発で外れる。
        stored(&format!(
            r#"{{"complete":false,"duration_secs":{read_upto_secs},"segments":[]}}"#
        ));
        assert!(
            is_worth_writing(cut, 3, &json_path, read_upto_secs),
            "re-running on the same audio replaces what it produced last time"
        );

        // **食い違いの無い保存物は、長さが同じでも置き換えない**（#175 / #176）。長さだけで
        // 見ていると、完成品が不可逆に降格する。**抜けているだけの結果でも同じ**——#176 より
        // 前は「終端まで読めた」を無条件に書いていたので、あとから壊れたファイルの結果が
        // 完成品を静かに潰せた。
        stored(&format!(
            r#"{{"complete":true,"duration_secs":{read_upto_secs},"segments":[]}}"#
        ));
        assert!(
            !is_worth_writing(cut, 3, &json_path, read_upto_secs),
            "a transcript with no shortfall is never replaced by one that stops partway"
        );
        assert!(
            !is_worth_writing(gaps, 3, &json_path, read_upto_secs),
            "a transcript with no shortfall is never replaced by a gapped one"
        );

        // 前の実行のほうが先まで読めている（印が無い古い JSON でも）。置き換えない。
        stored(r#"{"duration_secs":3600.0,"segments":[]}"#);
        assert!(
            !is_worth_writing(cut, 3, &json_path, read_upto_secs),
            "what reaches further is never replaced by a shorter partial run"
        );

        // **長さは、どちらも途中で終わっているときだけ比べる**（#176）。読み飛ばしがあると
        // `duration_secs` は抜けたぶん短くなり、音声の位置ではなくなる。
        //
        // 在るほうが最後まで届いている（抜けてはいる）。届いていない結果では潰さない。
        stored(r#"{"complete":true,"gapped":true,"duration_secs":3200.0,"segments":[]}"#);
        assert!(
            !is_worth_writing(cut, 3, &json_path, read_upto_secs),
            "a gapped transcript of the whole recording outranks one that stops partway"
        );
        // 逆向き。在るほうが 55 分で切れていて、新しいほうは最後まで届いた（抜けてはいる）。
        // 長さで比べると「前のほうが長い」に転んで、最後まで届いた結果を捨ててしまう。
        stored(r#"{"complete":false,"duration_secs":3300.0,"segments":[]}"#);
        assert!(
            is_worth_writing(gaps, 3, &json_path, read_upto_secs),
            "reaching the end outranks a longer run that stopped partway"
        );

        // 長さが読めない（壊れている・古い形式）なら、残して困るものが無いので書く。
        std::fs::write(&json_path, b"{ this is not json").expect("writing should succeed");
        assert!(
            is_worth_writing(cut, 3, &json_path, read_upto_secs),
            "a transcript that cannot be read is not worth protecting"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn decode_mp3_keeps_what_it_read_when_it_cannot_read_further() {
        let whole = sine_mp3(48_000, 4);
        let readable = decode_mp3_stream(
            MediaSourceStream::new(
                Box::new(StopsReadingAfter {
                    bytes: std::io::Cursor::new(whole.clone()),
                    // 打ち切らない（最後まで読める側）。
                    limit: u64::MAX,
                }),
                Default::default(),
            ),
            "mic.mp3",
        )
        .expect("the whole stream should decode");
        assert_eq!(
            readable.shortfall, None,
            "a stream that ends cleanly has no shortfall"
        );

        let limit = whole.len() as u64 / 3;
        let cut = decode_mp3_stream(
            MediaSourceStream::new(
                Box::new(StopsReadingAfter {
                    bytes: std::io::Cursor::new(whole),
                    limit,
                }),
                Default::default(),
            ),
            "mic.mp3",
        )
        .expect("the readable part should still decode");

        assert_eq!(
            cut.shortfall,
            Some(TranscriptShortfall::StopsPartway),
            "a stream that cannot be read further stops partway"
        );
        assert!(
            !cut.samples.is_empty(),
            "the part that could be read is kept"
        );
        assert!(
            cut.samples.len() < readable.samples.len(),
            "only the part that could be read is kept: {} of {}",
            cut.samples.len(),
            readable.samples.len()
        );
        // 形式は最初に読めたパケットで決まるので、途中で降りても変わらない。
        assert_eq!(cut.sample_rate, readable.sample_rate);
        assert_eq!(cut.channels, readable.channels);
    }

    #[test]
    fn decode_mp3_fails_on_empty_file() {
        // 壊れた/空の入力ではエラーを返す（黙って空の音声にしない）。
        let dir = std::env::temp_dir();
        let path = dir.join(format!("shoki-empty-{}.mp3", std::process::id()));
        std::fs::write(&path, b"").expect("writing the empty file should succeed");
        let result = decode_mp3(&path);
        let _ = std::fs::remove_file(&path);
        assert!(result.is_err());
    }

    #[test]
    fn write_transcription_creates_json_with_owner_only_permissions() {
        // 文字起こしは録音と同じ機微データ。JSON の内容と 0600（Unix）を whisper なしで検証する
        // （E2E は #[ignore] のため、この安全性は CI ではここで担保する）。
        let dir = std::env::temp_dir();
        let path = dir.join(format!("shoki-transcription-{}.json", std::process::id()));
        let result = Transcription {
            source: "mic".to_owned(),
            model: "ggml-base.bin".to_owned(),
            language: "ja".to_owned(),
            duration_secs: 1.0,
            complete: true,
            gapped: false,
            segments: vec![Segment {
                start: 0.0,
                end: 1.0,
                text: "hi".to_owned(),
            }],
        };
        write_transcription(&path, &result).expect("writing should succeed");

        let text = std::fs::read_to_string(&path).expect("the JSON should be readable");
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["segments"][0]["text"], "hi");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata should be readable")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the JSON must be created with 0600");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// パイプライン全体（MP3 デコード→リサンプル→whisper→JSON 保存）のスモークテスト。
    /// whisper モデルが必要なため通常は実行しない。ローカルでモデルを用意して
    /// `SHOKI_WHISPER_MODEL=<path> cargo test -- --ignored` で実行する。
    /// 入力は合成サイン波（発話なし）なので、認識テキストではなく「JSON が既定の形・0600 で
    /// 生成される」ことだけを確認する。
    #[test]
    #[ignore = "requires a whisper model; set SHOKI_WHISPER_MODEL and run with --ignored"]
    fn end_to_end_writes_transcription_json_for_generated_mp3() {
        let model_path = std::env::var("SHOKI_WHISPER_MODEL")
            .expect("SHOKI_WHISPER_MODEL must point to a ggml whisper model");

        // 2 秒の 440Hz サイン波（48kHz モノラル）を MP3 にエンコードする。
        let mp3 = sine_mp3(48_000, 2);

        let dir =
            std::env::temp_dir().join(format!("shoki-transcribe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creating the temp dir should succeed");
        let audio_path = dir.join("mic.mp3");
        std::fs::write(&audio_path, &mp3).expect("writing the test MP3 should succeed");

        run_job(
            &TranscribeJob {
                session_dir: dir.clone(),
                audio_paths: vec![audio_path.clone()],
                model_id: crate::whisper_model::DEFAULT_MODEL_ID.to_owned(),
                model_override: Some(PathBuf::from(model_path)),
                language: "en".to_owned(),
                summarize: None,
            },
            &crate::model_download::ModelDownloader::new(),
            &crate::inference_slot::InferenceSlot::new(),
            &test_queue(),
            0,
            &Arc::new(AtomicBool::new(false)),
        );

        let json_path = audio_path.with_extension("json");
        let text =
            std::fs::read_to_string(&json_path).expect("the transcription JSON should exist");
        let value: serde_json::Value =
            serde_json::from_str(&text).expect("the output should be valid JSON");
        assert_eq!(value["source"], "mic");
        assert!(value["segments"].is_array());
        assert!(value["duration_secs"].as_f64().unwrap_or(0.0) > 1.5);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&json_path)
                .expect("metadata should be readable")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the JSON must be created with 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 書く側と読む側が、**同じ欄名・同じ極性**で録音との食い違いを受け渡すこと（#175 / #176）。
    ///
    /// `Transcription`（書く）と `TranscriptFile`（読む）は、名前が同じという約束だけで
    /// 繋がっている。片方の欄名を変えても、片方の意味を反転させても、それぞれの単体テストは
    /// 緑のまま——**再起動後に欠けた文字起こしが完成品として読める**という、いちばん
    /// 気づきにくい壊れ方になる。
    ///
    /// **4 通りを全部回す**（#176）。`complete` と `gapped` は極性が逆なので、対角
    /// （両方揃っている／両方欠けている）だけを回すと、2 つを取り違えても素通りする。
    #[test]
    fn what_we_write_about_the_shortfall_is_what_the_reader_sees() {
        let dir = std::env::temp_dir().join(format!("shoki-reach-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the session dir");

        for shortfall in [
            None,
            Some(TranscriptShortfall::HasGaps),
            Some(TranscriptShortfall::StopsPartway),
            Some(TranscriptShortfall::StopsPartwayWithGaps),
        ] {
            // **本番と同じ組み立てを通す**（`transcribe_file` が書くのと同じ写像）。
            let marks = TranscriptShortfall::marks(shortfall);
            let result = Transcription {
                source: "mic".to_owned(),
                model: "ggml-base.bin".to_owned(),
                language: "ja".to_owned(),
                duration_secs: 12.5,
                complete: marks.reached_the_end,
                gapped: marks.gapped,
                segments: vec![Segment {
                    start: 0.0,
                    end: 3.2,
                    text: "hello".to_owned(),
                }],
            };
            // 書いた値そのものから分岐する側（`transcribe_file` が保存の可否を決めるのに使う）も
            // 同じ答えになること。
            assert_eq!(result.shortfall(), shortfall);
            let path = dir.join("mic.json");
            write_transcription(&path, &result).expect("write the transcript");

            let reach = crate::transcript::stored_reach(&path).expect("the reader can read it");
            assert_eq!(
                reach.shortfall, shortfall,
                "the reader must see the same shortfall we wrote"
            );
            assert_eq!(reach.duration_secs, Some(12.5));
            // 在る音源ぶんの判定（画面が伏せるかを決める側）まで同じ答えになること。
            let loaded = crate::transcript::load_transcript(
                &dir,
                &[crate::transcript::Speaker::Mic],
                crate::dataless::Fetch::allowed(),
            );
            assert_eq!(loaded.shortfall, shortfall);
            assert_eq!(loaded.segments.len(), 1);
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn transcription_json_shape_matches_viewer_contract() {
        // 録音一覧ビュー（src/transcript.rs）が読む契約: segments[].start/end/text（秒）。
        let result = Transcription {
            source: "mic".to_owned(),
            model: "ggml-base.bin".to_owned(),
            language: "auto".to_owned(),
            duration_secs: 3.21,
            complete: true,
            gapped: false,
            segments: vec![Segment {
                start: 0.0,
                end: 3.2,
                text: "hello".to_owned(),
            }],
        };
        let json = serde_json::to_string(&result).expect("serialization should succeed");
        let value: serde_json::Value =
            serde_json::from_str(&json).expect("round trip should succeed");
        assert_eq!(value["source"], "mic");
        assert_eq!(value["segments"][0]["start"], 0.0);
        assert_eq!(value["segments"][0]["end"], 3.2);
        assert_eq!(value["segments"][0]["text"], "hello");
        // 最後まで読めたかは JSON に残る（#175。読む側が再起動後も途中結果を見分ける）。
        assert_eq!(value["complete"], true);
    }
}
