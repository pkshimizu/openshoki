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

    /// `--lang` の既定。
    const DEFAULT_LANG: &str = "ja";

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
        // 指定の取り違えを黙って既定へ落とさない。このプローブの成果はプランへ転記する
        // 計測値なので、ラベルと実際の条件がずれると記録そのものが誤りになる。
        let lang = match value_of(&args, "--lang") {
            Some(lang) if lang == "ja" || lang == "en" => lang,
            Some(lang) => return Err(format!("--lang must be ja or en (got {lang:?})").into()),
            None => DEFAULT_LANG.to_owned(),
        };
        let ctx_size: u32 = match value_of(&args, "--ctx") {
            Some(value) => value
                .parse()
                .map_err(|err| format!("--ctx needs a number ({value:?}: {err})"))?,
            None => DEFAULT_CTX,
        };
        let transcript = match value_of(&args, "--transcript") {
            Some(path) => std::fs::read_to_string(path)?,
            None => sample_transcript(&lang).to_owned(),
        };

        check_whisper_coexistence();

        section("Setup");
        println!("  model: {}", model_path.display());
        println!("  language: {lang}");
        println!("  context: {ctx_size}");
        println!("  transcript: {} chars", transcript.chars().count());

        let backend = LlamaBackend::init()?;
        report_backends();
        let load_started = Instant::now();
        // GPU があれば載せられるだけ載せる。**このクレート（0.1.152）では macOS でも Metal が
        // 登録されないため実際には CPU で動く**（`Cargo.toml` の依存コメント参照）。指定自体は
        // 残しておき、クレート側が直ったら効くようにする。
        let model_params = LlamaModelParams::default().with_n_gpu_layers(u32::MAX);
        let model = LlamaModel::load_from_file(&backend, &model_path, &model_params)?;
        let load_elapsed = load_started.elapsed();
        println!("  trained context: {}", model.n_ctx_train());

        let prompt = build_prompt(&model, &lang, &transcript)?;
        let tokens = model.str_to_token(&prompt, AddBos::Always)?;
        println!("  prompt tokens: {}", tokens.len());
        // 超過したまま進むと最初の decode が `NoKvCacheSlot` で落ち、原因が分かりにくい。
        // ここで止めて、必要な --ctx を示す。
        let needed = tokens.len() as u32 + MAX_TOKENS as u32;
        if needed > ctx_size {
            return Err(format!(
                "the prompt ({} tokens) plus the reply ({MAX_TOKENS}) needs --ctx {needed} \
                 (chunking would be required in the real implementation)",
                tokens.len()
            )
            .into());
        }

        let mut context = model.new_context(
            &backend,
            LlamaContextParams::default().with_n_ctx(NonZeroU32::new(ctx_size)),
        )?;

        // プロンプトの評価（prefill）と生成（decode）を分けて計測する。会議が長くなると
        // 効いてくるのは prefill 側なので、まとめると判断を誤る。
        // 投入は `PREFILL_BATCH` ずつに割る（一括だと n_batch 超過で abort する）。
        let mut batch = LlamaBatch::new(PREFILL_BATCH, 1);
        let last = tokens
            .len()
            .checked_sub(1)
            .ok_or("the prompt tokenized to nothing")?;
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
            ctx_size,
            load_elapsed,
            prefill_elapsed,
            generate_elapsed,
        );
        section("Generated minutes");
        println!("{}", generated.trim());
        Ok(())
    }

    /// 実行に使ったバックエンドを出力する。計測値だけを転記すると「GPU で測った」と取り違え
    /// やすいので、記録が自己記述的になるようにする（検証中に実際に取り違えた）。
    fn report_backends() {
        section("Backends");
        let devices = llama_cpp_2::list_llama_ggml_backend_devices();
        if devices.is_empty() {
            println!("  no ggml backend devices registered");
            return;
        }
        for device in &devices {
            println!(
                "  {:?}: {} ({})",
                device.device_type, device.name, device.description
            );
        }
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

    /// 節の見出し（`examples/mas_probe.rs` と同じ体裁: 前に空行を置く）。
    fn section(title: &str) {
        println!();
        println!("== {title} ==");
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
        section("whisper-rs / llama-cpp-2 coexistence");
        match result {
            // 期待どおり: whisper.cpp のコードが動いてファイルが無いと言っている。
            Err(err) => {
                println!("  whisper.cpp reachable in the same binary (expected error: {err})")
            }
            Ok(_) => println!("  whisper.cpp reachable in the same binary (unexpectedly opened)"),
        }
    }

    /// 議事録生成の system プロンプト（日本語）。
    ///
    /// **人名入りの few-shot 例は置かない**。検証中、例に書いた人名が
    /// (1) 評価サンプルと重なると「抽出できたのか写しただけか」を区別できず品質評価が汚染され、
    /// (2) 重ならない名前にすると、今度は**その名前が架空の担当者として出力に漏れた**
    /// （3B 英語は例を丸写し、7B 日本語は実在のタスクの担当に例の人名を当てた）。
    /// 形は言葉で説明し、担当は「文字起こしに出てきた人だけ」と制約する。
    ///
    /// **これはユーザー向け文言ではなくモデルへの入力データ**なので、日本語のまま置く
    /// （`docs/rules/messages.md` は GUI ラベルやログを対象にしており、ここは対象外）。
    /// 出力言語は「何語で指示するか」で決まるため、英語に直すと検証対象そのものが変わる
    /// （whisper へ渡す言語コードと同じ性質のパラメータ）。
    const SYSTEM_JA: &str = "あなたは会議の書記です。文字起こしから議事録を作成します。\n\
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

    /// 議事録生成の system プロンプト（英語）。日本語版と同じ構成にして、言語だけが違う状態で
    /// 比べられるようにする。
    const SYSTEM_EN: &str = "You are a meeting scribe. You write minutes from a transcript.\n\
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

    /// system プロンプトとトランスクリプトを、モデルのチャットテンプレートへ流し込む。
    fn build_prompt(
        model: &LlamaModel,
        lang: &str,
        transcript: &str,
    ) -> Result<String, Box<dyn std::error::Error>> {
        let (system, user) = if lang == "ja" {
            (
                SYSTEM_JA,
                format!("次の文字起こしから議事録を作成してください。\n\n{transcript}"),
            )
        } else {
            (
                SYSTEM_EN,
                format!("Write the minutes for the following transcript.\n\n{transcript}"),
            )
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
        ctx_size: u32,
        load: Duration,
        prefill: Duration,
        generate: Duration,
    ) {
        section("Measurements");
        // n_ctx は KV キャッシュの大きさ＝ピーク RSS を直接決めるので、数値と一緒に残す。
        println!("  n_ctx: {ctx_size} (KV cache is allocated for this, not for the prompt length)");
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
            // ピークメモリはこのプローブが測るべき項目の一つなので、取れなかった理由を残す。
            None => println!(
                "  peak RSS: <unavailable> (getrusage failed: {})",
                std::io::Error::last_os_error()
            ),
        }
    }

    /// プロセスのピーク RSS（バイト）。macOS の `ru_maxrss` はバイト単位。
    fn peak_rss_bytes() -> Option<u64> {
        use std::ffi::c_int;

        /// `sys/resource.h` の `RUSAGE_SELF`。
        const RUSAGE_SELF: c_int = 0;

        /// `sys/resource.h` の `struct rusage`。必要なのは `ru_maxrss` だけだが、`getrusage` は
        /// 構造体全体ぶんの書き込み先を要求するため末尾まで確保する（`sizeof == 144`。
        /// `timeval` 16 バイト × 2 ＋ `long` × 14。`ru_maxrss` のオフセットは 32）。
        ///
        /// `ru_utime` / `ru_stime` は `struct timeval`（`tv_sec: i64` ＋ `tv_usec: i32` ＋
        /// パディング 4 の計 16 バイト）。値は読まないので、中身を写さず不透明な 16 バイトとして
        /// 確保する。
        ///
        /// `libc::rusage` を使えばこの写しは不要（`libc` は screencapturekit 経由で既に依存
        /// ツリーにある）。検証専用のプローブで、レイアウトの根拠を doc に残せば足りると判断して
        /// 手書きのままにしている。本実装へ持ち込むなら `libc` に寄せること。
        #[repr(C)]
        struct Rusage {
            ru_utime: [u8; 16],
            ru_stime: [u8; 16],
            ru_maxrss: i64,
            /// `ru_ixrss` 以降（`ru_nivcsw` まで）の 13 個。読まない。
            ru_rest: [i64; 13],
        }

        unsafe extern "C" {
            fn getrusage(who: c_int, usage: *mut Rusage) -> c_int;
        }

        // SAFETY: `Rusage` は整数だけの POD なので全ゼロは有効な値。書き込み先は構造体全体。
        let mut usage: Rusage = unsafe { std::mem::zeroed() };
        // SAFETY: usage は有効な書き込み先。戻り値 0 が成功。
        let status = unsafe { getrusage(RUSAGE_SELF, &raw mut usage) };
        (status == 0).then_some(usage.ru_maxrss as u64)
    }

    /// 内蔵のサンプル。実際の会議データを使わずに再現できるようにするためのもので、
    /// 話者の入れ替わり・決定事項・条件つきの判断・担当者つきアクション・「該当なし」になる
    /// 議題を含む。**本題から外れた雑談は入っていない**ので、「議事録に載せるべきでない発話を
    /// 落とせるか」はこのサンプルでは検証できない（#80 の回帰確認で見る）。
    fn sample_transcript(lang: &str) -> &'static str {
        if lang == "ja" {
            include_str!("../assets/samples/meeting-ja.txt")
        } else {
            include_str!("../assets/samples/meeting-en.txt")
        }
    }
}
