//! 議事録要約のオンデバイス実行エンジン（llama.cpp / `llama-cpp-2`）。
//!
//! 外部送信は一切しない（通信はモデルの初回ダウンロード受信のみで、それも共有基盤
//! `src/model_download.rs` の担当）。長いトランスクリプトは map-reduce の 2 段要約で処理する。
//!
//! **#78 の検証で判明した、守らないと壊れる 3 点**
//! （`docs/plans/done/20260722-meeting-minutes-summary.md` の「実装上の注意」）:
//!
//! 1. プロンプトは `n_batch`（既定 2048）以下に分けて `decode` へ渡す。一括で渡すと llama.cpp が
//!    **アサートで abort し、プロセスごと落ちる**（常駐アプリなので録音中でも巻き添えになる）。
//! 2. プロンプト＋生成が `n_ctx` を超えたら走らせる前に落とす。超えたまま進むと最初の `decode` が
//!    `NoKvCacheSlot` で失敗し、原因が分かりにくい。
//! 3. 生成結果はトークンごとに文字列化せず、バイトで受けてから UTF-8 にする。日本語は
//!    マルチバイト文字がトークン境界をまたぐため、トークン単位だと欠落する。
//!
//! なお `llama-cpp-2` 0.1.152 では **Metal がリンクされず CPU 実行になる**（`Cargo.toml` の
//! 依存コメント参照）。GPU 層数の指定は残してあるが現状は黙って無視される。

use std::num::NonZeroU32;
use std::path::Path;
use std::sync::OnceLock;

use llama_cpp_2::TokenToStringError;
use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaChatTemplate, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use llama_cpp_2::token::LlamaToken;

use super::{
    CHUNK_TOKEN_BUDGET, MAX_REDUCE_ROUNDS, MinutesSource, estimate_joined, group_blocks,
    minutes_system_prompt, minutes_user_prompt, notes_system_prompt, notes_user_prompt,
    truncate_to_budget,
};

/// コンテキスト長。ピーク RSS は会議の長さではなくここで決まる（KV キャッシュを一括確保する
/// ため。#78 の実測で 7B・8192 のときピーク 8.2GB）。チャンク閾値 4,000 ＋ 定型 400 ＋ 生成
/// 1,200 に対して十分な余裕がある。
const CONTEXT_TOKENS: u32 = 8_192;

/// 議事録 1 本の生成トークン上限。暴走しても待たされない長さ。
const MAX_MINUTES_TOKENS: u32 = 1_200;

/// 中間メモ 1 本の生成トークン上限。reduce 段の入力量を抑えるため議事録より短くする
/// （畳み直しが何回で収まるかの見積もりは `super::MAX_REDUCE_ROUNDS` の doc を参照）。
const MAX_NOTES_TOKENS: u32 = 800;

/// prefill を 1 回に投入するトークン数。`n_batch` 超過の abort を避けるための分割で、
/// 既定の `n_ubatch` に合わせた値（上の注意 1）。
const PREFILL_BATCH: usize = 512;

/// トークン 1 個の表記を受ける最初のバッファ（バイト）。足りなければ必要サイズで引き直すので、
/// ここは「大半のトークンが 1 回で収まる」程度でよい。
const TOKEN_PIECE_BUF: usize = 32;

/// llama.cpp のバックエンド。**プロセスで 1 回しか初期化できない**（2 回目は
/// `BackendAlreadyInitialized` になる）ため、ここで保持して使い回す。初期化に失敗したら
/// その理由も覚えておき、以後のジョブへ同じエラーを返す（毎回の再試行で状態を壊さない）。
fn backend() -> Result<&'static LlamaBackend, String> {
    static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
    BACKEND
        .get_or_init(|| {
            // llama.cpp / GGML が stderr へ出す冗長な内部ログを止める（whisper 側の
            // `install_logging_hooks` と同じ趣旨）。初期化より先に設定する。
            llama_cpp_2::send_logs_to_tracing(
                llama_cpp_2::LogOptions::default().with_logs_enabled(false),
            );
            LlamaBackend::init().map_err(|err| err.to_string())
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// トランスクリプトの各行から議事録 Markdown を生成する。
///
/// 概算トークンがチャンク閾値に収まれば 1 段で生成し、超えるなら map-reduce
/// （チャンクごとの中間メモ → 議事録）にする。中間メモがまだ多いときは収まるまで
/// メモ自身を畳み直す（上限 `MAX_REDUCE_ROUNDS` 回）。
pub(super) fn generate(
    model_path: &Path,
    language: &str,
    lines: &[String],
) -> Result<String, Box<dyn std::error::Error>> {
    let backend = backend()?;
    // GPU があれば載せられるだけ載せる（現状のクレートでは無視される。モジュール doc 参照）。
    let model_params = LlamaModelParams::default().with_n_gpu_layers(u32::MAX);
    // 失敗しやすい段（GGUF の破損・コンテキスト確保）は、どこで落ちたかがログで分かるように
    // 文脈を足す（whisper 側がモデルロード失敗を専用メッセージにしているのと同じ粒度）。
    let model = LlamaModel::load_from_file(backend, model_path, &model_params)
        .map_err(|err| format!("loading the summary model failed: {err}"))?;
    let mut context = model
        .new_context(
            backend,
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(CONTEXT_TOKENS)),
        )
        .map_err(|err| format!("creating the summary context failed: {err}"))?;
    let mut batch = LlamaBatch::new(PREFILL_BATCH, 1);
    // チャットテンプレートはモデルを借用しない所有型なので、1 回取って全パスで使い回す。
    let template = model.chat_template(None)?;
    let mut pass = Pass {
        model: &model,
        context: &mut context,
        batch: &mut batch,
        template: &template,
    };

    // 収まるかどうかは連結せずに測る（長い会議のトランスクリプトは MB 級になりうるので、
    // 判定のためだけに全文をコピーしない）。
    if estimate_joined(lines, "\n") <= CHUNK_TOKEN_BUDGET {
        return pass.run(
            &minutes_system_prompt(language),
            &minutes_user_prompt(language, MinutesSource::Transcript, &lines.join("\n")),
            MAX_MINUTES_TOKENS,
        );
    }

    // map: チャンクごとに中間メモを作る。
    let chunks = group_blocks(lines, "\n", CHUNK_TOKEN_BUDGET);
    println!(
        "Summarizing a long transcript in {} parts (this takes several minutes)",
        chunks.len()
    );
    let notes_system = notes_system_prompt(language);
    let mut notes = pass.summarize_chunks(language, &notes_system, &chunks)?;

    // reduce の前段: メモがまだ 1 回分に収まらないなら、メモ自身をまとめ直す。
    for _ in 0..MAX_REDUCE_ROUNDS {
        if notes.len() <= 1 || estimate_joined(&notes, "\n\n") <= CHUNK_TOKEN_BUDGET {
            break;
        }
        let groups = group_blocks(&notes, "\n\n", CHUNK_TOKEN_BUDGET);
        notes = pass.summarize_chunks(language, &notes_system, &groups)?;
    }

    // reduce: メモから議事録を作る。上限回数でも収まらなかった場合は先頭から入るぶんだけ使う
    // （生成そのものを失敗させるより、途中までの議事録を残すほうが役に立つ）。
    let joined = notes.join("\n\n");
    let body = truncate_to_budget(&joined, CHUNK_TOKEN_BUDGET);
    if body.len() < joined.len() {
        eprintln!("Truncating the notes because the transcript is too long to summarize in full");
    }
    pass.run(
        &minutes_system_prompt(language),
        &minutes_user_prompt(language, MinutesSource::Notes, body),
        MAX_MINUTES_TOKENS,
    )
}

/// 1 モデル・1 コンテキストを使い回して推論を繰り返すための束。map-reduce では同じモデルで
/// 何度も生成するので、借用の組（モデル・コンテキスト・バッチ・テンプレート）をまとめて持つ。
struct Pass<'a, 'model, 'batch> {
    model: &'a LlamaModel,
    context: &'a mut LlamaContext<'model>,
    batch: &'a mut LlamaBatch<'batch>,
    template: &'a LlamaChatTemplate,
}

impl Pass<'_, '_, '_> {
    /// 各チャンクを順に中間メモへ落とす（map 段。畳み直しの各ラウンドでも同じものを使う）。
    fn summarize_chunks(
        &mut self,
        language: &str,
        notes_system: &str,
        chunks: &[String],
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let total = chunks.len();
        let mut notes = Vec::with_capacity(total);
        for (index, chunk) in chunks.iter().enumerate() {
            notes.push(self.run(
                notes_system,
                &notes_user_prompt(language, index + 1, total, chunk),
                MAX_NOTES_TOKENS,
            )?);
        }
        Ok(notes)
    }

    /// 1 回ぶんの推論（prefill → 生成）。呼ぶたびに KV キャッシュを空にするので、同じコンテキストを
    /// map-reduce の各段で使い回せる（コンテキストの作り直しは KV キャッシュの再確保を伴い重い）。
    fn run(
        &mut self,
        system: &str,
        user: &str,
        max_tokens: u32,
    ) -> Result<String, Box<dyn std::error::Error>> {
        self.context.clear_kv_cache();

        let chat = vec![
            LlamaChatMessage::new("system".to_owned(), system.to_owned())?,
            LlamaChatMessage::new("user".to_owned(), user.to_owned())?,
        ];
        let prompt = self.model.apply_chat_template(self.template, &chat, true)?;
        let tokens = self.model.str_to_token(&prompt, AddBos::Always)?;

        // 走らせる前に収まるか見る（モジュール doc の注意 2）。概算が外れて超えた場合はここで
        // 止まり、ジョブは失敗として記録される（黙って壊れた出力を出さない）。
        // 以降の `as i32` はすべてこのガードが通ったことに依存するので、ここでは飽和・切り捨ての
        // 起きない形（try_from ＋ checked_add）で判定する。
        let prompt_tokens =
            u32::try_from(tokens.len()).map_err(|_| "the prompt has too many tokens")?;
        let needed = prompt_tokens
            .checked_add(max_tokens)
            .ok_or("the prompt plus the reply overflows the token count")?;
        let n_ctx = self.context.n_ctx();
        if needed > n_ctx {
            return Err(format!(
                "the prompt ({prompt_tokens} tokens) plus the reply ({max_tokens}) does not fit in the context ({n_ctx})"
            )
            .into());
        }
        let last = tokens
            .len()
            .checked_sub(1)
            .ok_or("the prompt tokenized to nothing")?;

        // prefill。`PREFILL_BATCH` ずつに割って投入する（モジュール doc の注意 1）。
        for (chunk_index, chunk) in tokens.chunks(PREFILL_BATCH).enumerate() {
            self.batch.clear();
            for (i, token) in chunk.iter().enumerate() {
                let position = chunk_index * PREFILL_BATCH + i;
                // ロジットが要るのは最後の 1 トークンだけ。
                self.batch
                    .add(*token, position as i32, &[0], position == last)?;
            }
            self.context.decode(self.batch)?;
        }

        // 議事録は事実の抽出なので、揺らぎを避けて貪欲サンプリングにする。
        // `sample` は llama.cpp 側で accept まで済ませるので、こちらで `accept` を呼ばない
        // （呼ぶと二重になり、ペナルティ系をチェーンへ足したときに数え違える）。
        let mut sampler = LlamaSampler::greedy();
        // **バイトで受けてから UTF-8 にする**（モジュール doc の注意 3）。
        let mut generated: Vec<u8> = Vec::new();
        let mut produced = 0u32;
        let mut position = prompt_tokens as i32;
        loop {
            let token = sampler.sample(self.context, -1);
            if self.model.is_eog_token(token) {
                break;
            }
            generated.extend_from_slice(&token_bytes(self.model, token)?);
            produced += 1;
            if produced >= max_tokens {
                // 打ち切りは出力が尻切れになるので黙って通さない（本文はログに出さない）。
                eprintln!(
                    "Truncating the generated text because it hit the {max_tokens}-token limit"
                );
                break;
            }
            self.batch.clear();
            self.batch.add(token, position, &[0], true)?;
            position += 1;
            self.context.decode(self.batch)?;
        }
        // 途中で打ち切った場合、末尾に不完全なバイト列が残りうるので lossy で受ける。
        Ok(String::from_utf8_lossy(&generated).into_owned())
    }
}

/// トークン 1 個の表記をバイト列で得る。
///
/// `token_to_piece_bytes` は固定バッファなので、**2 つの「失敗ではない失敗」を吸収する**:
///
/// - `InsufficientBufferSpace(n)`: 表記が `TOKEN_PIECE_BUF` に収まらなかった。クレート自身の
///   `token_to_str` と同じく、必要サイズで引き直す。
/// - `UnknownTokenType`: `special = false` では表記が空になる制御トークン（Qwen2.5 の
///   `<|im_start|>` 等。EOG ではないので生成ループでは止まらない）。表記なしとして読み飛ばす。
///
/// どちらも `?` で伝播させると、**1 トークンのために数分〜十数分かけた要約が丸ごと失敗する**。
/// 制御トークンの文字列そのものは議事録へ出したくないので `special = true` にはしない。
fn token_bytes(model: &LlamaModel, token: LlamaToken) -> Result<Vec<u8>, TokenToStringError> {
    match model.token_to_piece_bytes(token, TOKEN_PIECE_BUF, false, None) {
        Ok(bytes) => Ok(bytes),
        Err(TokenToStringError::UnknownTokenType) => Ok(Vec::new()),
        Err(TokenToStringError::InsufficientBufferSpace(needed)) => {
            model.token_to_piece_bytes(token, needed.unsigned_abs() as usize, false, None)
        }
        Err(err) => Err(err),
    }
}
