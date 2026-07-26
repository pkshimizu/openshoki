//! 議事録要約のオンデバイス LLM を確かめる検証プローブ（#78）。**検証専用**で、出荷バイナリには
//! 入らない。本実装（`src/summarize.rs`・ワーカー・UI）は #80 で行う。
//!
//! 確かめたいのは 4 点（プラン `docs/plans/done/20260722-meeting-minutes-summary.md` の
//! ステップ 1）:
//!
//! 1. `llama-cpp-2` が `whisper-rs` と**同居できるか**（どちらも ggml を同梱するため、
//!    シンボル衝突でリンクできない可能性がある）。このプローブは両方を実際に呼ぶ。
//! 2. 候補モデルで、日本語・英語のトランスクリプトから**使える議事録が出るか**
//!    （見出し構造の安定・幻覚・言語の取り違え）。
//! 3. **速度とピークメモリ**が常駐アプリの後処理として許容範囲か。
//! 4. 長い会議で要る**チャンク分割**の目安（プロンプトが何トークンになるか）。
//!
//! ```sh
//! cargo run --release --example summary_probe -- --model <path.gguf> --lang ja
//! cargo run --release --example summary_probe -- --model <path.gguf> --lang en --transcript <file>
//! ```
//!
//! `--transcript` は「`[mm:ss] Mic: text`」形式の 1 行 1 発話。省略すると内蔵のサンプルを使う
//! （実際の会議データを使わずに再現できるようにするため）。

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("summary_probe only runs on macOS.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    if let Err(err) = probe::run() {
        eprintln!("summary_probe failed: {err}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod probe {
    use std::num::NonZeroU32;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use llama_cpp_2::context::params::LlamaContextParams;
    use llama_cpp_2::llama_backend::LlamaBackend;
    use llama_cpp_2::llama_batch::LlamaBatch;
    use llama_cpp_2::model::params::LlamaModelParams;
    use llama_cpp_2::model::{AddBos, LlamaChatMessage, LlamaModel};
    use llama_cpp_2::sampling::LlamaSampler;

    /// 生成の上限トークン数。議事録 1 本ぶんとしては十分で、暴走しても待たされない長さ。
    const MAX_TOKENS: i32 = 1_200;

    /// コンテキスト長の既定。プロンプト（トランスクリプト）＋生成が収まる必要がある。
    const DEFAULT_CTX: u32 = 8_192;

    /// prefill を投入する 1 回ぶんのトークン数。llama.cpp の `n_batch`（既定 2048）を超える
    /// バッチを `decode` に渡すと**アサートで abort する**（プロセスごと落ちる）ので、
    /// 長いトランスクリプトは必ず分割して投入する。余裕を見て既定 `n_ubatch` に合わせる。
    const PREFILL_BATCH: usize = 512;

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.iter().any(|a| a == "--help" || a == "-h") {
            print_usage();
            return Ok(());
        }
        let model_path =
            PathBuf::from(value_of(&args, "--model").ok_or("--model <path to .gguf> is required")?);
        let lang = value_of(&args, "--lang").unwrap_or_else(|| "ja".to_owned());
        let ctx_size: u32 = value_of(&args, "--ctx")
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CTX);
        let transcript = match value_of(&args, "--transcript") {
            Some(path) => std::fs::read_to_string(path)?,
            None => sample_transcript(&lang).to_owned(),
        };

        check_whisper_coexistence();

        println!("== Setup ==");
        println!("  model: {}", model_path.display());
        println!("  language: {lang}");
        println!("  context: {ctx_size}");
        println!("  transcript: {} chars", transcript.chars().count());

        let backend = LlamaBackend::init()?;
        let load_started = Instant::now();
        // GPU（Metal）へ載せられるだけ載せる。載らなければ llama.cpp が CPU へ落とす。
        let model_params = LlamaModelParams::default().with_n_gpu_layers(u32::MAX);
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)?;
        let load_elapsed = load_started.elapsed();
        println!("  load: {:.2?}", load_elapsed);
        println!("  trained context: {}", model.n_ctx_train());

        let prompt = build_prompt(&model, &lang, &transcript)?;
        let tokens = model.str_to_token(&prompt, AddBos::Always)?;
        println!("  prompt tokens: {}", tokens.len());
        if tokens.len() as u32 + MAX_TOKENS as u32 > ctx_size {
            println!(
                "  → the prompt plus the reply does not fit in {ctx_size}; chunking would be required"
            );
        }

        let mut context = model.new_context(
            &backend,
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(ctx_size)),
        )?;

        // プロンプトの評価（prefill）と生成（decode）を分けて計測する。会議が長くなると
        // 効いてくるのは prefill 側なので、まとめると判断を誤る。
        // 投入は `PREFILL_BATCH` ずつに割る（一括だと n_batch 超過で abort する）。
        let mut batch = LlamaBatch::new(PREFILL_BATCH, 1);
        let last = tokens.len() - 1;
        let prefill_started = Instant::now();
        for (chunk_index, chunk) in tokens.chunks(PREFILL_BATCH).enumerate() {
            batch.clear();
            for (i, token) in chunk.iter().enumerate() {
                let position = chunk_index * PREFILL_BATCH + i;
                // ロジットが要るのは最後の 1 トークンだけ。
                batch.add(*token, position as i32, &[0], position == last)?;
            }
            context.decode(&mut batch)?;
        }
        let prefill_elapsed = prefill_started.elapsed();

        // 議事録は事実の抽出なので、揺らぎを避けて貪欲サンプリングにする。
        let mut sampler = LlamaSampler::greedy();
        // **バイトで受けてから UTF-8 にする**。日本語はマルチバイト文字がトークン境界を
        // またぐため、トークンごとに文字列化すると壊れた分が落ちる（実際に「議事概要」が
        // 「事概要」になった）。
        let mut generated_bytes: Vec<u8> = Vec::new();
        let mut produced = 0i32;
        let mut position = tokens.len() as i32;
        let generate_started = Instant::now();
        loop {
            let token = sampler.sample(&context, -1);
            if model.is_eog_token(token) {
                break;
            }
            generated_bytes.extend_from_slice(&model.token_to_piece_bytes(token, 32, false, None)?);
            produced += 1;
            if produced >= MAX_TOKENS {
                println!("  → hit the {MAX_TOKENS}-token cap");
                break;
            }
            sampler.accept(token);
            batch.clear();
            batch.add(token, position, &[0], true)?;
            position += 1;
            context.decode(&mut batch)?;
        }
        let generate_elapsed = generate_started.elapsed();
        // 途中で打ち切った場合、末尾に不完全なバイト列が残りうるので lossy で受ける。
        let generated = String::from_utf8_lossy(&generated_bytes).into_owned();

        report(
            tokens.len(),
            produced,
            load_elapsed,
            prefill_elapsed,
            generate_elapsed,
        );
        println!();
        println!("== Generated minutes ==");
        println!("{}", generated.trim());
        Ok(())
    }

    fn print_usage() {
        println!("Usage: summary_probe --model <path.gguf> [options]");
        println!();
        println!(
            "  --lang ja|en            Language of the transcript and the output (default ja)"
        );
        println!("  --ctx <n>               Context size (default 8192)");
        println!("  --transcript <path>     Use this transcript instead of the built-in sample");
    }

    fn value_of(args: &[String], flag: &str) -> Option<String> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index + 1).cloned()
    }

    /// `whisper-rs` と `llama-cpp-2` を同じバイナリへリンクできるかを確かめる。どちらも ggml を
    /// 同梱するため、シンボルが衝突するとここまで到達できない（リンクエラーになる）。
    /// 実際にネイティブ側を呼ぶ必要があるので、存在しないモデルを開いて失敗させる。
    fn check_whisper_coexistence() {
        use whisper_rs::{WhisperContext, WhisperContextParameters};

        let result = WhisperContext::new_with_params(
            "/nonexistent/whisper-model.bin",
            WhisperContextParameters::default(),
        );
        println!("== whisper-rs / llama-cpp-2 coexistence ==");
        match result {
            // 期待どおり: whisper.cpp のコードが動いてファイルが無いと言っている。
            Err(err) => {
                println!("  whisper.cpp reachable in the same binary (expected error: {err})")
            }
            Ok(_) => println!("  whisper.cpp reachable in the same binary (unexpectedly opened)"),
        }
    }

    /// 議事録生成のプロンプト。見出しは言語ごとに固定で指定し、話者ラベルの意味と
    /// 「書かれていないことを足さない」ことを明示する。
    fn build_prompt(
        model: &LlamaModel,
        lang: &str,
        transcript: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let system = if lang == "ja" {
            "あなたは会議の書記です。文字起こしから議事録を作成します。\n\
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
             `- 担当: やること（期限）` の形で箇条書きにする。担当が分からなければ `- 未定: …`。\n\
             例: `- 田中さん: CI に設定差分のチェックを追加する（来週のスプリント）`\n\
             \n\
             守ること:\n\
             - 文字起こしに書かれていないことを推測して書かない。\n\
             - 該当が無い見出しには「なし」とだけ書く。\n\
             - 「Mic:」は書記自身（自分）の発話、「System:」は相手側の発話。"
        } else {
            "You are a meeting scribe. You write minutes from a transcript.\n\
             Reply with English Markdown only. No preamble, no closing remarks.\n\
             Use these four headings in this order, each for its own purpose.\n\
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
             Bullets shaped `- owner: what to do (when)`. Use `- Unassigned: …` if no owner.\n\
             Example: `- Tanaka: add a config diff check to CI (next sprint)`\n\
             \n\
             Rules:\n\
             - Do not add anything that is not in the transcript.\n\
             - Write \"None\" under a heading with no content.\n\
             - \"Mic:\" is the scribe speaking; \"System:\" is the other participants."
        };
        let user = if lang == "ja" {
            format!("次の文字起こしから議事録を作成してください。\n\n{transcript}")
        } else {
            format!("Write the minutes for the following transcript.\n\n{transcript}")
        };

        let template = model.chat_template(None)?;
        let chat = vec![
            LlamaChatMessage::new("system".to_owned(), system.to_owned())?,
            LlamaChatMessage::new("user".to_owned(), user)?,
        ];
        Ok(model.apply_chat_template(&template, &chat, true)?)
    }

    fn report(
        prompt_tokens: usize,
        produced: i32,
        load: Duration,
        prefill: Duration,
        generate: Duration,
    ) {
        println!();
        println!("== Measurements ==");
        println!("  load: {load:.2?}");
        println!(
            "  prefill: {prefill:.2?} ({:.1} tok/s over {prompt_tokens} prompt tokens)",
            prompt_tokens as f64 / prefill.as_secs_f64()
        );
        println!(
            "  generate: {generate:.2?} ({:.1} tok/s over {produced} tokens)",
            f64::from(produced) / generate.as_secs_f64()
        );
        println!("  total: {:.2?}", load + prefill + generate);
        match peak_rss_bytes() {
            Some(bytes) => println!(
                "  peak RSS: {:.2} GB",
                bytes as f64 / (1024.0 * 1024.0 * 1024.0)
            ),
            None => println!("  peak RSS: <unavailable>"),
        }
    }

    /// プロセスのピーク RSS（バイト）。macOS の `ru_maxrss` はバイト単位。
    fn peak_rss_bytes() -> Option<u64> {
        use std::ffi::c_int;

        /// `sys/resource.h` の `RUSAGE_SELF`。
        const RUSAGE_SELF: c_int = 0;

        /// `struct rusage` の先頭部分。必要なのは `ru_maxrss` だけだが、`getrusage` は
        /// 構造体全体ぶんの書き込み先を要求するため末尾まで確保する
        /// （`ru_utime` / `ru_stime` の 2 つの `timeval` に続いて long が 14 個）。
        #[repr(C)]
        struct Rusage {
            utime: [i64; 2],
            stime: [i64; 2],
            maxrss: i64,
            rest: [i64; 13],
        }

        unsafe extern "C" {
            fn getrusage(who: c_int, usage: *mut Rusage) -> c_int;
        }

        // SAFETY: `Rusage` は整数だけの POD なので全ゼロは有効な値。書き込み先は構造体全体。
        let mut usage: Rusage = unsafe { std::mem::zeroed() };
        // SAFETY: usage は有効な書き込み先。戻り値 0 が成功。
        let status = unsafe { getrusage(RUSAGE_SELF, &raw mut usage) };
        (status == 0).then_some(usage.maxrss as u64)
    }

    /// 内蔵のサンプル。実際の会議データを使わずに再現できるようにするためのもので、
    /// 話者の入れ替わり・決定事項・アクションアイテム・雑談を一通り含める。
    fn sample_transcript(lang: &str) -> &'static str {
        if lang == "ja" {
            include_str!("../assets/samples/meeting-ja.txt")
        } else {
            include_str!("../assets/samples/meeting-en.txt")
        }
    }
}
