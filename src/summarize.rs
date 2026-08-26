//! 文字起こしから議事録要約（`summary.md`）を生成して保存する。
//!
//! 入力は `src/transcript.rs` がマージした話者ラベル付きトランスクリプト、出力はセッション
//! ディレクトリ直下の `summary.md`（0600）。生成は `TranscribeWorker` と同型の逐次ワーカー
//! （`SummarizeWorker`）がバックグラウンドで行い、設定 ON かつ文字起こし成功時に自動投入される
//! （投入元は `src/transcribe.rs`）。
//!
//! **実行エンジンは enum ディスパッチ**（`SummaryEngine`）にしてある。いまは
//! オンデバイス LLM（`on_device`。llama.cpp）だけだが、後続でオンライン LLM
//! （Claude / OpenAI / Gemini）が追加される予定なので、ジョブ投入・状態管理・`summary.md` の
//! 保存はエンジン非依存のこのモジュールに置き、エンジン固有の実装だけをサブモジュールへ分ける
//! （`docs/plans/done/20260722-online-llm-engines.md`）。
//!
//! プロンプト・モデル・チャンク閾値は #78 の検証で確定したもの
//! （`docs/plans/done/20260722-meeting-minutes-summary.md`）。純粋部分（トランスクリプトの
//! 整形・チャンク分割・見出し言語の選択・プロンプト組み立て）はモデル無しで単体テストできる
//! ように関数へ切り出してある。

mod on_device;

use crate::dataless::ReadFailure;
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

/// 失敗の種別は、文言表（網羅 match）の隣に置くために `reading_pane` が持っている。
/// ただし値を作るのはこのモジュールなので、読む人が探す場所はここでもある——同じ名前で
/// 引けるように再エクスポートしておく。
pub use crate::reading_pane::SummarizeFailure;
use crate::transcript::TranscriptSegment;

/// 生成した議事録の保存ファイル名。セッションディレクトリに固定名で置く
/// （`mic.json` / `mix.mp3` と同系統）。生成（`run_job`）・表示（`load_summary`）・
/// 一覧の有無判定（`crate::recordings`）がこの 1 つの名前を共有する。
pub const SUMMARY_FILENAME: &str = "summary.md";

/// 読み込む `summary.md` のサイズ上限。保存先の生成物は手で置換されうる信頼境界外の入力なので、
/// 想定外の巨大ファイルでメモリを大量確保しない保険（`docs/rules/security.md`。
/// `transcript.rs` の `MAX_TRANSCRIPT_BYTES` と同じ趣旨）。
///
/// 実際の議事録は長い会議でも数十 KB なので、桁の余裕は 1 つに留める。**バイト数の上限は
/// そこから作る表示行の数・長さの上限も兼ねる**（`main::summary_rows` は 1 行ごとに文字列を
/// 確保するので、改行だけの巨大ファイルを許すと行モデルの構築で UI が固まる）。
const MAX_SUMMARY_BYTES: u64 = 256 * 1024;

/// 1 チャンクに入れるトランスクリプト本文のトークン概算の上限。
///
/// コンテキスト長（Qwen2.5 は 32,768）ではなく **prefill が超線形に伸びること**が理由の閾値
/// （#78 の確定プロンプトでの実測: 3B でトークン 4.29 倍に対し時間 6.97 倍）。約 4,000 トークンは本文で日本語 5,800 文字
/// （約 19 分）・英語 12,800 文字（約 25 分）に相当する。
const CHUNK_TOKEN_BUDGET: usize = 4_000;

/// 中間メモを畳み直す最大回数。**畳み直しの収束に関する説明の正はここ**（`on_device` 側は
/// ここを参照する）。
///
/// 1 本のメモは生成上限（`on_device::MAX_NOTES_TOKENS` = 800）以下、詰め先は
/// `CHUNK_TOKEN_BUDGET`（4,000）なので、1 ラウンドで件数はおよそ 1/5 になる。2 時間の会議
/// （6〜7 チャンク）なら 1 ラウンドで収まる。上限があるのは、想定外の入力で無限に回さない
/// ための保険（超えたぶんは末尾を切り詰める）。
const MAX_REDUCE_ROUNDS: usize = 4;

/// 要約の実行エンジン。いまはオンデバイスのみで、オンライン LLM（Claude / OpenAI / Gemini）は
/// 後続でバリアントとして足す（設定・API キー基盤も同じ後続が用意する）。
///
/// **オンラインのエンジンを足すときは、設定画面の注記も直すこと**（`ui/app-window.slint` の
/// 「Audio and transcripts never leave this Mac.」）。いまはオンデバイスだけなのでこの断言が
/// 成り立っているが、送信するエンジンが増えた瞬間に**そのままだと嘘になる**。Slint の文字列
/// リテラルなので、足してもコンパイルは通ってしまう（#83〜#85）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryEngine {
    /// ローカルの llama.cpp で生成する（外部送信なし）。
    OnDevice,
}

/// 要約ジョブ。1 セッション分の入力と、投入時点の設定スナップショット
/// （処理中に設定が変わっても影響しない。`TranscribeJob` と同じ方針）。
#[derive(Debug)]
pub struct SummarizeJob {
    /// 対象の録音セッションディレクトリ。文字起こし JSON の読み元・`summary.md` の書き先・
    /// 状態表示（`SummarizeStatus`）のキーを兼ねる。
    pub session_dir: PathBuf,
    /// 使用するエンジン。
    pub engine: SummaryEngine,
    /// 使用する内蔵 LLM の識別子（設定 `summary_model`）。カタログ外は既定へフォールバック。
    pub model_id: String,
    /// LLM モデルの上書きパス（設定 `summary_model_path`）。`None` なら内蔵モデル
    /// （未取得なら処理時に自動ダウンロードされる。`src/model_download.rs`）。
    pub model_override: Option<PathBuf>,
    /// 出力言語の決定に使う認識言語コード（設定 `transcribe_language`。`auto` を含む）。
    pub language: String,
    /// 既にある `summary.md` を「古い」と見なすか（生成に失敗したときに消すかの判断。`failed`）。
    ///
    /// 文字起こし直後の自動生成は `true`: 既存の議事録は**前の文字起こし**のものなので、生成に
    /// 失敗したときに残しておくと新しい文字起こしの議事録として読まれてしまう。
    /// Library ウィンドウからの手動生成は `false`: 既存の議事録は現在の文字起こしと整合した
    /// 有効なデータなので、失敗しても失わせない（成功時は上書きされる）。
    pub existing_is_stale: bool,
}

/// セッション単位の要約の進行状況（`TranscribeWorker` と同型）。Library ウィンドウの
/// 詳細ペインが `main::summary_display_status` で表示状態へ合成して出す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummarizeStatus {
    /// 投入済みで、ワーカーが取り出すのを待っている（まだ CPU を使っていない）。
    /// この間だけ `cancel` で取り消せる。
    Queued,
    /// 生成中（ワーカーが取り出して走らせている）。
    Summarizing,
    /// `summary.md` を保存した。
    Done,
    /// 生成に失敗した（理由はログ。メモリのみで、再起動後は消える）。
    Failed,
}

/// 要約のバックグラウンドワーカー。`submit` されたジョブを 1 本のスレッドで逐次処理する
/// （LLM は CPU・メモリを大きく食うため、録音が連続してもスレッドを増やさない）。
/// `Clone` で共有できる（自動投入と、後続の手動再生成・状態表示が同じ状態マップを使う）。
#[derive(Clone)]
pub struct SummarizeWorker {
    /// ワーカースレッドへの送信口。スレッド起動に失敗していたら `None`（要約のみ縮退）。
    tx: Option<Sender<QueuedJob>>,
    /// キューの状態（進行状況と採番）。ジョブを走らせてよいかの唯一の判断材料（`QueueState`）。
    queue: Arc<Mutex<QueueState>>,
}

/// キューへ流すジョブ（投入通番つき）。通番は `submit` が採番する内部の識別子で、
/// 呼び出し側は組み立てない（`SummarizeJob` に持たせるとジョブの内容と混ざる）。
struct QueuedJob {
    seq: u64,
    job: SummarizeJob,
}

/// キューの状態（UI スレッドとワーカースレッドで 1 つのミューテックスに入れて共有する）。
///
/// 通番はジョブの識別子で、**`status` が「いま有効なジョブはどれか」の単一のソース**になる:
/// ワーカーは取り出したジョブの通番がここに載っているときだけ走らせ、完了の書き戻しも
/// 同じ照合で守る。これにより (1) 取り消したジョブ、(2) 同じセッションで後から積み直されて
/// 追い越されたジョブ、(3) 先行ジョブの完了による後続の表示の上書き、がまとめて落ちる。
///
/// 採番を同じミューテックスに入れているのは、**ロックの外で採番するとこの仕組みが壊れる**
/// ため（2 つのスレッドが同時に積むと、古い通番が後からマップに載って新しいジョブが落ちる）。
/// 別々のカウンタにするとその制約がコメントでしか守られないので、型で 1 つにしている。
struct QueueState {
    /// セッションディレクトリ → そのセッションで**最後に投入したジョブ**の通番と進行状況。
    status: HashMap<PathBuf, (u64, SummarizeEntry)>,
    /// 次に配る投入通番。**セッションではなくジョブを識別する**ので、同じセッションを
    /// 積み直しても別の値になり、取り消しが他のジョブを巻き添えにしない。
    next_seq: u64,
}

/// キューに載っている 1 ジョブの状態と、**読む領域に出す中身**（#154）。
///
/// 状態と説明を 1 つの値にまとめる理由は `crate::transcribe::TranscribeState` と同じ
/// （別々に持つと、片方だけ更新した瞬間にありえない組み合わせができる）。
/// **状態ごとに、その状態でだけ意味のあるものを持つ**（#159。`TranscribeState` と同じ理由）。
/// 以前は `status` と `Option` を並べていたので、`demote_superseded` が開始時刻を手で消す必要が
/// あった——いまは変種を差し替えるだけで、消し忘れが起こりえない。
#[derive(Debug, Clone)]
enum SummarizeEntry {
    /// 投入済みで、ワーカーが取り出すのを待っている。**モデル名は持たない**——ワーカーが
    /// 取り出すまで何で走るかは決まらない（積み直しで追い越されることがある）。
    Queued,
    /// 生成中。`started` は経過を出すのに使う。
    Summarizing {
        model_label: String,
        started: Instant,
    },
    /// `summary.md` を保存した。
    Done,
    /// 生成に失敗した。
    Failed { reason: SummarizeFailure },
}

impl SummarizeEntry {
    /// 削除ガードや表示が読む、粗い進行状況。
    fn status(&self) -> SummarizeStatus {
        match self {
            Self::Queued => SummarizeStatus::Queued,
            Self::Summarizing { .. } => SummarizeStatus::Summarizing,
            Self::Done => SummarizeStatus::Done,
            Self::Failed { .. } => SummarizeStatus::Failed,
        }
    }
}

/// 読む領域が読む、セッション 1 件分の要約の状態。`SummarizeEntry` から組み立てる
/// （順番・経過はマップ全体を見ないと出せないので、読み出しのたびに計算する）。
#[derive(Debug, Clone)]
pub enum SummarizeState {
    Queued {
        /// キュー待ちの中で何番目か（1 始まり）。**保存せず読み出し時に数える**——前のジョブが
        /// 終われば順番は繰り上がるので、投入時に固定すると嘘になる。
        position: usize,
    },
    /// **モデル名を持つのはここだけ**。読む領域が「何が動いているか」を言うのは走っている間で、
    /// 待っている間・終わったあとに出しても読み手の役に立たない（要るようになったら足す）。
    Summarizing {
        model_label: String,
        /// 始めてからの経過。
        elapsed: std::time::Duration,
    },
    Done,
    Failed {
        reason: SummarizeFailure,
    },
}

impl QueueState {
    /// この通番のジョブが、そのセッションの「いま有効なジョブ」か。
    fn is_current(&self, session_dir: &Path, seq: u64) -> bool {
        self.status.get(session_dir).map(|(latest, _)| *latest) == Some(seq)
    }

    /// この通番のキュー待ちジョブが、キュー待ちの中で何番目か（1 始まり）。
    ///
    /// **走り出しているジョブは数えない**。数えると「1 番目なのに始まらない」になる
    /// （先頭は既に生成中で、待っている側から見た順番ではない）。
    fn queued_position(&self, seq: u64) -> usize {
        self.status
            .values()
            .filter(|(other, entry)| matches!(entry, SummarizeEntry::Queued) && *other < seq)
            .count()
            + 1
    }

    /// 通番を 1 つ配る（ロックの中でしか呼べないので、採番順とマップの登録順がずれない）。
    fn next_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }
}

/// この状態を「まだ終わっていないジョブ」として数えるか（`SummarizeWorker::has_pending_jobs`）。
///
/// **網羅 match**にしてあるので、状態を足したら扱いを書くまでコンパイルが通らない
/// （`_ => false` にしておくと、状態を足した日にモデルの削除ガードが静かに外れる）。
fn counts_as_pending(status: SummarizeStatus) -> bool {
    match status {
        SummarizeStatus::Queued | SummarizeStatus::Summarizing => true,
        SummarizeStatus::Done | SummarizeStatus::Failed => false,
    }
}

/// `cancel_queued` の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelOutcome {
    /// キュー待ちだったジョブを取り消した。
    Cancelled,
    /// 投入されていない（または既に終わっている）。
    NotQueued,
    /// 生成中で取り消せない。ワーカーがこのセッションのファイルを読み書きしている。
    Running,
}

impl SummarizeWorker {
    /// ワーカースレッドを起動する。スレッド生成に失敗しても常駐アプリは落とさず、
    /// 要約だけを無効化してログを残す。
    ///
    /// スレッドは意図的に join しない（detach）: 生成は数分かかりうるため、終了時に join すると
    /// アプリの終了がブロックされる。常駐終了時に処理中のジョブは中断される（ベストエフォート。
    /// 次回に手動で再生成できる）。
    /// `slot` は文字起こしワーカーと共有する重い推論の実行権（`crate::inference_slot`）。
    pub fn start(
        downloader: crate::model_download::ModelDownloader,
        slot: crate::inference_slot::InferenceSlot,
    ) -> Self {
        let queue = Arc::new(Mutex::new(QueueState {
            status: HashMap::new(),
            next_seq: 0,
        }));
        let queue_for_worker = Arc::clone(&queue);
        let (tx, rx) = mpsc::channel::<QueuedJob>();
        let spawned = std::thread::Builder::new()
            .name("summarize-worker".into())
            .spawn(move || {
                // 送信側（アプリ本体）が落ちてチャネルが閉じたら自然に終了する。
                while let Ok(QueuedJob { seq, job }) = rx.recv() {
                    let model_label = job_model_label(&job);
                    // **走らせてよいかの判定と「生成中」への遷移は 1 つのクリティカル
                    // セクションで行う**（別々にすると、その隙間に入った `cancel` が
                    // 「取り消せた」と答えたあとでジョブが走り出す）。マップに自分の通番が
                    // 載っていなければ、取り消されたか後続に追い越されたジョブ（`StatusMap`）。
                    let claimed = {
                        let mut queue = lock_queue(&queue_for_worker);
                        if queue.is_current(&job.session_dir, seq) {
                            queue.status.insert(
                                job.session_dir.clone(),
                                (
                                    seq,
                                    SummarizeEntry::Summarizing {
                                        model_label: model_label.clone(),
                                        started: Instant::now(),
                                    },
                                ),
                            );
                            true
                        } else {
                            false
                        }
                    };
                    if !claimed {
                        println!("Skipping summarization because the job is no longer current");
                        continue;
                    }
                    // 生成中のパニックでワーカースレッドを殺さない。死ぬと状態が
                    // `Summarizing` のまま残り、そのセッションは再起動まで Transcribe /
                    // Summarize / Delete がすべて無効になる（UI の `detail-files-in-use` /
                    // `detail-jobs-pending`）。
                    // 失敗として記録し、次のジョブは受け続ける。
                    let outcome =
                        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            run_job(&job, &downloader, &slot)
                        })) {
                            Ok(outcome) => outcome,
                            Err(_) => {
                                eprintln!(
                                    "Skipping summarization because generating the summary panicked"
                                );
                                // 古い議事録の後始末は `run_job` の失敗経路と同じにする
                                // （パニックでも「古い議事録を残さない」を守る）。
                                failed(
                                    &job,
                                    &job.session_dir.join(SUMMARY_FILENAME),
                                    SummarizeFailure::Panicked,
                                )
                            }
                        };
                    let mut queue = lock_queue(&queue_for_worker);
                    // 自分より後に積まれたジョブが載っているなら、その表示を古い結果で
                    // 上書きしない。
                    if !queue.is_current(&job.session_dir, seq) {
                        demote_superseded(&mut queue, &job.session_dir);
                        continue;
                    }
                    match outcome {
                        // 対象なしで何もしなかった場合は「投入済み」の痕跡を消す。
                        JobOutcome::Skipped => {
                            queue.status.remove(&job.session_dir);
                        }
                        JobOutcome::Done => {
                            queue
                                .status
                                .insert(job.session_dir, (seq, SummarizeEntry::Done));
                        }
                        JobOutcome::Failed(reason) => {
                            queue
                                .status
                                .insert(job.session_dir, (seq, SummarizeEntry::Failed { reason }));
                        }
                    }
                }
            });
        // 差分はワーカーが立ったか（= 送信口を持つか）だけ。`Self { .. }` を 2 回書くと、
        // フィールドを足したときに片方だけ直す事故になる。
        let tx = match spawned {
            Ok(_handle) => Some(tx),
            Err(err) => {
                eprintln!(
                    "Disabling summarization because the worker thread failed to start: {err}"
                );
                None
            }
        };
        Self { tx, queue }
    }

    /// ジョブを投入する。投入した時点でセッションを「キュー待ち」として記録する
    /// （ワーカーが取り出すと「生成中」へ進む）。ワーカーが動いていない場合はログのみ
    /// （文字起こしまでは保存済み）。
    ///
    /// 同じセッションを積み直すと**新しいジョブが古いジョブを追い越す**（古い方はワーカーが
    /// 取り出したときに落ちる。`StatusMap`）。既に生成中のセッションへ積んだ場合は表示を
    /// 「キュー待ち」へ下げない: ワーカーはまだそのセッションのファイルを読み書きしており、
    /// 下げると Delete と Cancel を開けてしまう。
    pub fn submit(&self, job: SummarizeJob) {
        let Some(tx) = &self.tx else {
            eprintln!("Skipping summarization because the summary worker is not running");
            return;
        };
        // 採番とマップへの登録は 1 つのクリティカルセクションで行う（理由は `QueueState`）。
        let seq = {
            let mut queue = lock_queue(&self.queue);
            let seq = queue.next_seq();
            // 生成中のセッションへ積み直したときは、走っているジョブの表示を**まるごと**
            // 引き継ぐ（下げない理由は上の doc）。フィールドを選んで写すと、モデル名だけ新しい
            // ジョブのものになって「動いていないモデルが、動いているジョブの経過つきで」出る。
            let running = match queue.status.get(&job.session_dir) {
                Some((_, entry @ SummarizeEntry::Summarizing { .. })) => Some(entry.clone()),
                _ => None,
            };
            let shown = running.unwrap_or(SummarizeEntry::Queued);
            queue.status.insert(job.session_dir.clone(), (seq, shown));
            seq
        };
        // 送信失敗 = ワーカースレッドが（panic 等で）終了しレシーバが閉じた状態。記録した
        // 「キュー待ち」を取り消す（永遠に進行中表示のままにしない）。自分の通番が載っている
        // ときだけ消す（後から積まれたジョブの記録を消さない）。
        if let Err(mpsc::SendError(QueuedJob { job, .. })) = tx.send(QueuedJob { seq, job }) {
            eprintln!("Skipping summarization because the summary worker is not running");
            let mut queue = lock_queue(&self.queue);
            if queue.is_current(&job.session_dir, seq) {
                queue.status.remove(&job.session_dir);
            }
        }
    }

    /// セッションの進行状況。マップに載っていなければ `None`
    /// （表示側が `summary.md` の有無で「未生成/生成済み」を解決する。
    /// `main::summary_display_status`）。
    pub fn status_of(&self, session_dir: &Path) -> Option<SummarizeStatus> {
        // **`state_of` へ委譲しない**（理由は `TranscribeWorker::status_of`）。こちらは
        // 委譲すると、状態 1 つを読むのに `queued_position` のマップ全走査まで付いてくる。
        lock_queue(&self.queue)
            .status
            .get(session_dir)
            .map(|(_, entry)| entry.status())
    }

    /// セッションの進行状況と、読む領域に出す中身（モデル名・順番・経過・失敗の理由）。
    /// **`status_of` はこれの一部**なので、状態と説明が食い違わない。
    pub fn state_of(&self, session_dir: &Path) -> Option<SummarizeState> {
        let queue = lock_queue(&self.queue);
        let (seq, entry) = queue.status.get(session_dir)?;
        Some(match entry {
            SummarizeEntry::Queued => SummarizeState::Queued {
                position: queue.queued_position(*seq),
            },
            SummarizeEntry::Summarizing {
                model_label,
                started,
            } => SummarizeState::Summarizing {
                model_label: model_label.clone(),
                elapsed: started.elapsed(),
            },
            SummarizeEntry::Done => SummarizeState::Done,
            SummarizeEntry::Failed { reason } => SummarizeState::Failed {
                reason: reason.clone(),
            },
        })
    }

    /// キュー待ちのジョブを取り消す（取り消せたら `true`）。
    ///
    /// **取り消せるのはまだ走り出していないジョブだけ**。生成中（`Summarizing`）のジョブは
    /// 止められないので `false` を返す: 重い区間は `on_device::generate` の中（llama.cpp の
    /// `decode` 呼び出し）で、そこから抜ける口が無い。中断できるようにするなら生成ループへ
    /// 中断フラグを見る箇所を作る必要があり、#133 ではキュー待ちの取り消しに絞った。
    ///
    /// **仕組み**: `mpsc` は積んだジョブを取り出せないので、キューからは消さず**状態マップの
    /// エントリを消す**。ワーカーは取り出したジョブの通番がマップに載っているときだけ走らせる
    /// ので、載っていないジョブ（＝取り消した／追い越された）はそこで捨てられる（`StatusMap`）。
    /// 「取り消したジョブの印」を別に持たないため、取り消し → 積み直し → 取り消しでも、
    /// 同じセッションのジョブが同時に何本キューに載っていても、走るのは常に最後の 1 本だけ。
    /// 状態を消すことで、表示は `summary.md` の有無ベース（未生成／生成済み）へ戻る。
    #[must_use]
    pub fn cancel(&self, session_dir: &Path) -> bool {
        matches!(self.cancel_queued(session_dir), CancelOutcome::Cancelled)
    }

    /// `cancel` の結果つき版。**生成中だったこと**（`Running`）を呼び出し側が区別できる。
    ///
    /// 削除はこちらを使う: 「生成中か」を別に問い合わせてから取り消すと、その隙間でワーカーが
    /// ジョブを取り出し、生成中のセッションを削除してしまう（呼び出し側が 2 回ロックを取るので、
    /// UI の tick 遅れよりずっと短いが必ず開く窓）。1 回のロックで判定と取り消しをまとめる。
    #[must_use]
    pub fn cancel_queued(&self, session_dir: &Path) -> CancelOutcome {
        let mut queue = lock_queue(&self.queue);
        match queue
            .status
            .get(session_dir)
            .map(|(_, entry)| entry.status())
        {
            Some(SummarizeStatus::Summarizing) => CancelOutcome::Running,
            Some(SummarizeStatus::Queued) => {
                queue.status.remove(session_dir);
                CancelOutcome::Cancelled
            }
            _ => CancelOutcome::NotQueued,
        }
    }

    /// 要約のジョブが在るか（**キュー待ちを含む**）。モデル一覧の削除可否に使う（#117）。
    ///
    /// キュー待ちも数えるのは、破壊的操作のガードを**安全側に転ばせる**ため
    /// （消してもジョブは失敗せず 4.4GB を再取得するだけだが、それは待たせるだけで誰の得にも
    /// ならない）。文字起こし側（`TranscribeWorker::has_pending_jobs`）がキュー待ちを含むのと
    /// 揃える。判定が種別単位である理由と、数える範囲（投入済みのジョブだけ）もそちらと同じ。
    pub fn has_pending_jobs(&self) -> bool {
        lock_queue(&self.queue)
            .status
            .values()
            .any(|(_, entry)| counts_as_pending(entry.status()))
    }

    /// セッションの進行状況の記録を破棄する（セッション削除時の掃除）。未登録なら何もしない。
    /// キュー待ちのジョブが残っていても、記録が無くなるのでワーカーが取り出したときに
    /// 捨てられる（`QueueState`）。
    pub fn forget(&self, session_dir: &Path) {
        lock_queue(&self.queue).status.remove(session_dir);
    }
}

/// セッションの `summary.md` を読む（表示用）。未生成・欠落は `None`、読み取り失敗・過大・
/// 非通常ファイルもログして `None` にする（縮退。アプリは落とさない）。
///
/// ガードの理由は `transcript.rs` の `read_guarded` と同じ（保存先の生成物は手で置換されうる
/// 信頼境界外の入力）。ログに出すのは**セッション名（日時のディレクトリ名）とファイル名だけ**:
/// フルパス（保存先）も本文（発話由来の議事録）も機微情報なので出さない
/// （`docs/rules/security.md`）。
pub fn load_summary(session_dir: &Path, fetch: crate::dataless::Fetch) -> Summary {
    load_summary_limited(session_dir, MAX_SUMMARY_BYTES, fetch)
}

/// 読めた議事録と、**実体が無くて読めなかったか**（#182）。理由は
/// `transcript::Segments`（本文だけ見て「無い」と決めると、検索が黙って対象から外したことに
/// 気づけない）。
pub struct Summary {
    /// 読めた本文。未生成・空・破損・過大は `None`。
    pub text: Option<String>,
    /// 実体がこの Mac に無くて読めなかった。**`Fetch::Allowed` では常に `false`**。
    pub not_downloaded: bool,
}

impl Summary {
    /// 読めなかった（理由は問わない）。
    fn nothing() -> Self {
        Self {
            text: None,
            not_downloaded: false,
        }
    }

    /// 読み取りに失敗したときに、何を読めたことにするか（#182。理由は文字起こし側の対
    /// `transcript::read_outcome_from` と同じ）。
    fn from_failure(failure: ReadFailure) -> Self {
        match failure {
            // 待っても直らない（未生成・破損・権限）。
            ReadFailure::NotCreated | ReadFailure::Failed => Self::nothing(),
            // 取り寄せれば読める。
            ReadFailure::NotDownloaded => Self {
                text: None,
                not_downloaded: true,
            },
        }
    }
}

/// `load_summary` の本体。上限はテスト容易性のため引数で受ける（`write_verified` の
/// `max_bytes` と同じ理由。境界のオフバイワンを小さな上限で検証できるようにする）。
fn load_summary_limited(
    session_dir: &Path,
    max_bytes: u64,
    fetch: crate::dataless::Fetch,
) -> Summary {
    // **写像はここ 1 箇所**（#182）。読み取り側が失敗の理由を返し、それを表示用の値へ
    // 落とすのをここだけにしてある——`Summary::nothing()` を直接返す経路を読み取りの
    // 途中に置くと、実体が無いだけのファイルが「読めなかった」に化けて検索から静かに消える。
    match read_summary_text(session_dir, max_bytes, fetch) {
        Ok(text) => Summary {
            text: Some(text),
            not_downloaded: false,
        },
        Err(failure) => Summary::from_failure(failure),
    }
}

/// `summary.md` を信頼境界外の入力として読む（#182 で失敗の理由を返すようにした）。
///
/// **失敗はすべて `ReadFailure` で返す**。ログを出すかもここで決めるが、判断そのものは
/// `ReadFailure::should_report` が持つ（頼まれていない読み取りでは 1 行も出さない）。
///
/// ガードの理由は `transcript.rs` の `read_guarded` と同じ（保存先の生成物は手で置換されうる
/// 信頼境界外の入力）。ログに出すのは**セッション名（日時のディレクトリ名）とファイル名だけ**:
/// フルパス（保存先）も本文（発話由来の議事録）も機微情報なので出さない
/// （`docs/rules/security.md`）。
fn read_summary_text(
    session_dir: &Path,
    max_bytes: u64,
    fetch: crate::dataless::Fetch,
) -> Result<String, ReadFailure> {
    use std::io::Read;

    let path = session_dir.join(SUMMARY_FILENAME);
    // ログ用のセッション識別子（日時のディレクトリ名だけ）。フルパス（保存先）は出さない
    // （`docs/rules/security.md`）。名前が取れない異常時も固定文字列へ落とし、退避先に
    // パスを混ぜない。
    let session = session_dir
        .file_name()
        .map_or(std::borrow::Cow::Borrowed("unknown"), |name| {
            name.to_string_lossy()
        });
    // **失敗の理由とログを 1 箇所で決める**。経路ごとに `eprintln!` を書くと、頼まれて
    // いない読み取りで黙らせる約束が片方だけ守られる。
    let report = |failure: ReadFailure, reason: std::fmt::Arguments| {
        if failure.should_report(fetch) {
            eprintln!("Skipping the summary of {session} because {SUMMARY_FILENAME} {reason}");
        }
        failure
    };

    let file = std::fs::File::open(&path).map_err(|err| {
        // **開くときも読むときも同じ見分けを通す**（`Fetch::classify` の doc）。
        report(
            fetch.classify(err.kind()),
            format_args!("could not be opened: {err}"),
        )
    })?;
    // 開いたハンドルの fstat で通常ファイルを確認し（FIFO 等は読み終わらないことがある）、
    // サイズ上限は読み込みそのものに掛ける（事前の metadata 判定では差し替えに追従できない）。
    if let Ok(meta) = file.metadata()
        && !meta.is_file()
    {
        return Err(report(
            ReadFailure::Failed,
            format_args!("is not a regular file"),
        ));
    }
    let mut limited = file.take(max_bytes + 1);
    let mut text = String::new();
    if let Err(err) = limited.read_to_string(&mut text) {
        // 実測では、退避されたファイルは `open` が通ってここで返る（見分けは
        // `dataless::is_not_downloaded` の doc）。UTF-8 でない（破損・別物への置換）場合も
        // ここに来る。
        return Err(report(
            fetch.classify(err.kind()),
            format_args!("could not be read: {err}"),
        ));
    }
    // 上限＋1 バイトまで読み切った（limit が尽きた）なら上限超過。
    if limited.limit() == 0 {
        return Err(report(ReadFailure::Failed, format_args!("is too large")));
    }
    // 空ファイル（生成が中途で終わった等）は「無い」と同じ扱いにする（縮退表示へ落とす）。
    if text.trim().is_empty() {
        return Err(ReadFailure::NotCreated);
    }
    Ok(text)
}

/// 追い越されたジョブが終わったときの後始末: 表示が「生成中」のまま残っていたら
/// 「キュー待ち」へ戻す。
///
/// `submit` は生成中のセッションへ積んでも表示を下げない（下げると走行中に Delete と
/// 取り消しを開けてしまう）ので、ここで戻さないと**誰も走っていないのに生成中のまま**になり、
/// 後続が取り出されるまで（他セッションのジョブを挟むと数分）取り消しも削除もできない。
/// ワーカーは 1 本なので、この時点で後続はまだ走っていない＝キュー待ちが実態。
///
/// 取り消し済み・削除済み（エントリが無い）セッションを復活させないよう、在るときだけ触る。
fn demote_superseded(queue: &mut QueueState, session_dir: &Path) {
    // **変種ごと差し替える**ので、開始時刻の消し忘れが起こりえない（#159）。
    if let Some((_, entry)) = queue.status.get_mut(session_dir)
        && matches!(entry, SummarizeEntry::Summarizing { .. })
    {
        *entry = SummarizeEntry::Queued;
    }
}

/// キュー状態のガードを取る。poison（ロック保持中のパニック）でも状態表示を止めないため、
/// ガードを取り出して続行する（`docs/rules/error-handling.md`）。
fn lock_queue(queue: &Mutex<QueueState>) -> MutexGuard<'_, QueueState> {
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 1 ジョブの処理結果（状態マップへの反映用）。
enum JobOutcome {
    /// `summary.md` を保存した。
    Done,
    /// 生成・保存に失敗した（モデル準備の失敗を含む）。文言は読む領域が組む。
    Failed(SummarizeFailure),
    /// 対象なしで何もしなかった（文字起こしが無い・空）。
    Skipped,
}

/// 1 セッション分の要約を生成して保存する。モデルはジョブ内で 1 回だけロードする
/// （`docs/rules/performance.md`）。対象が無ければロードもダウンロードもしない。
fn run_job(
    job: &SummarizeJob,
    downloader: &crate::model_download::ModelDownloader,
    slot: &crate::inference_slot::InferenceSlot,
) -> JobOutcome {
    // トランスクリプトは行へ整形したら手放す（数分かかる推論の間、同じ内容を 2 重に抱えない。
    // `docs/rules/performance.md`）。
    let lines = {
        // **本文しか要らない**（揃っているかの判断は読む領域の仕事。#175）。
        // **取り寄せてよい**（#182）——ユーザーが頼んだ生成なので、退避されていれば落として
        // でも読む。読めなければ下の空判定が縮退する。
        let segments =
            crate::transcript::load_segments(&job.session_dir, crate::dataless::Fetch::Allowed);
        transcript_lines(&segments.segments)
    };
    if lines.is_empty() {
        // 文字起こしが未生成・欠落・破損・全行空。GB 級のモデルをロードしない防御でもある。
        println!("Skipping summarization because the session has no transcript");
        return JobOutcome::Skipped;
    }

    let path = job.session_dir.join(SUMMARY_FILENAME);
    let generated = match job.engine {
        SummaryEngine::OnDevice => {
            let Some(model_path) = resolve_model(job, downloader) else {
                return failed(job, &path, SummarizeFailure::ModelPrepare);
            };
            // ここから先が重い区間。文字起こしと同時に走らせない（`crate::inference_slot`）。
            // モデルの準備（ダウンロード）はスロットの外で済ませてある。
            let _slot = slot.acquire();
            on_device::generate(&model_path, &job.language, &lines)
        }
    };
    let generated = match generated {
        Ok(text) => text,
        Err(err) => {
            eprintln!("Skipping summarization because generating the summary failed: {err}");
            // **なぜ落ちたかは分からない**（llama.cpp の失敗は理由を返さないことが多い）ので、
            // 断定せずに「いちばんよくある原因」と、そこから取れる手を添える。
            return failed(job, &path, SummarizeFailure::ModelRun);
        }
    };
    // 空（または空白だけ）の生成結果は失敗として扱う。空ファイルを置くと、表示側が
    // 「生成済み」と読んで白紙を出してしまう。
    let generated = generated.trim();
    if generated.is_empty() {
        eprintln!("Skipping summarization because the model produced no text");
        return failed(job, &path, SummarizeFailure::EmptyOutput);
    }

    match write_summary(&path, generated) {
        Ok(()) => {
            // 保存先のフルパス（＝録音の所在）と本文（＝発話内容）はログへ出さない
            // （`docs/rules/security.md`）。
            println!("Saved the meeting summary ({} characters)", generated.len());
            JobOutcome::Done
        }
        Err(err) => {
            eprintln!("Skipping summarization because writing the summary failed: {err}");
            failed(job, &path, SummarizeFailure::Save)
        }
    }
}

/// 生成に失敗したときの後始末（結果は常に `Failed`）。
///
/// `write_summary` が原子的に置き換えるので、失敗した時点で既にある `summary.md` は手つかず。
/// **それが古いと分かっているジョブ（`existing_is_stale`）だけ消す**（判断の理由はその
/// フィールドの doc）。
fn failed(job: &SummarizeJob, path: &Path, reason: SummarizeFailure) -> JobOutcome {
    if job.existing_is_stale {
        remove_stale_summary(path);
    }
    JobOutcome::Failed(reason)
}

/// 古い（＝ひとつ前の文字起こしから作った）議事録を消す。無ければ何もしない。
/// 呼ばれるのは生成に失敗したときだけ（成功時は `write_summary` が原子的に置き換える）。
fn remove_stale_summary(path: &Path) {
    if let Err(err) = std::fs::remove_file(path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("Could not remove the stale meeting summary after a failed run: {err}");
    }
}

/// ジョブが使う LLM の表示名。上書き指定は**ファイル名だけ**にする（読む領域にそのまま出るので、
/// パスを漏らさない。`docs/rules/security.md`）。
fn job_model_label(job: &SummarizeJob) -> String {
    match &job.model_override {
        Some(path) => path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Custom model".to_owned()),
        None => crate::summary_model::spec_or_default(&job.model_id)
            .display_name
            .to_owned(),
    }
}

/// 使うモデルファイルを決める。上書き指定があればそれを、無ければ設定で選ばれた内蔵モデルを
/// 使う（未取得ならここで自動ダウンロードする。UI 起点のダウンロード中なら完了を待つ。
/// ワーカースレッド上なので分オーダーかかっても UI は塞がない）。
fn resolve_model(
    job: &SummarizeJob,
    downloader: &crate::model_download::ModelDownloader,
) -> Option<PathBuf> {
    let path = match &job.model_override {
        Some(path) => path.clone(),
        None => {
            let spec = crate::summary_model::spec_or_default(&job.model_id);
            match downloader.ensure_model(spec) {
                Ok(path) => path,
                Err(err) => {
                    eprintln!(
                        "Skipping summarization because the summary model could not be prepared: {err}"
                    );
                    return None;
                }
            }
        }
    };
    if !path.is_file() {
        eprintln!(
            "Skipping summarization because the summary model file was not found: {}",
            path.display()
        );
        return None;
    }
    Some(path)
}

/// 議事録を保存する。録音・文字起こしと同じ機微データなので所有者のみ読み書き可で作る
/// （`crate::private_file`）。
fn write_summary(path: &Path, markdown: &str) -> Result<(), Box<dyn std::error::Error>> {
    // 一時ファイルへ書き切ってから rename で置き換える（`crate::atomic_replace`）。直接
    // 上書きすると `truncate` で**開いた時点で**既存の議事録が消え、書き込み中に失敗した場合に
    // (1) 前の議事録が失われ、(2) 途中まで書けたファイルが「生成済み」として表示される
    // （`load_summary` は非空なら返す）。失敗しても一時ファイルは番人が消す。
    // Drop が走らない終わり方で残った `summary.md.part.<pid>` は、Library ウィンドウを
    // 開いたときに回収する（`recordings::spawn_session_part_sweep`）。
    let part = crate::atomic_replace::PartFile::for_dest(path)
        .ok_or("the summary path does not end in a file name")?;
    // 0600 で作る（議事録は発話由来の機微データ。`crate::private_file`）。rename はモードを
    // 保つので、置き換え後も 0600 のまま。
    let mut file = crate::private_file::create(part.path())?;
    file.write_all(markdown.as_bytes())?;
    // Markdown ファイルとして扱いやすいよう末尾を改行で終える。
    file.write_all(b"\n")?;
    // rename の前にハンドルを閉じる（書き込みの失敗を rename より先に見つける）。
    file.sync_all()?;
    drop(file);
    part.commit()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ここから下はモデル非依存の純粋関数（単体テスト対象）。
// ---------------------------------------------------------------------------

/// マージ済みトランスクリプトを、モデルへ渡す 1 発話 1 行のテキストにする。
/// 形式は #78 の検証サンプル（`assets/samples/meeting-*.txt`）と同じ `[mm:ss] Speaker: text`
/// （1 時間を超える録音では `[h:mm:ss]`）。空の発話（whisper が無音区間に付けることがある）は落とす。
///
/// 時刻の整形は表示側と同じ `reading_pane::format_elapsed` を使う（同じ表記の実装を 2 つ持つと、
/// 片方だけ直したときに文字起こし表示とプロンプト内の時刻がずれる）。開始秒は信頼境界外の
/// JSON 由来なので、丸めも表示側と同じ `TranscriptSegment::start_duration` に任せる。
fn transcript_lines(segments: &[TranscriptSegment]) -> Vec<String> {
    segments
        .iter()
        .filter_map(|segment| {
            let text = segment.text.trim();
            if text.is_empty() {
                return None;
            }
            Some(format!(
                "[{}] {}: {text}",
                crate::tray::format_elapsed(segment.start_duration()),
                segment.speaker.label()
            ))
        })
        .collect()
}

/// テキストのトークン数を概算する。
///
/// #78 の実測は日本語 0.627 tok/文字・英語 0.294 tok/文字。ここでは非 ASCII を 0.70、
/// ASCII を 0.35 として切り上げる（実測より高め）。**過小評価だけが害**（チャンクが
/// `n_ctx` を超えて生成そのものが失敗する）なので、余裕を持たせる側へ倒している。
/// 非 ASCII をすべて「広い文字」に数えるのはラテン系の言語では過大評価になるが、
/// これも安全側なので許容する。
fn estimate_tokens(text: &str) -> usize {
    let (wide, narrow) = text.chars().fold((0usize, 0usize), |(wide, narrow), c| {
        if c.is_ascii() {
            (wide, narrow + 1)
        } else {
            (wide + 1, narrow)
        }
    });
    wide.saturating_mul(70)
        .saturating_add(narrow.saturating_mul(35))
        .div_ceil(100)
}

/// ブロック列を `separator` で連結したときのトークン概算。**連結はしない**
/// （長い会議のトランスクリプトは MB 級になりうるので、測るためだけに全文をコピーしない）。
///
/// 各要素の概算の和なので、`estimate_tokens(&blocks.join(sep))` より最大でブロック数ぶん
/// 大きくなる。判定は常に安全側（＝多めに見る側）へ倒したいので、この差は許容する。
fn estimate_joined(blocks: &[String], separator: &str) -> usize {
    let separators = blocks.len().saturating_sub(1);
    blocks
        .iter()
        .map(|block| estimate_tokens(block))
        .sum::<usize>()
        .saturating_add(separators.saturating_mul(estimate_tokens(separator)))
}

/// ブロック（発話行・中間メモ）を順序を保ったまま詰めて、1 つが `max_tokens` の概算に
/// 収まるチャンク列にする。map-reduce の map 段の入力と、メモを畳み直す段の両方で使う。
///
/// 単体で上限を超えるブロックは文字単位で切り分ける（落とさない）。通常の文字起こし
/// セグメントでは起きないが、JSON は手編集されうる信頼境界外なので経路を用意しておく。
fn group_blocks(blocks: &[String], separator: &str, max_tokens: usize) -> Vec<String> {
    let mut chunks: Vec<String> = Vec::new();
    let mut current = String::new();
    for block in blocks {
        if estimate_tokens(block) > max_tokens {
            if !current.is_empty() {
                chunks.push(std::mem::take(&mut current));
            }
            chunks.extend(split_oversized(block, max_tokens));
            continue;
        }
        if current.is_empty() {
            current.push_str(block);
        } else if estimate_tokens(&current) + estimate_tokens(separator) + estimate_tokens(block)
            <= max_tokens
        {
            current.push_str(separator);
            current.push_str(block);
        } else {
            chunks.push(std::mem::replace(&mut current, block.clone()));
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

/// 単体で上限を超えるブロックを、文字境界で上限以下の断片へ切り分ける。
fn split_oversized(block: &str, max_tokens: usize) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut rest = block;
    while !rest.is_empty() {
        let head = truncate_to_budget(rest, max_tokens);
        // 予算 0 だと 1 文字も入らず無限ループになる。呼び出し側は正の予算しか渡さないが、
        // 静かに回り続けるより残り全部を 1 断片にして抜ける。
        if head.is_empty() {
            pieces.push(rest.to_owned());
            break;
        }
        pieces.push(head.to_owned());
        rest = &rest[head.len()..];
    }
    pieces
}

/// 先頭から、トークン概算が `max_tokens` に収まるところまでを返す（文字境界で切る）。
/// 全体が収まるならそのまま返す。
fn truncate_to_budget(text: &str, max_tokens: usize) -> &str {
    let mut tokens = 0usize;
    for (offset, c) in text.char_indices() {
        let cost = if c.is_ascii() { 35 } else { 70 };
        // 端数の扱いを `estimate_tokens` と揃えるため、同じ切り上げで比べる。
        if (tokens + cost).div_ceil(100) > max_tokens {
            return &text[..offset];
        }
        tokens += cost;
    }
    text
}

/// 出力言語を特定できないときにプロンプトへ埋め込む指定（`auto`・カタログ外の手編集値）。
const SAME_LANGUAGE_AS_TRANSCRIPT: &str = "the same language as the transcript";

/// 要約の出力言語の決め方。認識言語設定（`transcribe_language`）から決まる。
/// **プロンプトの分岐はこの enum だけで決める**（表示名の文字列比較で再導出しない。
/// 表示名は Select 用の UI 文言なので、変えるとプロンプトの挙動が黙って変わってしまう）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputLanguage {
    /// 日本語。#78 で検証済みの専用プロンプト（見出しも日本語）を使う。
    Japanese,
    /// 英語。#78 で検証済みの骨組みをそのまま使う（見出しも英語のまま）。
    English,
    /// それ以外。英語の骨組みに出力言語をこの語で指定し、見出しも訳させる
    /// （例: `Chinese` / `the same language as the transcript`）。
    Other(&'static str),
}

/// 認識言語コードから出力言語を決める。
///
/// `ja` と `en` は #78 で品質を検証済み。それ以外のカタログ言語は英語のプロンプト骨組みに
/// 出力言語を指定して渡す（見出しも訳させる）。`auto` とカタログ外の手編集値は
/// 「文字起こしと同じ言語」を指示する（コードから表示名を決められないため）。
fn output_language(code: &str) -> OutputLanguage {
    match code {
        "ja" => return OutputLanguage::Japanese,
        "en" => return OutputLanguage::English,
        _ => {}
    }
    match crate::config::TRANSCRIBE_LANGUAGES
        .iter()
        .find(|(c, _)| *c == code)
    {
        // `auto` はカタログにあるが、表示名（`Auto detect`）は出力言語の指定に使えない。
        Some((c, _)) if *c == "auto" => OutputLanguage::Other(SAME_LANGUAGE_AS_TRANSCRIPT),
        Some((_, display)) => OutputLanguage::Other(display),
        None => OutputLanguage::Other(SAME_LANGUAGE_AS_TRANSCRIPT),
    }
}

/// 議事録生成の system プロンプト（日本語）。#78 の検証で確定したもの。
///
/// **人名入りの few-shot 例は置かない**。検証中、例に書いた人名が
/// (1) 評価サンプルと重なると「抽出できたのか写しただけか」を区別できず品質評価が汚染され、
/// (2) 重ならない名前にすると、今度は**その名前が架空の担当者として出力に漏れた**。
/// 形は言葉で説明し、担当は「文字起こしに出てきた人だけ」と制約する。
///
/// **これはユーザー向け文言ではなくモデルへの入力データ**なので、日本語のまま置く
/// （`docs/rules/messages.md` は GUI ラベルやログを対象にしており、ここは対象外）。
/// 出力言語は「何語で指示するか」で決まるため、英語に直すと出力そのものが変わる。
const MINUTES_SYSTEM_JA: &str = "あなたは会議の書記です。文字起こしから議事録を作成します。\n\
     出力は日本語の Markdown のみ。前置き・後書きを書かない。\n\
     次の 4 つの見出しをこの順で必ず使い、それぞれの役割どおりに書き分ける。\n\
     \n\
     ## 議事概要\n\
     会議全体を 2〜3 行で要約する。詳細は書かない。\n\
     \n\
     ## 議題内容\n\
     話題ごとに `### 見出し` を作り、その下に議論の中身を箇条書きにする。\n\
     誰が何を言ったか・検討した選択肢・結論に至った理由を残す。ここが本文なので\n\
     最も詳しく書く。\n\
     \n\
     ## 決定事項\n\
     会議で決まったことだけを箇条書きにする。判断の基準や条件も含める。\n\
     \n\
     ## アクションアイテム\n\
     次の形の箇条書きにする（山かっこはプレースホルダ。そのまま出力しない）:\n\
     `- <担当者名>: <やること>（<期限>）`\n\
     担当者名は**文字起こしに出てきた話者名・人名だけ**を使う。分からなければ `未定`。\n\
     期限が言及されていなければ丸かっこごと省く。書いてよいのは、文字起こしで誰かが\n\
     「やる」と言ったことだけ。\n\
     \n\
     守ること:\n\
     - 文字起こしに書かれていないことを推測して書かない。\n\
     - 該当が無い見出しには「なし」とだけ書く。\n\
     - 「Mic:」は書記自身（自分）の発話、「System:」は相手側の発話。";

/// 議事録生成の system プロンプト（英語骨組み）。`{0}` に出力言語、`{1}` に見出しの扱いが入る。
/// 日本語版と同じ構成にして、言語だけが違う状態にする。
const MINUTES_SYSTEM_EN: &str = "You are a meeting scribe. You write minutes from a transcript.\n\
     Write the minutes in {0}. Output Markdown only. No preamble, no closing remarks.\n\
     Use these four headings in this order, each for its own purpose. {1}\n\
     \n\
     ## Summary\n\
     Two or three lines covering the whole meeting. No detail here.\n\
     \n\
     ## Discussion\n\
     One `### heading` per topic, with bullets underneath. Keep who said what,\n\
     the options considered, and why the group landed where it did. This is the\n\
     body of the minutes, so it is the most detailed section.\n\
     \n\
     ## Decisions\n\
     Only what the meeting actually decided, as bullets. Include the criteria or\n\
     conditions attached to a decision.\n\
     \n\
     ## Action Items\n\
     Bullets in this shape (angle brackets are placeholders; never output them):\n\
     `- <owner>: <what to do> (<when>)`\n\
     The owner must be a speaker or a person named in the transcript; use `Unassigned` if\n\
     nobody is named. Drop the parenthetical if no timing was mentioned. Only list things\n\
     someone said they would do.\n\
     \n\
     Rules:\n\
     - Do not add anything that is not in the transcript.\n\
     - Write \"None\" under a heading with no content.\n\
     - \"Mic:\" is the scribe speaking; \"System:\" is the other participants.";

/// 中間メモ（map 段）の system プロンプト（日本語）。ここでは議事録の形にせず、
/// 後段がまとめるための素材だけを残す（形を作らせると reduce で二重に要約されて情報が落ちる）。
const NOTES_SYSTEM_JA: &str = "あなたは会議の書記です。長い文字起こしを分割した一部分を読み、\n\
     あとで議事録にまとめるためのメモを作ります。\n\
     出力は日本語の Markdown の箇条書きのみ。前置き・後書き・見出しを書かない。\n\
     次を漏らさず、それぞれ 1 行の箇条書きにする:\n\
     - 話された話題と議論の要点（誰が何を言ったか・検討した選択肢・結論の理由）\n\
     - 決まったこと\n\
     - 誰かが「やる」と言ったこと（担当者名と、言及があれば期限）\n\
     \n\
     守ること:\n\
     - 文字起こしに書かれていないことを推測して書かない。\n\
     - 担当者名は文字起こしに出てきた話者名・人名だけを使う。\n\
     - 「Mic:」は書記自身（自分）の発話、「System:」は相手側の発話。";

/// 中間メモ（map 段）の system プロンプト（英語骨組み）。`{0}` に出力言語が入る。
const NOTES_SYSTEM_EN: &str = "You are a meeting scribe. You are reading one part of a long\n\
     transcript and taking notes that will be turned into minutes later.\n\
     Write the notes in {0}. Output Markdown bullets only. No preamble, no closing\n\
     remarks, no headings.\n\
     Cover all of the following, one bullet each:\n\
     - Topics discussed and the substance (who said what, options considered, why)\n\
     - Anything the meeting decided\n\
     - Anything someone said they would do (with the owner, and the timing if mentioned)\n\
     \n\
     Rules:\n\
     - Do not add anything that is not in the transcript.\n\
     - The owner must be a speaker or a person named in the transcript.\n\
     - \"Mic:\" is the scribe speaking; \"System:\" is the other participants.";

/// 議事録生成の入力が何か（user メッセージの言い回しを変える）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MinutesSource {
    /// 文字起こしそのもの（チャンク分割なしの 1 段生成）。
    Transcript,
    /// チャンクごとの中間メモ（map-reduce の reduce 段）。
    Notes,
}

/// 議事録生成の system プロンプトを、出力言語に合わせて組み立てる。
fn minutes_system_prompt(language: &str) -> String {
    // 英語出力なら見出しは骨組みのまま。それ以外は見出しも訳させる（要約の言語は認識言語に
    // 追従させる、という要件のため）。
    let (name, headings) = match output_language(language) {
        OutputLanguage::Japanese => return MINUTES_SYSTEM_JA.to_owned(),
        OutputLanguage::English => ("English", "Keep the headings exactly as written."),
        OutputLanguage::Other(name) => (
            name,
            "Translate the four headings into that language, keeping this order and meaning.",
        ),
    };
    MINUTES_SYSTEM_EN
        .replace("{0}", name)
        .replace("{1}", headings)
}

/// 議事録生成の user プロンプトを組み立てる。
fn minutes_user_prompt(language: &str, source: MinutesSource, body: &str) -> String {
    match (output_language(language), source) {
        (OutputLanguage::Japanese, MinutesSource::Transcript) => {
            format!("次の文字起こしから議事録を作成してください。\n\n{body}")
        }
        (OutputLanguage::Japanese, MinutesSource::Notes) => {
            format!(
                "次は 1 つの会議の文字起こしを分割して作ったメモです（時系列順）。\
                 これらをまとめて議事録を作成してください。\n\n{body}"
            )
        }
        (OutputLanguage::English | OutputLanguage::Other(_), MinutesSource::Transcript) => {
            format!("Write the minutes for the following transcript.\n\n{body}")
        }
        (OutputLanguage::English | OutputLanguage::Other(_), MinutesSource::Notes) => {
            format!(
                "The following are notes taken from consecutive parts of one meeting, in order. \
                 Write the minutes for the meeting as a whole.\n\n{body}"
            )
        }
    }
}

/// 中間メモ（map 段）の system プロンプトを、出力言語に合わせて組み立てる。
fn notes_system_prompt(language: &str) -> String {
    match output_language(language) {
        OutputLanguage::Japanese => NOTES_SYSTEM_JA.to_owned(),
        OutputLanguage::English => NOTES_SYSTEM_EN.replace("{0}", "English"),
        OutputLanguage::Other(name) => NOTES_SYSTEM_EN.replace("{0}", name),
    }
}

/// 中間メモ（map 段）の user プロンプトを組み立てる。何番目の断片かを伝えて、
/// 「会議の一部だけを見ている」ことをモデルに明示する（全体の結論を捏造させないため）。
fn notes_user_prompt(language: &str, part: usize, total: usize, body: &str) -> String {
    match output_language(language) {
        OutputLanguage::Japanese => format!(
            "次は会議の文字起こしの一部（全 {total} 部中の {part} 部目）です。\
             メモを作成してください。\n\n{body}"
        ),
        OutputLanguage::English | OutputLanguage::Other(_) => format!(
            "The following is part {part} of {total} of a meeting transcript. \
             Take notes on it.\n\n{body}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Speaker;

    /// モデル名は**ファイル名だけ**になる。上書き指定は任意のパスを取れて、その値は生成中の
    /// 本文としてそのまま画面に出る（`docs/rules/security.md`。`transcribe` 側と対）。
    #[test]
    fn job_model_label_drops_the_directories() {
        let job = |model_override: Option<&str>| SummarizeJob {
            session_dir: PathBuf::from("/tmp/shoki-label"),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: model_override.map(PathBuf::from),
            language: "ja".to_owned(),
            existing_is_stale: false,
        };

        assert_eq!(
            job_model_label(&job(Some("/Users/someone/models/qwen2.5-3b.gguf"))),
            "qwen2.5-3b.gguf"
        );
        assert_eq!(job_model_label(&job(Some("/"))), "Custom model");
        assert_eq!(
            job_model_label(&job(None)),
            crate::summary_model::default_spec().display_name
        );
    }

    /// テスト用のキューのエントリ（状態だけ指定し、ペイロードは既定で埋める）。
    fn test_entry(status: SummarizeStatus) -> SummarizeEntry {
        let model_label = "Qwen2.5 3B Instruct".to_owned();
        match status {
            SummarizeStatus::Queued => SummarizeEntry::Queued,
            SummarizeStatus::Summarizing => SummarizeEntry::Summarizing {
                model_label,
                started: Instant::now(),
            },
            SummarizeStatus::Done => SummarizeEntry::Done,
            SummarizeStatus::Failed => SummarizeEntry::Failed {
                reason: SummarizeFailure::ModelRun,
            },
        }
    }

    fn segment(start_secs: f64, speaker: Speaker, text: &str) -> TranscriptSegment {
        TranscriptSegment {
            start_secs,
            text: text.to_owned(),
            speaker,
        }
    }

    #[test]
    fn transcript_lines_use_the_probe_format_and_drop_empty_text() {
        let segments = vec![
            segment(0.0, Speaker::Mic, "hello"),
            // 空・空白だけの発話は落とす（whisper が無音区間に付けることがある）。
            segment(5.0, Speaker::System, "   "),
            segment(65.4, Speaker::System, "  reply  "),
            segment(3_725.0, Speaker::Mic, "much later"),
        ];
        assert_eq!(
            transcript_lines(&segments),
            vec![
                "[00:00] Mic: hello".to_owned(),
                "[01:05] System: reply".to_owned(),
                // 1 時間を超えたら時間まで出す。
                "[1:02:05] Mic: much later".to_owned(),
            ]
        );
    }

    #[test]
    fn transcript_lines_tolerate_broken_timestamps() {
        // 開始秒は手編集されうる JSON 由来。負・非有限・巨大値でもパニックせず 0 になる
        // （丸めは表示側と同じ `TranscriptSegment::start_duration`）。
        let segments = vec![
            segment(-3.0, Speaker::Mic, "negative"),
            segment(f64::NAN, Speaker::Mic, "nan"),
            segment(f64::INFINITY, Speaker::Mic, "inf"),
        ];
        assert_eq!(
            transcript_lines(&segments),
            vec![
                "[00:00] Mic: negative".to_owned(),
                "[00:00] Mic: nan".to_owned(),
                "[00:00] Mic: inf".to_owned(),
            ]
        );
    }

    #[test]
    fn estimate_tokens_stays_above_the_measured_ratio() {
        // #78 の実測（日本語 0.627 / 英語 0.294 tok/文字）より高めに出ること。過小評価だけが
        // 害（チャンクが n_ctx を超える）なので、この向きの不等式を固定する。
        let japanese: String = "議事録".repeat(100);
        let english: String = "meeting ".repeat(100);
        assert!(estimate_tokens(&japanese) as f64 >= japanese.chars().count() as f64 * 0.627);
        assert!(estimate_tokens(&english) as f64 >= english.chars().count() as f64 * 0.294);
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn group_blocks_packs_in_order_within_the_budget() {
        let blocks: Vec<String> = (0..6).map(|i| format!("line {i} aaaa")).collect();
        // 1 ブロックは 12 文字 ASCII = 5 トークン概算。予算 12 なら区切り込みで 2 本ずつ入る。
        let chunks = group_blocks(&blocks, "\n", 12);
        assert!(chunks.len() > 1, "the budget should force several chunks");
        for chunk in &chunks {
            assert!(estimate_tokens(chunk) <= 12, "chunk over budget: {chunk}");
        }
        // 時系列（元の順序）が保たれ、内容も落ちていない。
        assert_eq!(chunks.join("\n"), blocks.join("\n"));
    }

    #[test]
    fn group_blocks_splits_a_single_oversized_block() {
        // 1 ブロックだけで予算を超える場合も落とさず、上限以下の断片へ切り分ける。
        let long = "あ".repeat(100);
        let chunks = group_blocks(std::slice::from_ref(&long), "\n", 10);
        assert!(chunks.len() > 1);
        for chunk in &chunks {
            assert!(estimate_tokens(chunk) <= 10, "chunk over budget");
        }
        assert_eq!(chunks.concat(), long);
    }

    #[test]
    fn group_blocks_handles_empty_input() {
        assert!(group_blocks(&[], "\n", 100).is_empty());
    }

    #[test]
    fn estimate_joined_never_underestimates_the_join() {
        // 連結せずに測る近似。**下回らない**ことが要点（下回るとチャンクが n_ctx を超える）。
        let blocks: Vec<String> =
            vec!["あいうえお".to_owned(), "hello".to_owned(), "か".to_owned()];
        assert!(estimate_joined(&blocks, "\n") >= estimate_tokens(&blocks.join("\n")));
        // 区切り文字ぶんを足さないと**下回る**入力。ASCII 20 文字 × 2 は各 7 トークン概算
        // （和 14）だが、連結すると 41 文字で 15 になる。ここで不等式が効く。
        let tight: Vec<String> = vec!["a".repeat(20), "a".repeat(20)];
        assert!(estimate_joined(&tight, "\n") >= estimate_tokens(&tight.join("\n")));
        // 区切りは要素数 - 1 個ぶんだけ数える（空・単数で過大にしない）。
        assert_eq!(estimate_joined(&[], "\n"), 0);
        assert_eq!(
            estimate_joined(std::slice::from_ref(&blocks[0]), "\n"),
            estimate_tokens(&blocks[0])
        );
    }

    #[test]
    fn truncate_to_budget_cuts_on_char_boundaries() {
        let text = "あいうえおかきくけこ";
        let head = truncate_to_budget(text, 3);
        assert!(estimate_tokens(head) <= 3);
        // 文字境界で切れている（切れていればそのまま部分文字列として一致する）。
        assert!(text.starts_with(head));
        assert!(!head.is_empty());
        // 収まるならそのまま返す。
        assert_eq!(truncate_to_budget(text, 1_000), text);
        assert_eq!(truncate_to_budget("", 10), "");
        // 予算 0 では 1 文字も入らない。`split_oversized` はこれをガードして前へ進む
        // （ガードを外すと無限ループになるので、テストで固定しておく）。
        assert_eq!(truncate_to_budget("あ", 0), "");
        assert_eq!(split_oversized("ab", 0), vec!["ab".to_owned()]);
    }

    #[test]
    fn output_language_follows_the_transcription_setting() {
        assert_eq!(output_language("ja"), OutputLanguage::Japanese);
        assert_eq!(output_language("en"), OutputLanguage::English);
        // カタログにある他の言語は表示名で指定する。
        assert_eq!(output_language("ko"), OutputLanguage::Other("Korean"));
        // `auto` とカタログ外（手編集値）は「文字起こしと同じ言語」を指示する。
        assert_eq!(
            output_language("auto"),
            OutputLanguage::Other(SAME_LANGUAGE_AS_TRANSCRIPT)
        );
        assert_eq!(
            output_language("xx"),
            OutputLanguage::Other(SAME_LANGUAGE_AS_TRANSCRIPT)
        );
    }

    #[test]
    fn minutes_prompt_headings_follow_the_language() {
        // 日本語は専用プロンプト（見出しも日本語）。
        let ja = minutes_system_prompt("ja");
        for heading in [
            "## 議事概要",
            "## 議題内容",
            "## 決定事項",
            "## アクションアイテム",
        ] {
            assert!(ja.contains(heading), "missing {heading}");
        }

        // 英語は骨組みのまま・見出しは英語で固定。
        let en = minutes_system_prompt("en");
        for heading in [
            "## Summary",
            "## Discussion",
            "## Decisions",
            "## Action Items",
        ] {
            assert!(en.contains(heading), "missing {heading}");
        }
        assert!(en.contains("Write the minutes in English."));
        assert!(en.contains("Keep the headings exactly as written."));

        // それ以外の言語は骨組み＋出力言語の指定で、見出しも訳させる。
        let ko = minutes_system_prompt("ko");
        assert!(ko.contains("Write the minutes in Korean."));
        assert!(ko.contains("Translate the four headings"));

        let auto = minutes_system_prompt("auto");
        assert!(auto.contains("Write the minutes in the same language as the transcript."));

        // プレースホルダが残っていないこと（置換漏れの検知）。
        for prompt in [&ja, &en, &ko, &auto] {
            assert!(!prompt.contains("{0}"), "unreplaced placeholder");
            assert!(!prompt.contains("{1}"), "unreplaced placeholder");
        }
    }

    #[test]
    fn minutes_prompt_never_contains_a_person_name_example() {
        // #78 の教訓: アクションアイテムの書式に人名入りの例を置くと、その人名が架空の担当者
        // として出力へ漏れる。山かっこのプレースホルダであることを固定する。
        assert!(minutes_system_prompt("ja").contains("`- <担当者名>: <やること>（<期限>）`"));
        assert!(minutes_system_prompt("en").contains("`- <owner>: <what to do> (<when>)`"));
    }

    #[test]
    fn user_prompts_carry_the_body_and_the_source_kind() {
        let ja = minutes_user_prompt("ja", MinutesSource::Transcript, "[00:00] Mic: あ");
        assert!(ja.contains("[00:00] Mic: あ"));
        assert!(ja.contains("文字起こし"));

        // reduce 段は「メモをまとめる」と伝える（文字起こしそのものだと誤認させない）。
        let ja_notes = minutes_user_prompt("ja", MinutesSource::Notes, "- メモ");
        assert!(ja_notes.contains("メモ"));

        let en_notes = minutes_user_prompt("en", MinutesSource::Notes, "- note");
        assert!(en_notes.contains("notes taken from consecutive parts"));
        assert!(en_notes.contains("- note"));
    }

    /// 実体が無いだけの議事録を「読めなかった」に丸めないこと（#182）。理由と、なぜ
    /// 分類から先しか検査できないかは `transcript` 側の
    /// `a_body_that_is_only_elsewhere_is_not_lost` と同じ。
    #[test]
    fn a_summary_that_is_only_elsewhere_is_not_lost() {
        use crate::dataless::ReadFailure;

        let not_downloaded = Summary::from_failure(ReadFailure::NotDownloaded);
        assert!(not_downloaded.not_downloaded);
        assert!(not_downloaded.text.is_none());

        // 待っても直らないものは、どちらも「無い」側（読めなかったとは数えない）。
        for failure in [ReadFailure::NotCreated, ReadFailure::Failed] {
            let nothing = Summary::from_failure(failure);
            assert!(!nothing.not_downloaded);
            assert!(nothing.text.is_none());
        }
    }

    #[test]
    fn notes_prompts_state_which_part_is_being_read() {
        let ja = notes_user_prompt("ja", 2, 5, "[00:00] Mic: あ");
        assert!(ja.contains("全 5 部中の 2 部目"));
        let en = notes_user_prompt("en", 2, 5, "[00:00] Mic: a");
        assert!(en.contains("part 2 of 5"));
        // map 段は議事録の見出しを作らせない（reduce で二重要約にしないため）。
        assert!(!notes_system_prompt("ja").contains("## 議事概要"));
        assert!(!notes_system_prompt("en").contains("## Summary"));
        assert!(!notes_system_prompt("ko").contains("{0}"));
    }

    /// 表示用の読み込み: 生成物はそのまま読め、未生成・空・非通常ファイル・過大は `None`
    /// （縮退。呼び出し側は状態依存のラベルへ落とす）。上限は引数で受ける版
    /// （`load_summary_limited`）で境界の両側を見る。
    #[test]
    fn load_summary_reads_the_file_and_degrades_on_bad_input() {
        let dir = std::env::temp_dir().join(format!("shoki-summary-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");

        // 未生成（ファイルが無い）。
        assert!(
            load_summary(&dir, crate::dataless::Fetch::Allowed)
                .text
                .is_none()
        );

        let path = dir.join(SUMMARY_FILENAME);
        std::fs::write(&path, "# 議事概要\n\n本文\n").expect("the summary should be writable");
        assert_eq!(
            load_summary(&dir, crate::dataless::Fetch::Allowed)
                .text
                .as_deref(),
            Some("# 議事概要\n\n本文\n"),
            "the file content is returned as-is (rendering is the UI's job)"
        );

        // 空・空白だけは「無い」と同じ扱い（生成が中途で終わった場合）。
        std::fs::write(&path, "   \n\n").expect("the summary should be writable");
        assert!(
            load_summary(&dir, crate::dataless::Fetch::Allowed)
                .text
                .is_none()
        );

        // UTF-8 でない（別物へ置換された）ファイルは読めないものとして縮退する。
        std::fs::write(&path, [0xff, 0xfe, 0x00]).expect("the summary should be writable");
        assert!(
            load_summary(&dir, crate::dataless::Fetch::Allowed)
                .text
                .is_none()
        );

        // 上限の境界（オフバイワン）は小さな上限で両側を見る。ちょうど上限は読め、
        // 1 バイト超えると読まない。
        std::fs::write(&path, "abcd").expect("the summary should be writable");
        assert_eq!(
            load_summary_limited(&dir, 4, crate::dataless::Fetch::Allowed)
                .text
                .as_deref(),
            Some("abcd")
        );
        assert!(
            load_summary_limited(&dir, 3, crate::dataless::Fetch::Allowed)
                .text
                .is_none()
        );

        // 公開入口が `MAX_SUMMARY_BYTES` を渡していること（結線）も見る。
        let too_large = "a".repeat(MAX_SUMMARY_BYTES as usize + 1);
        std::fs::write(&path, &too_large).expect("the summary should be writable");
        assert!(
            load_summary(&dir, crate::dataless::Fetch::Allowed)
                .text
                .is_none()
        );

        // ディレクトリ（非通常ファイル）に置き換えられていても落ちない（macOS では読み取り
        // 自体も失敗するので、`is_file()` ガードが無くても同じ結果になる。ここで見るのは
        // 「落ちない」ことまで）。
        std::fs::remove_file(&path).expect("the summary should be removable");
        std::fs::create_dir(&path).expect("the fixture directory should be creatable");
        assert!(
            load_summary(&dir, crate::dataless::Fetch::Allowed)
                .text
                .is_none()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// キュー待ち → 取り消しの契約を、推論スロットでワーカーを止めて決定的に見る。
    ///
    /// 先行ジョブにスロットを取らせて止めておくと、後続は**確実にキュー待ちのまま**になる
    /// （生成の速さに依存しない）。この間だけ `cancel` が効くこと、取り消したジョブは取り出されても
    /// 走らないこと、**取り消し → 積み直し → 取り消し**でも 2 本とも捨てられることを見る
    /// （取り消しがセッション単位の印だと 2 本目が走ってしまう）。
    ///
    /// 「捨てられた」ことは**番兵ジョブ**で確かめる: 取り消したジョブより後に積んだ番兵が
    /// 終端状態へ達していれば、取り消したジョブは既に取り出されている（＝まだ取り出されて
    /// いないから状態が無い、という偽の成功を排除する）。
    #[test]
    fn cancel_drops_queued_jobs_even_after_resubmitting() {
        let slot = crate::inference_slot::InferenceSlot::new();
        let worker =
            SummarizeWorker::start(crate::model_download::ModelDownloader::new(), slot.clone());
        let root =
            std::env::temp_dir().join(format!("shoki-summary-cancel-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = |name: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
            // 文字起こしが在るセッションにする（無いと Skipped になり状態が消える）。
            std::fs::write(
                dir.join("mic.json"),
                r#"{"segments":[{"start":0.0,"end":1.0,"text":"hello"}]}"#,
            )
            .expect("the transcript should be writable");
            // 実在するが GGUF ではないファイル（`resolve_model` は通り、ロードで失敗する）。
            std::fs::write(dir.join("not-a-model.gguf"), b"not a gguf")
                .expect("the fake model should be writable");
            dir
        };
        // スロットまで到達させたいジョブだけ実在の偽 GGUF を渡す（ロードで失敗する）。
        // それ以外は存在しないパスにして、`resolve_model` の時点で即 Failed にする
        // （llama.cpp に触れないぶん速く、環境差にも強い）。
        let job = |dir: &std::path::Path, reach_slot: bool| SummarizeJob {
            session_dir: dir.to_path_buf(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(dir.join(if reach_slot {
                "not-a-model.gguf"
            } else {
                "missing.gguf"
            })),
            language: "en".to_owned(),
            existing_is_stale: false,
        };
        // 上限つきポーリング（`docs/rules/testing.md`）。生成の失敗は llama.cpp のモデル
        // ロードで起きるので、環境差を見込んで 6 秒待つ。
        let wait_for_failure = |dir: &std::path::Path| {
            for _ in 0..600 {
                if worker.status_of(dir) == Some(SummarizeStatus::Failed) {
                    return true;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            false
        };

        let running = session("running");
        let cancelled = session("cancelled");
        let sentinel = session("sentinel");

        // 重い区間の実行権を握って、先行ジョブをスロット待ちで止める。
        let held = slot.acquire();
        worker.submit(job(&running, true));
        let mut started = false;
        for _ in 0..600 {
            if worker.status_of(&running) == Some(SummarizeStatus::Summarizing) {
                started = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            started,
            "the first job should reach the slot within 6s (it blocks there)"
        );

        // 後続はワーカーが塞がっているのでキュー待ちのまま。
        worker.submit(job(&cancelled, false));
        assert_eq!(worker.status_of(&cancelled), Some(SummarizeStatus::Queued));

        // 走っているジョブは取り消せない（止める口が無い）。
        assert!(!worker.cancel(&running));
        assert_eq!(
            worker.status_of(&running),
            Some(SummarizeStatus::Summarizing)
        );

        // キュー待ちは取り消せ、表示はファイルの有無ベース（記録なし）へ戻る。
        assert!(worker.cancel(&cancelled));
        assert_eq!(worker.status_of(&cancelled), None);

        // 取り消し → 積み直し → 取り消し。2 本ともキューに残るが、どちらも捨てられること。
        worker.submit(job(&cancelled, false));
        assert_eq!(worker.status_of(&cancelled), Some(SummarizeStatus::Queued));
        assert!(worker.cancel(&cancelled));
        assert_eq!(worker.status_of(&cancelled), None);

        // 番兵はこれらより後に積む（終端に達したら、前のジョブは取り出し済み）。
        worker.submit(job(&sentinel, false));

        drop(held);
        assert!(
            wait_for_failure(&running),
            "the first job should fail within 6s (the model file is not a gguf)"
        );
        assert!(
            wait_for_failure(&sentinel),
            "the sentinel job should fail within 6s (the model file is not a gguf)"
        );
        assert_eq!(
            worker.status_of(&cancelled),
            None,
            "both cancelled jobs must be dropped even though they were resubmitted"
        );
        assert!(
            !cancelled.join(SUMMARY_FILENAME).exists(),
            "a cancelled job must not produce a summary"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 生成中のセッションへ積み直したときの契約を、推論スロットで止めて決定的に見る。
    ///
    /// (1) 表示を「キュー待ち」へ下げないこと（下げると走行中のセッションで Delete と Cancel が
    /// 開いてしまう）、(2) それでも後続はちゃんと走ること、を見る。追い越された先行ジョブの
    /// 後始末は、外から観測できる隙間が無い（後続の取り出しは同じスレッドの次の一手）ので、
    /// `demote_superseded` の単体テストと
    /// `a_forgotten_session_is_not_resurrected_by_the_job_that_was_running` で見る。
    #[test]
    fn resubmitting_while_running_keeps_the_session_marked_as_running() {
        let slot = crate::inference_slot::InferenceSlot::new();
        let worker =
            SummarizeWorker::start(crate::model_download::ModelDownloader::new(), slot.clone());
        let dir =
            std::env::temp_dir().join(format!("shoki-summary-resubmit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
        std::fs::write(
            dir.join("mic.json"),
            r#"{"segments":[{"start":0.0,"end":1.0,"text":"hello"}]}"#,
        )
        .expect("the transcript should be writable");
        // 1 本目はスロットまで到達させたいので実在の偽 GGUF、2 本目は即 Failed にする。
        std::fs::write(dir.join("not-a-model.gguf"), b"not a gguf")
            .expect("the fake model should be writable");
        let job = |name: &str| SummarizeJob {
            session_dir: dir.clone(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(dir.join(name)),
            language: "en".to_owned(),
            existing_is_stale: false,
        };

        let held = slot.acquire();
        worker.submit(job("not-a-model.gguf"));
        let mut started = false;
        for _ in 0..600 {
            if worker.status_of(&dir) == Some(SummarizeStatus::Summarizing) {
                started = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            started,
            "the first job should reach the slot within 6s (it blocks there)"
        );

        // 走っている最中の積み直しでは表示を下げない（Delete / Cancel を開けない）。
        worker.submit(job("missing.gguf"));
        assert_eq!(
            worker.status_of(&dir),
            Some(SummarizeStatus::Summarizing),
            "resubmitting must not downgrade a running session to Queued"
        );
        assert!(
            !worker.cancel(&dir),
            "a running session must not be cancellable"
        );

        drop(held);
        // 2 本目が走り切るまで待つ（1 本目の完了は通番が古いので書き戻されない）。
        let mut finished = false;
        for _ in 0..600 {
            if worker.status_of(&dir) == Some(SummarizeStatus::Failed) {
                finished = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            finished,
            "the resubmitted job should run and reach a terminal state within 6s"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 削除（`forget`）したセッションを、走っていたジョブの完了で復活させないこと。
    ///
    /// 復活すると、一覧に無いセッションの記録が残り続け、同じ日時のセッションが再び現れたときに
    /// 他人の状態を表示してしまう。ここは書き戻しの通番照合が守っている唯一の外向き経路なので、
    /// スロットで走行中を作って決定的に見る。
    #[test]
    fn a_forgotten_session_is_not_resurrected_by_the_job_that_was_running() {
        let slot = crate::inference_slot::InferenceSlot::new();
        let worker =
            SummarizeWorker::start(crate::model_download::ModelDownloader::new(), slot.clone());
        let root =
            std::env::temp_dir().join(format!("shoki-summary-forget-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = |name: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
            std::fs::write(
                dir.join("mic.json"),
                r#"{"segments":[{"start":0.0,"end":1.0,"text":"hello"}]}"#,
            )
            .expect("the transcript should be writable");
            std::fs::write(dir.join("not-a-model.gguf"), b"not a gguf")
                .expect("the fake model should be writable");
            dir
        };
        let job = |dir: &std::path::Path, reach_slot: bool| SummarizeJob {
            session_dir: dir.to_path_buf(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(dir.join(if reach_slot {
                "not-a-model.gguf"
            } else {
                "missing.gguf"
            })),
            language: "en".to_owned(),
            existing_is_stale: false,
        };

        let deleted = session("deleted");
        let sentinel = session("sentinel");

        let held = slot.acquire();
        worker.submit(job(&deleted, true));
        let mut started = false;
        for _ in 0..600 {
            if worker.status_of(&deleted) == Some(SummarizeStatus::Summarizing) {
                started = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            started,
            "the job should reach the slot within 6s (it blocks there)"
        );

        // セッション削除の掃除。走っているジョブは止められないので、完了は後から届く。
        worker.forget(&deleted);
        assert_eq!(worker.status_of(&deleted), None);

        // 番兵は削除済みセッションのジョブより後に積む（終端に達したら書き戻しは済んでいる）。
        worker.submit(job(&sentinel, false));
        drop(held);
        let mut sentinel_done = false;
        for _ in 0..600 {
            if worker.status_of(&sentinel) == Some(SummarizeStatus::Failed) {
                sentinel_done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            sentinel_done,
            "the sentinel job should fail within 6s (the model file is missing)"
        );
        assert_eq!(
            worker.status_of(&deleted),
            None,
            "a forgotten session must not come back from the finished job"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 追い越されたジョブが終わったとき、セッションが「キュー待ち」で残ること（呼び出し口）。
    ///
    /// 本物の後続を積むと、先行の書き戻しの直後に**同じスレッド**が後続を取り出すので、外から
    /// 覗ける隙間が無い。ここでは後続の投入だけをキュー状態に直接書いて（チャネルには流さない）
    /// 追い越された状況を固定し、書き戻し後の表示を見る。
    #[test]
    fn a_superseded_job_leaves_the_session_queued() {
        let slot = crate::inference_slot::InferenceSlot::new();
        let worker =
            SummarizeWorker::start(crate::model_download::ModelDownloader::new(), slot.clone());
        let root =
            std::env::temp_dir().join(format!("shoki-summary-superseded-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let session = |name: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
            std::fs::write(
                dir.join("mic.json"),
                r#"{"segments":[{"start":0.0,"end":1.0,"text":"hello"}]}"#,
            )
            .expect("the transcript should be writable");
            std::fs::write(dir.join("not-a-model.gguf"), b"not a gguf")
                .expect("the fake model should be writable");
            dir
        };
        let job = |dir: &std::path::Path, reach_slot: bool| SummarizeJob {
            session_dir: dir.to_path_buf(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(dir.join(if reach_slot {
                "not-a-model.gguf"
            } else {
                "missing.gguf"
            })),
            language: "en".to_owned(),
            existing_is_stale: false,
        };

        let superseded = session("superseded");
        let sentinel = session("sentinel");

        let held = slot.acquire();
        worker.submit(job(&superseded, true));
        let mut started = false;
        for _ in 0..600 {
            if worker.status_of(&superseded) == Some(SummarizeStatus::Summarizing) {
                started = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            started,
            "the job should reach the slot within 6s (it blocks there)"
        );

        // 「生成中に積み直された」状態を作る（`submit` が走行中に行うのと同じ書き込み）。
        // チャネルには流さないので、この通番のジョブは永遠に取り出されず、書き戻し後の
        // 表示が固定される。
        lock_queue(&worker.queue).status.insert(
            superseded.clone(),
            (u64::MAX, test_entry(SummarizeStatus::Summarizing)),
        );

        worker.submit(job(&sentinel, false));
        drop(held);
        let mut sentinel_done = false;
        for _ in 0..600 {
            if worker.status_of(&sentinel) == Some(SummarizeStatus::Failed) {
                sentinel_done = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            sentinel_done,
            "the sentinel job should fail within 6s (the model file is missing)"
        );
        assert_eq!(
            worker.status_of(&superseded),
            Some(SummarizeStatus::Queued),
            "a superseded job must hand the session back as queued, not leave it running"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 追い越されたジョブの後始末（`demote_superseded`）。走っていた表示だけを戻し、
    /// 取り消し済み・終わったセッションには触らない。
    #[test]
    fn demote_superseded_only_touches_a_session_that_still_shows_running() {
        let dir = std::path::PathBuf::from("/tmp/shoki-demote");
        let state = |status: Option<SummarizeStatus>| {
            let mut queue = QueueState {
                status: HashMap::new(),
                next_seq: 0,
            };
            if let Some(status) = status {
                queue.status.insert(dir.clone(), (7, test_entry(status)));
            }
            queue
        };

        // 走行中の表示 → 実態（後続がキュー待ち）へ戻す。あわせて開始時刻も落とす
        // （走っていないものに経過を出さない）。
        let mut queue = state(Some(SummarizeStatus::Summarizing));
        demote_superseded(&mut queue, &dir);
        let demoted = queue
            .status
            .get(&dir)
            .map(|(_, entry)| entry.clone())
            .expect("the entry should stay");
        // **変種ごと差し替わる**ので、開始時刻を持つ場所そのものが無くなる（#159）。
        assert!(matches!(demoted, SummarizeEntry::Queued));

        // 取り消し・削除でエントリが消えていたら復活させない。
        let mut queue = state(None);
        demote_superseded(&mut queue, &dir);
        assert!(!queue.status.contains_key(&dir));

        // 終わった表示は上書きしない（後続が完了を書いた後に届いた先行ジョブ）。
        for status in [
            SummarizeStatus::Queued,
            SummarizeStatus::Done,
            SummarizeStatus::Failed,
        ] {
            let mut queue = state(Some(status));
            demote_superseded(&mut queue, &dir);
            assert_eq!(
                queue.status.get(&dir).map(|(_, entry)| entry.status()),
                Some(status),
                "{status:?} must be left alone"
            );
        }
    }

    /// 削除ガードが読む述語（`has_pending_jobs`）が数える状態を、**全バリアント**で固定する。
    /// キュー待ちも数える: 数えないと、要約が積まれているのに 4.4GB の LLM を消せてしまう
    /// （消してもジョブは失敗せず再取得するだけだが、待たせるだけで誰の得にもならない）。
    #[test]
    fn counts_as_pending_covers_all_states() {
        assert!(counts_as_pending(SummarizeStatus::Queued));
        assert!(counts_as_pending(SummarizeStatus::Summarizing));
        assert!(!counts_as_pending(SummarizeStatus::Done));
        assert!(!counts_as_pending(SummarizeStatus::Failed));
    }

    /// キュー待ちの順番は**読み出しのたびに数え直す**（前が終われば繰り上がる）。走っている
    /// ジョブは数えない——数えると「1 番目なのに始まらない」になる。
    #[test]
    fn queued_position_counts_only_the_jobs_still_waiting() {
        let mut queue = QueueState {
            status: HashMap::new(),
            next_seq: 0,
        };
        let mut put = |name: &str, seq: u64, status| {
            queue
                .status
                .insert(std::path::PathBuf::from(name), (seq, test_entry(status)));
        };
        put("/tmp/a", 1, SummarizeStatus::Summarizing);
        put("/tmp/b", 2, SummarizeStatus::Queued);
        put("/tmp/c", 3, SummarizeStatus::Queued);
        put("/tmp/d", 4, SummarizeStatus::Done);

        assert_eq!(queue.queued_position(2), 1, "走っている分は数えない");
        assert_eq!(queue.queued_position(3), 2);

        // 前のキュー待ちが取り消されたら繰り上がる。
        queue.status.remove(std::path::Path::new("/tmp/b"));
        assert_eq!(queue.queued_position(3), 1);
    }

    /// ワーカー越しでも同じ判定が効くこと（状態マップを直接組んで、ジョブを走らせずに見る）。
    #[test]
    fn has_pending_jobs_reads_the_status_map() {
        let worker = SummarizeWorker::start(
            crate::model_download::ModelDownloader::new(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = std::path::PathBuf::from("/tmp/shoki-pending");
        assert!(!worker.has_pending_jobs(), "an empty queue is not pending");

        for status in [SummarizeStatus::Queued, SummarizeStatus::Summarizing] {
            lock_queue(&worker.queue)
                .status
                .insert(dir.clone(), (1, test_entry(status)));
            assert!(
                worker.has_pending_jobs(),
                "{status:?} must count as a pending job"
            );
        }
        // 終わったジョブは数えない（消してよい）。
        for status in [SummarizeStatus::Done, SummarizeStatus::Failed] {
            lock_queue(&worker.queue)
                .status
                .insert(dir.clone(), (1, test_entry(status)));
            assert!(
                !worker.has_pending_jobs(),
                "{status:?} must not count as a pending job"
            );
        }
    }

    /// 状態マップのライフサイクルを、モデル無しで検証する。存在しないモデル上書きパスを渡すと、
    /// ネットワークにもモデルにも触れず即 Failed になる。
    #[test]
    fn submit_tracks_status_until_failure() {
        let worker = SummarizeWorker::start(
            crate::model_download::ModelDownloader::new(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = std::env::temp_dir().join(format!("shoki-summary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
        // 文字起こしが在るセッションにする（無いと Skipped になり状態が消える）。
        std::fs::write(
            dir.join("mic.json"),
            r#"{"segments":[{"start":0.0,"end":1.0,"text":"hello"}]}"#,
        )
        .expect("the transcript should be writable");

        worker.submit(SummarizeJob {
            session_dir: dir.clone(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(dir.join("missing-model.gguf")),
            language: "en".to_owned(),
            existing_is_stale: true,
        });
        // 投入直後は「キュー待ち」。ワーカーが取り出せば生成中、その後 Failed へ進むので、
        // どの段階を観測してもよいよう 3 つを許す（#133 でキュー待ちを分けた）。
        assert!(matches!(
            worker.status_of(&dir),
            Some(SummarizeStatus::Queued)
                | Some(SummarizeStatus::Summarizing)
                | Some(SummarizeStatus::Failed)
        ));
        // 最終的に Failed へ収束する。無限ポーリングにしない（`docs/rules/error-handling.md`）。
        let mut settled = false;
        for _ in 0..200 {
            if worker.status_of(&dir) == Some(SummarizeStatus::Failed) {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            settled,
            "the job should fail within 2s (missing model file)"
        );
        // 失敗時に空の summary.md を置き残さない。
        assert!(!dir.join(SUMMARY_FILENAME).exists());

        worker.forget(&dir);
        assert!(worker.status_of(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文字起こしが無いセッションは、モデルに触れずスキップされる（状態も残さない）。
    #[test]
    fn a_session_without_a_transcript_is_skipped() {
        let worker = SummarizeWorker::start(
            crate::model_download::ModelDownloader::new(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = std::env::temp_dir().join(format!("shoki-summary-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
        // 空のセグメント列（文字起こしはあるが発話が無い）。
        std::fs::write(dir.join("mic.json"), r#"{"segments":[]}"#)
            .expect("the transcript should be writable");

        worker.submit(SummarizeJob {
            session_dir: dir.clone(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            // 上書きパスを与えない = カタログのモデルを取りに行く経路。スキップ判定が先に
            // 効くので、ここでダウンロードが始まらないことがこのテストの要点。
            model_override: None,
            language: "en".to_owned(),
            existing_is_stale: true,
        });
        let mut settled = false;
        for _ in 0..200 {
            if worker.status_of(&dir).is_none() {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(settled, "the job should be skipped and leave no status");
        assert!(!dir.join(SUMMARY_FILENAME).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 実モデルでの通しスモーク（要ダウンロード済み GGUF・7B で数分）。ローカルで
    /// `SHOKI_SUMMARY_MODEL=<path.gguf> cargo test --release generates_minutes -- --ignored --nocapture`
    /// により実行する。llama.cpp を通る経路（プロンプト・prefill 分割・生成・保存）は単体テスト
    /// では踏めないので、確認したいときはこれを使う。
    ///
    /// 入力は #78 と同じ架空のサンプル（実会議データを使わずに再現できる）。見出しの有無だけを
    /// 見る（内容の良し悪しは #78 の検証結果が正で、ここでは判定しない）。
    ///
    /// `SHOKI_SUMMARY_REPEAT=<n>` でサンプルを n 回つないだ長い会議にできる。`n` を 6 以上に
    /// すれば（サンプルは 1 本あたり約 717 トークン相当）チャンク閾値を超え、
    /// **map-reduce の経路**（単体テストでは踏めない）を通せる。
    #[test]
    #[ignore = "needs a downloaded GGUF and several minutes; run manually with --ignored"]
    fn generates_minutes_from_the_sample_transcript() {
        let Ok(model) = std::env::var("SHOKI_SUMMARY_MODEL") else {
            panic!("set SHOKI_SUMMARY_MODEL to a .gguf path to run this test");
        };
        let repeats: usize = std::env::var("SHOKI_SUMMARY_REPEAT")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(1);
        let dir = std::env::temp_dir().join(format!("shoki-summary-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
        // 話者はファイル名で決まる（`transcript.rs`）ので、サンプルを Mic / System へ振り分けて
        // 実際の保存形式と同じ 2 ファイルにする。1 ファイルに寄せると全発話が Mic になり、
        // #78 の検証と違う入力を測ってしまう。
        let sample = include_str!("../assets/samples/meeting-ja.txt");
        for (speaker, name) in [("Mic", "mic.json"), ("System", "system.json")] {
            std::fs::write(
                dir.join(name),
                sample_transcript_json(sample, speaker, repeats),
            )
            .expect("the transcript should be writable");
        }

        let worker = SummarizeWorker::start(
            crate::model_download::ModelDownloader::new(),
            crate::inference_slot::InferenceSlot::new(),
        );
        worker.submit(SummarizeJob {
            session_dir: dir.clone(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(PathBuf::from(model)),
            language: "ja".to_owned(),
            existing_is_stale: true,
        });
        // 7B・4 分の会議で 1 分弱（コールドのロードを含めても数分）。上限つきで待つ。
        let mut status = None;
        for _ in 0..600 {
            status = worker.status_of(&dir);
            if matches!(
                status,
                Some(SummarizeStatus::Done) | Some(SummarizeStatus::Failed) | None
            ) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_secs(1));
        }
        assert_eq!(status, Some(SummarizeStatus::Done), "the job should finish");

        let markdown =
            std::fs::read_to_string(dir.join(SUMMARY_FILENAME)).expect("summary.md should exist");
        println!("{markdown}");
        for heading in [
            "## 議事概要",
            "## 議題内容",
            "## 決定事項",
            "## アクションアイテム",
        ] {
            assert!(markdown.contains(heading), "missing {heading}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `[mm:ss] Speaker: text` 形式のサンプルから、指定した話者の発話だけを取り出して
    /// `transcribe.rs` が書く JSON にする（スモークの入力を実際の保存形式と同じ経路で
    /// 読ませるため）。`repeats` 回つないだ長い会議にもできる（各回を 5 分ずつ後ろへずらす）。
    fn sample_transcript_json(sample: &str, speaker: &str, repeats: usize) -> String {
        /// サンプル 1 本ぶんの長さ（秒）。つなぐときに時刻が巻き戻らないよう、実尺より長く取る。
        const SAMPLE_SPAN_SECS: f64 = 300.0;

        let mut segments: Vec<String> = Vec::new();
        for round in 0..repeats {
            let offset = round as f64 * SAMPLE_SPAN_SECS;
            segments.extend(sample.lines().filter_map(|line| {
                let (stamp, rest) = line.strip_prefix('[')?.split_once("] ")?;
                let (minutes, seconds) = stamp.split_once(':')?;
                let start: f64 =
                    minutes.parse::<f64>().ok()? * 60.0 + seconds.parse::<f64>().ok()? + offset;
                let text = rest.strip_prefix(speaker)?.strip_prefix(": ")?;
                Some(format!(
                    "{{\"start\":{start},\"end\":{start},\"text\":{}}}",
                    serde_json::to_string(text).ok()?
                ))
            }));
        }
        format!("{{\"segments\":[{}]}}", segments.join(","))
    }

    /// 再生成のとき、古い議事録が残ったままにならないこと。失敗の記録はメモリのみ（再起動で
    /// 消える）なので、古いファイルが残ると表示側が「新しい文字起こしの議事録」として読む。
    #[test]
    fn a_failed_run_removes_the_previous_summary_only_when_it_is_stale() {
        let worker = SummarizeWorker::start(
            crate::model_download::ModelDownloader::new(),
            crate::inference_slot::InferenceSlot::new(),
        );
        let dir = std::env::temp_dir().join(format!("shoki-summary-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp session dir should be creatable");
        std::fs::write(
            dir.join("mic.json"),
            r#"{"segments":[{"start":0.0,"end":1.0,"text":"hello"}]}"#,
        )
        .expect("the transcript should be writable");
        std::fs::write(dir.join(SUMMARY_FILENAME), "# stale minutes\n")
            .expect("the stale summary should be writable");

        // モデルの上書きパスは実在するファイルにする（存在しないと `resolve_model` が先に
        // 失敗し、削除まで到達しない＝この経路の検証にならない）。GGUF ではないのでロードは
        // 失敗し、ジョブは Failed へ落ちる。
        let fake_model = dir.join("not-a-model.gguf");
        std::fs::write(&fake_model, b"not a gguf").expect("the fake model should be writable");

        worker.submit(SummarizeJob {
            session_dir: dir.clone(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(fake_model),
            language: "en".to_owned(),
            existing_is_stale: true,
        });
        let mut settled = false;
        for _ in 0..600 {
            if worker.status_of(&dir) == Some(SummarizeStatus::Failed) {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            settled,
            "the job should fail within 6s (the model file is not a gguf)"
        );
        assert!(
            !dir.join(SUMMARY_FILENAME).exists(),
            "the stale summary must not survive a failed regeneration"
        );

        // 手動の再生成（`existing_is_stale: false`）では、失敗しても既存の議事録を失わせない
        // （現在の文字起こしと整合した有効なデータなので）。同じフィクスチャで対を見る。
        std::fs::write(dir.join(SUMMARY_FILENAME), "# valid minutes\n")
            .expect("the existing summary should be writable");
        worker.submit(SummarizeJob {
            session_dir: dir.clone(),
            engine: SummaryEngine::OnDevice,
            model_id: crate::summary_model::DEFAULT_MODEL_ID.to_owned(),
            model_override: Some(dir.join("not-a-model.gguf")),
            language: "en".to_owned(),
            existing_is_stale: false,
        });
        // `submit` が同じスレッドで同期的に `Summarizing` を記録するので、直前のジョブの
        // `Failed` を拾ってしまう競合は無い（状態だけを待てる）。
        let mut settled = false;
        for _ in 0..600 {
            if worker.status_of(&dir) == Some(SummarizeStatus::Failed) {
                settled = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            settled,
            "the manual job should fail within 6s (the model file is not a gguf)"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join(SUMMARY_FILENAME)).expect("the summary should remain"),
            "# valid minutes\n",
            "a failed manual regeneration must keep the existing summary"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_summary_creates_an_owner_only_file_with_a_trailing_newline() {
        let dir = std::env::temp_dir().join(format!("shoki-summary-w-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp dir should be creatable");
        let path = dir.join(SUMMARY_FILENAME);
        write_summary(&path, "## Summary\nAll good.").expect("writing should succeed");
        assert_eq!(
            std::fs::read_to_string(&path).expect("readable"),
            "## Summary\nAll good.\n"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |path: &Path| {
                std::fs::metadata(path)
                    .expect("metadata")
                    .permissions()
                    .mode()
                    & 0o777
            };
            assert_eq!(mode(&path), 0o600, "the summary must be owner-only");

            // 緩いモードの `summary.md` が在る状態で置き換えても 0600 になること。書き込み先は
            // 常に新規の一時ファイルで、rename が inode ごと差し替えるので宛先の旧モードは
            // 残らない（既存ファイルのモードを締め直す経路は `crate::private_file` 側のテスト）。
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
                .expect("the fixture mode should be settable");
            write_summary(&path, "## Summary\nStill good.").expect("overwriting should succeed");
            assert_eq!(mode(&path), 0o600, "overwriting must tighten the mode");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
