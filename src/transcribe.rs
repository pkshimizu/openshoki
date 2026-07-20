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

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};

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
    /// 対象の音声ファイル（セッションディレクトリ内の `mic.mp3` / `system.mp3`）。
    pub audio_paths: Vec<PathBuf>,
    /// whisper モデルファイル（ggml 形式）のパス。
    pub model_path: PathBuf,
    /// 認識言語（例: `ja`）。`None` は whisper の自動判定。
    pub language: Option<String>,
}

/// 文字起こしのバックグラウンドワーカー。`submit` されたジョブを 1 本のスレッドで逐次処理する
/// （whisper は CPU 集約のため、録音が連続してもスレッドを増やさない）。
pub struct TranscribeWorker {
    /// ワーカースレッドへの送信口。スレッド起動に失敗していたら `None`（文字起こしのみ縮退）。
    tx: Option<Sender<TranscribeJob>>,
}

impl TranscribeWorker {
    /// ワーカースレッドを起動する。スレッド生成に失敗しても常駐アプリは落とさず、
    /// 文字起こしだけを無効化してログを残す。
    ///
    /// スレッドは意図的に join しない（detach）: 文字起こしは数分かかりうるため、終了時に
    /// join するとアプリの終了がブロックされる。常駐終了時に処理中のジョブは中断される
    /// （ベストエフォート。#30 のスコープ）。
    pub fn start() -> Self {
        // whisper.cpp / GGML が stderr へ出す冗長な内部ログを止める（ログ backend の feature を
        // 有効にしていないため、フック先が無く事実上の無効化になる）。複数回呼んでも安全。
        whisper_rs::install_logging_hooks();
        let (tx, rx) = mpsc::channel::<TranscribeJob>();
        let spawned = std::thread::Builder::new()
            .name("transcribe-worker".into())
            .spawn(move || {
                // 送信側（アプリ本体）が落ちてチャネルが閉じたら自然に終了する。
                while let Ok(job) = rx.recv() {
                    run_job(&job);
                }
            });
        match spawned {
            Ok(_handle) => Self { tx: Some(tx) },
            Err(err) => {
                eprintln!(
                    "Disabling transcription because the worker thread failed to start: {err}"
                );
                Self { tx: None }
            }
        }
    }

    /// ジョブを投入する。ワーカーが動いていない場合はログのみ（録音自体は既に保存済み）。
    pub fn submit(&self, job: TranscribeJob) {
        let Some(tx) = &self.tx else {
            eprintln!("Skipping transcription because the transcription worker is not running");
            return;
        };
        // 送信失敗 = ワーカースレッドが（panic 等で）終了しレシーバが閉じた状態。
        if tx.send(job).is_err() {
            eprintln!("Skipping transcription because the transcription worker is not running");
        }
    }
}

/// 1 ジョブ（1 回の録音停止分）を処理する。モデルはジョブ内で 1 回だけロードして
/// 複数音源で使い回す（モデルのロードが重いため）。音源単位の失敗は他の音源へ波及させない。
fn run_job(job: &TranscribeJob) {
    if job.audio_paths.is_empty() {
        // 対象なしでモデル（数百 MB〜）をロードしない防御。通常は投入側が空を渡さない。
        return;
    }
    if !job.model_path.is_file() {
        eprintln!(
            "Skipping transcription because the whisper model file was not found: {}",
            job.model_path.display()
        );
        return;
    }
    let Some(model_path) = job.model_path.to_str() else {
        eprintln!("Skipping transcription because the whisper model path is not valid UTF-8");
        return;
    };
    let ctx = match WhisperContext::new_with_params(model_path, WhisperContextParameters::default())
    {
        Ok(ctx) => ctx,
        Err(err) => {
            eprintln!(
                "Skipping transcription because loading the whisper model failed ({}): {err}",
                job.model_path.display()
            );
            return;
        }
    };
    for path in &job.audio_paths {
        // ログには（既存の録音保存ログと同じ方針で）フルパスを出さず、ファイル名だけにする。
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "audio".to_owned());
        match transcribe_file(&ctx, path, job) {
            Ok(segments) => println!("Transcribed {name} ({segments} segments)"),
            Err(err) => eprintln!("Skipping transcription of {name} because it failed: {err}"),
        }
    }
}

/// 1 音源を文字起こしして `<音源名>.json` に保存する。成功時はセグメント数を返す。
fn transcribe_file(
    ctx: &WhisperContext,
    audio_path: &Path,
    job: &TranscribeJob,
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
    // 言語は設定 TOML（手編集されうる信頼境界外）由来。whisper-rs の set_language は NUL バイトを
    // 含む文字列で panic するため（内部の CString::new が expect）、ここで弾いて自動判定へ
    // フォールバックする。未知の言語コードは whisper.cpp 側が検証して full() が Err を返すので、
    // ここでは NUL だけ防げばよい。
    match job.language.as_deref() {
        Some(language) if language.contains('\0') => {
            eprintln!(
                "Ignoring the configured transcription language because it contains a NUL byte"
            );
        }
        Some(language) => params.set_language(Some(language)),
        None => {}
    }

    let mut state = ctx.create_state()?;
    state.full(params, &pcm)?;

    let segments = collect_segments(&state);
    let result = Transcription {
        source,
        model: job
            .model_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        language: job.language.clone().unwrap_or_else(|| "auto".to_owned()),
        duration_secs,
        segments,
    };
    let json_path = audio_path.with_extension("json");
    write_transcription(&json_path, &result)?;
    Ok(result.segments.len())
}

/// 文字起こし結果の保存形式。録音一覧ビュー（#54）が読む契約なので、`segments` の
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

/// 文字起こし結果を JSON で保存する。録音と同じ機微データなので Unix では 0600 で作成する
/// （`docs/rules/security.md`。セッションディレクトリ自体は録音側が 0700 で作成済み）。
fn write_transcription(
    path: &Path,
    result: &Transcription,
) -> Result<(), Box<dyn std::error::Error>> {
    let json = serde_json::to_string_pretty(result)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(json.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let path = dir.join(format!("openshoki-empty-{}.mp3", std::process::id()));
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
        let path = dir.join(format!(
            "openshoki-transcription-{}.json",
            std::process::id()
        ));
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
    /// `OPENSHOKI_WHISPER_MODEL=<path> cargo test -- --ignored` で実行する。
    /// 入力は合成サイン波（発話なし）なので、認識テキストではなく「JSON が既定の形・0600 で
    /// 生成される」ことだけを確認する。
    #[test]
    #[ignore = "requires a whisper model; set OPENSHOKI_WHISPER_MODEL and run with --ignored"]
    fn end_to_end_writes_transcription_json_for_generated_mp3() {
        let model_path = std::env::var("OPENSHOKI_WHISPER_MODEL")
            .expect("OPENSHOKI_WHISPER_MODEL must point to a ggml whisper model");

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
            std::env::temp_dir().join(format!("openshoki-transcribe-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creating the temp dir should succeed");
        let audio_path = dir.join("mic.mp3");
        std::fs::write(&audio_path, &mp3).expect("writing the test MP3 should succeed");

        run_job(&TranscribeJob {
            audio_paths: vec![audio_path.clone()],
            model_path: PathBuf::from(model_path),
            language: None,
        });

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
        // 録音一覧ビュー（#54）が読む契約: segments[].start/end/text（秒）。
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
