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

use llama_cpp_2::context::LlamaContext;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;

use super::{
    CHUNK_TOKEN_BUDGET, MAX_REDUCE_ROUNDS, MinutesSource, estimate_tokens, group_blocks,
    minutes_system_prompt, minutes_user_prompt, notes_system_prompt, notes_user_prompt,
    split_oversized,
};

/// コンテキスト長。ピーク RSS は会議の長さではなくここで決まる（KV キャッシュを一括確保する
/// ため。#78 の実測で 7B・8192 のときピーク 8.2GB）。チャンク閾値 4,000 ＋ 定型 400 ＋ 生成
/// 1,200 に対して十分な余裕がある。
const CONTEXT_TOKENS: u32 = 8_192;

/// 議事録 1 本の生成トークン上限。暴走しても待たされない長さ。
const MAX_MINUTES_TOKENS: i32 = 1_200;

/// 中間メモ 1 本の生成トークン上限。reduce 段の入力量を抑えるため議事録より短くする。
const MAX_NOTES_TOKENS: i32 = 800;

/// prefill を 1 回に投入するトークン数。`n_batch` 超過の abort を避けるための分割で、
/// 既定の `n_ubatch` に合わせた値（上の注意 1）。
const PREFILL_BATCH: usize = 512;

/// トークンを文字列化するときの受けバッファ（バイト）。1 トークンの表記としては十分な大きさ。
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
/// （チャンクごとの中間メモ → 議事録）にする。中間メモがまだ多いときは、収まるまで
/// メモ自身を畳み直す（上限 `MAX_REDUCE_ROUNDS` 回）。
pub(super) fn generate(
    model_path: &Path,
    language: &str,
    lines: &[String],
) -> Result<String, Box<dyn std::error::Error>> {
    let backend = backend()?;
    // GPU があれば載せられるだけ載せる（現状のクレートでは無視される。モジュール doc 参照）。
    let model_params = LlamaModelParams::default().with_n_gpu_layers(u32::MAX);
    let model = LlamaModel::load_from_file(backend, model_path, &model_params)?;
    let mut context = model.new_context(
        backend,
        LlamaContextParams::default().with_n_ctx(NonZeroU32::new(CONTEXT_TOKENS)),
    )?;
    let mut batch = LlamaBatch::new(PREFILL_BATCH, 1);

    let transcript = lines.join("\n");
    if estimate_tokens(&transcript) <= CHUNK_TOKEN_BUDGET {
        return run_pass(
            &model,
            &mut context,
            &mut batch,
            &minutes_system_prompt(language),
            &minutes_user_prompt(language, MinutesSource::Transcript, &transcript),
            MAX_MINUTES_TOKENS,
        );
    }
    drop(transcript);

    // map: チャンクごとに中間メモを作る。
    let chunks = group_blocks(lines, "\n", CHUNK_TOKEN_BUDGET);
    println!(
        "Summarizing a long transcript in {} parts (this takes several minutes)",
        chunks.len()
    );
    let notes_system = notes_system_prompt(language);
    let mut notes = summarize_parts(
        &model,
        &mut context,
        &mut batch,
        language,
        &notes_system,
        &chunks,
    )?;

    // reduce の前段: メモがまだ 1 回分に収まらないなら、メモ自身をまとめ直す。
    // 1 回で件数が約 1/3 になるので通常は 0〜1 回で終わる。
    for _ in 0..MAX_REDUCE_ROUNDS {
        if notes.len() <= 1 || estimate_tokens(&notes.join("\n\n")) <= CHUNK_TOKEN_BUDGET {
            break;
        }
        let groups = group_blocks(&notes, "\n\n", CHUNK_TOKEN_BUDGET);
        notes = summarize_parts(
            &model,
            &mut context,
            &mut batch,
            language,
            &notes_system,
            &groups,
        )?;
    }

    // reduce: メモから議事録を作る。上限回数でも収まらなかった場合は先頭から入るぶんだけ使う
    // （生成そのものを失敗させるより、途中までの議事録を残すほうが役に立つ）。
    let mut body = notes.join("\n\n");
    if estimate_tokens(&body) > CHUNK_TOKEN_BUDGET {
        eprintln!("Truncating the notes because the transcript is too long to summarize in full");
        body = split_oversized(&body, CHUNK_TOKEN_BUDGET)
            .into_iter()
            .next()
            .unwrap_or_default();
    }
    run_pass(
        &model,
        &mut context,
        &mut batch,
        &minutes_system_prompt(language),
        &minutes_user_prompt(language, MinutesSource::Notes, &body),
        MAX_MINUTES_TOKENS,
    )
}

/// 各パートを順に中間メモへ落とす（map 段。畳み直しの各ラウンドでも同じものを使う）。
fn summarize_parts(
    model: &LlamaModel,
    context: &mut LlamaContext<'_>,
    batch: &mut LlamaBatch,
    language: &str,
    notes_system: &str,
    parts: &[String],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    let total = parts.len();
    let mut notes = Vec::with_capacity(total);
    for (index, part) in parts.iter().enumerate() {
        let note = run_pass(
            model,
            context,
            batch,
            notes_system,
            &notes_user_prompt(language, index + 1, total, part),
            MAX_NOTES_TOKENS,
        )?;
        notes.push(note);
    }
    Ok(notes)
}

/// 1 回ぶんの推論（prefill → 生成）。呼ぶたびに KV キャッシュを空にするので、同じコンテキストを
/// map-reduce の各段で使い回せる（コンテキストの作り直しは KV キャッシュの再確保を伴い重い）。
fn run_pass(
    model: &LlamaModel,
    context: &mut LlamaContext<'_>,
    batch: &mut LlamaBatch,
    system: &str,
    user: &str,
    max_tokens: i32,
) -> Result<String, Box<dyn std::error::Error>> {
    context.clear_kv_cache();

    let template = model.chat_template(None)?;
    let chat = vec![
        LlamaChatMessage::new("system".to_owned(), system.to_owned())?,
        LlamaChatMessage::new("user".to_owned(), user.to_owned())?,
    ];
    let prompt = model.apply_chat_template(&template, &chat, true)?;
    let tokens = model.str_to_token(&prompt, AddBos::Always)?;

    // 走らせる前に収まるか見る（モジュール doc の注意 2）。概算が外れて超えた場合はここで
    // 止まり、ジョブは失敗として記録される（黙って壊れた出力を出さない）。
    let needed = tokens.len() as u32 + max_tokens.unsigned_abs();
    let n_ctx = context.n_ctx();
    if needed > n_ctx {
        return Err(format!(
            "the prompt ({} tokens) plus the reply ({max_tokens}) does not fit in the context ({n_ctx})",
            tokens.len()
        )
        .into());
    }
    let last = tokens
        .len()
        .checked_sub(1)
        .ok_or("the prompt tokenized to nothing")?;

    // prefill。`PREFILL_BATCH` ずつに割って投入する（モジュール doc の注意 1）。
    for (chunk_index, chunk) in tokens.chunks(PREFILL_BATCH).enumerate() {
        batch.clear();
        for (i, token) in chunk.iter().enumerate() {
            let position = chunk_index * PREFILL_BATCH + i;
            // ロジットが要るのは最後の 1 トークンだけ。
            batch.add(*token, position as i32, &[0], position == last)?;
        }
        context.decode(batch)?;
    }

    // 議事録は事実の抽出なので、揺らぎを避けて貪欲サンプリングにする。
    let mut sampler = LlamaSampler::greedy();
    // **バイトで受けてから UTF-8 にする**（モジュール doc の注意 3）。
    let mut generated: Vec<u8> = Vec::new();
    let mut produced = 0i32;
    let mut position = tokens.len() as i32;
    loop {
        let token = sampler.sample(context, -1);
        if model.is_eog_token(token) {
            break;
        }
        generated.extend_from_slice(&model.token_to_piece_bytes(
            token,
            TOKEN_PIECE_BUF,
            false,
            None,
        )?);
        produced += 1;
        if produced >= max_tokens {
            // 打ち切りは出力が尻切れになるので黙って通さない（本文はログに出さない）。
            eprintln!("Truncating the generated text because it hit the {max_tokens}-token limit");
            break;
        }
        sampler.accept(token);
        batch.clear();
        batch.add(token, position, &[0], true)?;
        position += 1;
        context.decode(batch)?;
    }
    // 途中で打ち切った場合、末尾に不完全なバイト列が残りうるので lossy で受ける。
    Ok(String::from_utf8_lossy(&generated).into_owned())
}
