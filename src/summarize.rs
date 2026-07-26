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

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::transcript::TranscriptSegment;

/// 生成した議事録の保存ファイル名。セッションディレクトリに固定名で置く
/// （`mic.json` / `mix.mp3` と同系統）。表示側（後続 issue）と一致させること。
pub const SUMMARY_FILENAME: &str = "summary.md";

/// 1 チャンクに入れるトランスクリプト本文のトークン概算の上限。
///
/// コンテキスト長（Qwen2.5 は 32,768）ではなく **prefill が超線形に伸びること**が理由の閾値
/// （#78: 3B でトークン 4.4 倍に対し時間 7.3 倍）。約 4,000 トークンは本文で日本語 5,800 文字
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
}

/// セッション単位の要約の進行状況。表示は後続 issue だが、ワーカーの契約
/// （`TranscribeWorker` と同型）として先に持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummarizeStatus {
    /// 投入済み（キュー待ちを含む）または生成中。
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
    tx: Option<Sender<SummarizeJob>>,
    /// セッションディレクトリ → 進行状況。
    status: Arc<Mutex<StatusMap>>,
}

/// セッションディレクトリ → 進行状況のマップ（UI スレッドとワーカースレッドで共有）。
type StatusMap = HashMap<PathBuf, SummarizeStatus>;

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
        let status: Arc<Mutex<StatusMap>> = Arc::new(Mutex::new(HashMap::new()));
        let status_for_worker = Arc::clone(&status);
        let (tx, rx) = mpsc::channel::<SummarizeJob>();
        let spawned = std::thread::Builder::new()
            .name("summarize-worker".into())
            .spawn(move || {
                // 送信側（アプリ本体）が落ちてチャネルが閉じたら自然に終了する。
                while let Ok(job) = rx.recv() {
                    // 処理開始でも「生成中」を入れ直す（先行ジョブの完了が後続の処理中表示を
                    // 上書きしたままにならないように。`TranscribeWorker` と同じ）。
                    lock_status(&status_for_worker)
                        .insert(job.session_dir.clone(), SummarizeStatus::Summarizing);
                    let outcome = run_job(&job, &downloader, &slot);
                    let mut map = lock_status(&status_for_worker);
                    match outcome {
                        // 対象なしで何もしなかった場合は「投入済み」の痕跡を消す。
                        JobOutcome::Skipped => map.remove(&job.session_dir),
                        JobOutcome::Done => map.insert(job.session_dir, SummarizeStatus::Done),
                        JobOutcome::Failed => map.insert(job.session_dir, SummarizeStatus::Failed),
                    };
                }
            });
        match spawned {
            Ok(_handle) => Self {
                tx: Some(tx),
                status,
            },
            Err(err) => {
                eprintln!(
                    "Disabling summarization because the worker thread failed to start: {err}"
                );
                Self { tx: None, status }
            }
        }
    }

    /// ジョブを投入する。投入した時点でセッションを「生成中」（キュー待ちを含む）として記録する。
    /// ワーカーが動いていない場合はログのみ（文字起こしまでは保存済み）。
    pub fn submit(&self, job: SummarizeJob) {
        let Some(tx) = &self.tx else {
            eprintln!("Skipping summarization because the summary worker is not running");
            return;
        };
        lock_status(&self.status).insert(job.session_dir.clone(), SummarizeStatus::Summarizing);
        // 送信失敗 = ワーカースレッドが（panic 等で）終了しレシーバが閉じた状態。記録した
        // 「生成中」を取り消す（永遠に進行中表示のままにしない）。
        if let Err(mpsc::SendError(job)) = tx.send(job) {
            eprintln!("Skipping summarization because the summary worker is not running");
            lock_status(&self.status).remove(&job.session_dir);
        }
    }

    /// セッションの進行状況。マップに載っていなければ `None`
    /// （表示側が `summary.md` の有無で「未生成/生成済み」を解決する）。
    ///
    /// ワーカーの契約として先に用意してあるが、**読む側（Recordings ウィンドウの状態表示・
    /// 手動再生成）は #81 のスコープ**なので、本体からの呼び出しはまだ無い（テストからは使う）。
    /// #81 で消費されるまでの一時的な `allow`。
    #[allow(dead_code)]
    pub fn status_of(&self, session_dir: &Path) -> Option<SummarizeStatus> {
        lock_status(&self.status).get(session_dir).copied()
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
    /// `summary.md` を保存した。
    Done,
    /// 生成・保存に失敗した（モデル準備の失敗を含む）。
    Failed,
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
        let segments = crate::transcript::load_transcript(&job.session_dir);
        transcript_lines(&segments)
    };
    if lines.is_empty() {
        // 文字起こしが未生成・欠落・破損・全行空。GB 級のモデルをロードしない防御でもある。
        println!("Skipping summarization because the session has no transcript");
        return JobOutcome::Skipped;
    }

    // 既にある `summary.md` は、ここへ来た時点で**古い文字起こしの議事録**（このジョブは
    // 文字起こしが成功した直後にしか走らない）。先に消しておかないと、生成に失敗したときに
    // 古い議事録が残り、失敗の記録はメモリのみで再起動すると消えるため、表示側は「新しい
    // 文字起こしの議事録」として読んでしまう。消せなかった場合は上書きに賭けて先へ進む。
    let path = job.session_dir.join(SUMMARY_FILENAME);
    if let Err(err) = std::fs::remove_file(&path)
        && err.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("Could not remove the previous meeting summary before regenerating it: {err}");
    }

    let generated = match job.engine {
        SummaryEngine::OnDevice => {
            let Some(model_path) = resolve_model(job, downloader) else {
                return JobOutcome::Failed;
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
            return JobOutcome::Failed;
        }
    };
    // 空（または空白だけ）の生成結果は失敗として扱う。空ファイルを置くと、表示側が
    // 「生成済み」と読んで白紙を出してしまう。
    let generated = generated.trim();
    if generated.is_empty() {
        eprintln!("Skipping summarization because the model produced no text");
        return JobOutcome::Failed;
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
            JobOutcome::Failed
        }
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

/// 議事録を保存する。録音・文字起こしと同じ機微データなので Unix では 0600 で作成する
/// （`docs/rules/security.md`。セッションディレクトリ自体は録音側が 0700 で作成済み）。
///
/// `OpenOptions::mode` は**新規作成時にしか効かない**ので、開いた後にモードを明示し直す
/// （セッションを `cp -r` した／バックアップから戻した等で 0644 の `summary.md` が在ると、
/// 上書きしてもそのモードが残ってしまう）。ファイルハンドル経由で設定するので、開いた後に
/// 差し替えられても別のファイルへ適用されることはない。
fn write_summary(path: &Path, markdown: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(markdown.as_bytes())?;
    // Markdown ファイルとして扱いやすいよう末尾を改行で終える。
    file.write_all(b"\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// ここから下はモデル非依存の純粋関数（単体テスト対象）。
// ---------------------------------------------------------------------------

/// マージ済みトランスクリプトを、モデルへ渡す 1 発話 1 行のテキストにする。
/// 形式は #78 の検証サンプル（`assets/samples/meeting-*.txt`）と同じ `[mm:ss] Speaker: text`
/// （1 時間を超える録音では `[h:mm:ss]`）。空の発話（whisper が無音区間に付けることがある）は落とす。
///
/// 時刻の整形は表示側と同じ `tray::format_elapsed` を使う（同じ表記の実装を 2 つ持つと、
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
/// 表示名は ComboBox 用の UI 文言なので、変えるとプロンプトの挙動が黙って変わってしまう）。
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
        });
        assert!(matches!(
            worker.status_of(&dir),
            Some(SummarizeStatus::Summarizing) | Some(SummarizeStatus::Failed)
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
            let mode = std::fs::metadata(&path)
                .expect("metadata")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the summary must be owner-only");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
