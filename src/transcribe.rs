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
//! 他音源・アプリ・録音を巻き込まない（`docs/rules/error-handling.md`）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

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
    /// 全音源の文字起こしが完了した。
    Done,
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
            Self::Done => TranscribeStatus::Done,
            Self::Failed { .. } => TranscribeStatus::Failed,
        }
    }
}

/// 文字起こしが失敗した理由（#159）。
///
/// **文言はここに持たない**。ワーカー層が UI のコピーを持つと、状態→文言の対応表が
/// `main::TranscriptPane::message` と 2 箇所に割れる（`docs/rules/messages.md` の管轄）。
/// 種別を足せば向こうの網羅 match が割れて、書き忘れに気づける。
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

/// セッション単位の文字起こしの進行状況。Recordings ウィンドウの状態表示に使う。
/// マップに載らないセッションの表示は「JSON の有無」で解決する（`docs/plans/done/` の #69 プラン）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscribeStatus {
    /// 投入済み（キュー待ちを含む）または処理中。
    Transcribing,
    /// 全音源の文字起こしが完了した。
    Done,
    /// 少なくとも 1 音源が失敗した（理由はログ。メモリのみで、再起動後は JSON の有無に基づく
    /// 表示へ戻る。再実行でクリアされる）。
    Failed,
}

/// 文字起こしのバックグラウンドワーカー。`submit` されたジョブを 1 本のスレッドで逐次処理する
/// （whisper は CPU 集約のため、録音が連続してもスレッドを増やさない）。
/// `Clone` で共有できる（後処理ワーカーからの自動投入と、Recordings ウィンドウからの
/// 手動再実行・状態表示が同じワーカー・同じ状態マップを使う）。
#[derive(Clone)]
pub struct TranscribeWorker {
    /// ワーカースレッドへの送信口。スレッド起動に失敗していたら `None`（文字起こしのみ縮退）。
    tx: Option<Sender<TranscribeJob>>,
    /// セッションディレクトリ → 進行状況。`submit` とワーカーのデキュー時に `Transcribing`、
    /// ジョブ完了で `Done` / `Failed` に遷移する（対象なしの Skipped はエントリ削除で
    /// JSON の有無ベースの表示へ戻す）。
    status: Arc<Mutex<StatusMap>>,
}

/// セッションディレクトリ → 進行状況のマップ（UI スレッドとワーカースレッドで共有）。
type StatusMap = HashMap<PathBuf, TranscribeState>;

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
    status: Arc<Mutex<StatusMap>>,
    session_dir: PathBuf,
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
    /// **ここでパニックしない**こと。呼び出し元は whisper.cpp の C フレームで、whisper-rs の
    /// トランポリンは `catch_unwind` を挟まない——巻き戻しが FFI 境界を越えると未定義動作に
    /// なる。いま安全なのは、ロックが poison を吸収し、`total == 0` を先に弾き、`as` が
    /// 飽和キャストだから。`expect` や添字アクセスを足さないこと（`docs/rules/ffi.md`）。
    fn report(&self, file_percent: i32) {
        if self.total == 0 {
            return;
        }
        let within_file = f64::from(file_percent.clamp(0, 100)) / 100.0;
        let overall = ((self.index as f64 + within_file) / self.total as f64 * 100.0).round();
        let overall = overall.clamp(0.0, 100.0) as u8;
        let mut map = lock_status(&self.status);
        // **型が「進行中のときだけ」を保証する**。完了・失敗のあとに遅れて届いた進捗は、
        // 書き込む先そのものが無い（以前はここが実行時のガードだった。#159）。
        if let Some(TranscribeState::Transcribing { percent, .. }) = map.get_mut(&self.session_dir)
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
        TranscribeStatus::Transcribing => true,
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
        let status: Arc<Mutex<StatusMap>> = Arc::new(Mutex::new(HashMap::new()));
        let status_for_worker = Arc::clone(&status);
        let (tx, rx) = mpsc::channel::<TranscribeJob>();
        let spawned = std::thread::Builder::new()
            .name("transcribe-worker".into())
            .spawn(move || {
                // 送信側（アプリ本体）が落ちてチャネルが閉じたら自然に終了する。
                while let Ok(mut job) = rx.recv() {
                    let model_label = job_model_label(&job);
                    // 処理開始でも「文字起こし中」を入れ直す。同一セッションが複数キューされて
                    // いる場合、先行ジョブの完了（Done/Failed）が後続の処理中表示を上書きした
                    // ままにならないようにする（単一ワーカーの逐次処理なので、先行完了→後続
                    // デキューの隙間はごく短い）。
                    lock_status(&status_for_worker).insert(
                        job.session_dir.clone(),
                        TranscribeState::starting(model_label.clone()),
                    );
                    // 文字起こし中のパニックでワーカースレッドを殺さない。死ぬと状態が
                    // `Transcribing` のまま残り、そのセッションは再起動まで Transcribe /
                    // Summarize / Delete がすべて無効になる（Recordings ウィンドウの
                    // `detail-files-in-use` / `detail-jobs-pending`）。失敗として記録し、
                    // 次のジョブは受け続ける（`SummarizeWorker` と同じ扱い）。
                    let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                        || run_job(&job, &downloader, &slot, &status_for_worker),
                    )) {
                        Ok(outcome) => outcome,
                        Err(_) => {
                            eprintln!(
                                "Skipping transcription because transcribing the session panicked"
                            );
                            JobOutcome::Failed(TranscribeFailure::Panicked)
                        }
                    };
                    // 要約は「全音源の文字起こしに成功した」ときだけ続ける。部分的に失敗した
                    // 文字起こしから議事録を作ると、欠けたまま完成品に見えてしまう。
                    let summarize = match outcome {
                        JobOutcome::Done => job.summarize.take(),
                        JobOutcome::Failed(_) | JobOutcome::Skipped => None,
                    };
                    {
                        let mut map = lock_status(&status_for_worker);
                        match outcome {
                            // 対象なしで何もしなかった場合は「投入済み」の痕跡を消し、
                            // 表示を JSON の有無ベース（前/完了）へ戻す。
                            JobOutcome::Skipped => {
                                map.remove(&job.session_dir);
                            }
                            JobOutcome::Done => {
                                map.insert(job.session_dir, TranscribeState::Done);
                            }
                            JobOutcome::Failed(reason) => {
                                map.insert(job.session_dir, TranscribeState::Failed { reason });
                            }
                        }
                    }
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
                status,
            },
            Err(err) => {
                eprintln!(
                    "Disabling transcription because the worker thread failed to start: {err}"
                );
                Self { tx: None, status }
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
        lock_status(&self.status).insert(
            job.session_dir.clone(),
            TranscribeState::starting(job_model_label(&job)),
        );
        // 送信失敗 = ワーカースレッドが（panic 等で）終了しレシーバが閉じた状態。
        // 記録した「文字起こし中」を取り消す（永遠に進行中表示のままにしない）。
        // ジョブは SendError から回収してキーの事前 clone を避ける。
        if let Err(mpsc::SendError(job)) = tx.send(job) {
            eprintln!("Skipping transcription because the transcription worker is not running");
            lock_status(&self.status).remove(&job.session_dir);
        }
    }

    /// セッションの進行状況。マップに載っていなければ `None`（表示側が JSON の有無で
    /// 「文字起こし前/完了」を解決する）。
    pub fn status_of(&self, session_dir: &Path) -> Option<TranscribeStatus> {
        // **`state_of` へ委譲しない**。これは一覧の全行が毎 tick 呼ぶ経路で、状態 1 つを読むのに
        // `model_label` と `reason` の確保を払うことになる。同じ 1 エントリを読むので、
        // 委譲しなくても状態と説明は食い違わない。
        lock_status(&self.status)
            .get(session_dir)
            .map(TranscribeState::status)
    }

    /// 一覧の行が要る分だけ（状態と進捗）を、**確保なしで**取る。
    ///
    /// `state_of` はモデル名まで clone するので、全行を毎 tick 回すこの経路には重い
    /// （`status_of` を `state_of` へ委譲しないのと同じ理由）。
    pub fn progress_of(&self, session_dir: &Path) -> Option<(TranscribeStatus, Option<u8>)> {
        lock_status(&self.status)
            .get(session_dir)
            .map(|state| match state {
                TranscribeState::Transcribing { percent, .. } => {
                    (TranscribeStatus::Transcribing, *percent)
                }
                TranscribeState::Done => (TranscribeStatus::Done, None),
                TranscribeState::Failed { .. } => (TranscribeStatus::Failed, None),
            })
    }

    /// セッションの進行状況と、読む領域に出す中身（モデル名・進捗・失敗の理由）。
    ///
    /// **`status_of` はこれの一部を取り出したもの**なので、状態と説明が食い違わない
    /// （2 つのマップに分けると、片方だけ更新した瞬間にありえない組み合わせができる）。
    pub fn state_of(&self, session_dir: &Path) -> Option<TranscribeState> {
        lock_status(&self.status).get(session_dir).cloned()
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
        lock_status(&self.status)
            .values()
            .any(|state| counts_as_pending(state.status()))
    }

    /// セッションの進行状況の記録を破棄する（セッション削除時の掃除）。未登録なら何もしない。
    pub fn forget(&self, session_dir: &Path) {
        lock_status(&self.status).remove(session_dir);
    }
}

/// 状態マップのガードを取る。poison（ロック保持中のパニック）でも状態表示を止めないため、
/// ガードを取り出して続行する（`docs/rules/error-handling.md`）。
fn lock_status(status: &Mutex<StatusMap>) -> MutexGuard<'_, StatusMap> {
    status
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 1 ジョブの処理結果（状態マップへの反映用）。
enum JobOutcome {
    /// 全音源の文字起こしに成功した。
    Done,
    /// 少なくとも 1 音源が失敗した（モデル準備の失敗を含む）。
    Failed(TranscribeFailure),
    /// 対象なしで何もしなかった。
    Skipped,
}

/// 1 ジョブ（1 回の録音停止分）を処理する。モデルはジョブ内で 1 回だけロードして
/// 複数音源で使い回す（モデルのロードが重いため）。音源単位の失敗は他の音源へ波及させない。
fn run_job(
    job: &TranscribeJob,
    downloader: &crate::model_download::ModelDownloader,
    slot: &crate::inference_slot::InferenceSlot,
    status: &Arc<Mutex<StatusMap>>,
) -> JobOutcome {
    if job.audio_paths.is_empty() {
        // 対象なしでモデル（数百 MB〜）をロードしない防御。通常は投入側が空を渡さない。
        return JobOutcome::Skipped;
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
    // 失敗した音源の名前を集める（文にするのは読む領域の仕事。`TranscribeFailure`）。
    let mut failed_names: Vec<String> = Vec::new();
    let total = job.audio_paths.len();
    for (index, path) in job.audio_paths.iter().enumerate() {
        let name = audio_display_name(path);
        let progress = ProgressSink {
            status: Arc::clone(status),
            session_dir: job.session_dir.clone(),
            index,
            total,
        };
        match transcribe_file(&ctx, path, &model_path, job, progress) {
            Ok(segments) => println!("Transcribed {name} ({segments} segments)"),
            Err(err) => {
                eprintln!("Skipping transcription of {name} because it failed: {err}");
                failed_names.push(name);
            }
        }
    }
    if failed_names.is_empty() {
        JobOutcome::Done
    } else {
        JobOutcome::Failed(TranscribeFailure::Files(failed_names))
    }
}

/// 音源を指す**ファイル名だけ**を取り出す。ログにも読む領域にも出るので、ディレクトリ成分を
/// 混ぜない（`docs/rules/security.md`）。**この保証を作っているのはここだけ**なので、テストで
/// 固定してある。
fn audio_display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_owned())
}

/// 1 音源を文字起こしして `<音源名>.json` に保存する。成功時はセグメント数を返す。
/// `model_path` は `run_job` で解決済みのモデル（JSON の `model` フィールド用）。
fn transcribe_file(
    ctx: &WhisperContext,
    audio_path: &Path,
    model_path: &Path,
    job: &TranscribeJob,
    progress: ProgressSink,
) -> Result<usize, Box<dyn std::error::Error>> {
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

    let mut state = ctx.create_state()?;
    state.full(params, &pcm)?;

    let segments = collect_segments(&state);
    let result = Transcription {
        source,
        model: model_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        language: job.language.clone(),
        duration_secs,
        segments,
    };
    let json_path = audio_path.with_extension("json");
    write_transcription(&json_path, &result)?;
    Ok(result.segments.len())
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
    /// 音声全体の長さ（秒）。
    duration_secs: f64,
    /// 発話セグメント（時刻順）。
    segments: Vec<Segment>,
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
}

/// MP3 をデコードしてインターリーブ f32 PCM を得る。
///
/// 対象は自アプリが保存した録音ファイルだが、保存後にユーザーが差し替え・破損させる可能性は
/// あるため、途中のパケットのデコード失敗はスキップして読める部分だけを使う（symphonia の
/// 推奨に従う）。1 サンプルも得られなければエラー。
fn decode_mp3(path: &Path) -> Result<DecodedAudio, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let stream = MediaSourceStream::new(Box::new(file), Default::default());
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
    loop {
        let packet = match format.next_packet() {
            Ok(Some(packet)) => packet,
            Ok(None) => break, // ストリーム終端。
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
                    // フレーム境界がずれて壊れた音声になるため、正直に失敗させる。
                    return Err("audio parameters changed mid-stream".into());
                }
                // 中間バッファを介さず samples の末尾へ直接書き、全量の二重コピーを避ける。
                let base = samples.len();
                samples.resize(base + buffer.samples_interleaved(), 0.0);
                buffer.copy_to_slice_interleaved(&mut samples[base..]);
            }
            // 壊れたパケットはスキップして続行（symphonia の推奨ハンドリング）。
            Err(SymphoniaError::DecodeError(_)) | Err(SymphoniaError::IoError(_)) => continue,
            Err(err) => return Err(err.into()),
        }
    }
    if samples.is_empty() || channels == 0 || sample_rate == 0 {
        return Err("no audio samples could be decoded".into());
    }
    Ok(DecodedAudio {
        samples,
        sample_rate,
        channels,
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
            TranscribeStatus::Done => TranscribeState::Done,
            TranscribeStatus::Failed => TranscribeState::Failed {
                reason: TranscribeFailure::ModelMissing,
            },
        }
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

        lock_status(&worker.status).insert(dir.clone(), TranscribeState::starting("Small".into()));
        assert!(
            worker.has_pending_jobs(),
            "Transcribing counts as a pending job"
        );
        // 終わったジョブは数えない（消してよい）。
        for status in [TranscribeStatus::Done, TranscribeStatus::Failed] {
            lock_status(&worker.status).insert(dir.clone(), test_state(status));
            assert!(
                !worker.has_pending_jobs(),
                "{status:?} must not count as a pending job"
            );
        }
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
        let status: Arc<Mutex<StatusMap>> = Arc::new(Mutex::new(StatusMap::new()));
        let dir = PathBuf::from("/tmp/shoki-progress");
        lock_status(&status).insert(dir.clone(), TranscribeState::starting("Small".into()));
        let percent_now = || match lock_status(&status).get(&dir) {
            Some(TranscribeState::Transcribing { percent, .. }) => *percent,
            other => panic!("the entry should still be transcribing, got {other:?}"),
        };

        let first = ProgressSink {
            status: Arc::clone(&status),
            session_dir: dir.clone(),
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
        let status: Arc<Mutex<StatusMap>> = Arc::new(Mutex::new(StatusMap::new()));
        let dir = PathBuf::from("/tmp/shoki-progress-late");
        lock_status(&status).insert(dir.clone(), test_state(TranscribeStatus::Failed));
        ProgressSink {
            status: Arc::clone(&status),
            session_dir: dir.clone(),
            index: 0,
            total: 1,
        }
        .report(80);
        let state = lock_status(&status)
            .get(&dir)
            .cloned()
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
        let sample_rate = 48_000u32;
        let pcm: Vec<i16> = (0..(sample_rate * 2) as usize)
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
            &Arc::new(Mutex::new(StatusMap::new())),
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

    #[test]
    fn transcription_json_shape_matches_viewer_contract() {
        // 録音一覧ビュー（src/transcript.rs）が読む契約: segments[].start/end/text（秒）。
        let result = Transcription {
            source: "mic".to_owned(),
            model: "ggml-base.bin".to_owned(),
            language: "auto".to_owned(),
            duration_secs: 3.21,
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
    }
}
