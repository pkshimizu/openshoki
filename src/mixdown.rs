//! 録音停止後のバックグラウンド後処理パイプライン。セッションごとに次を**この順で**行う:
//!
//! 1. **音量の正規化**: 会議アプリ（ブラウザの Google Meet 等）の自動ゲイン調整（AGC）が
//!    マイクデバイスの OS 入力ボリュームを下げることがあり、録音が極端に小さく「無音に聞こえる」
//!    ことがある（実測でピーク -35dBFS / RMS -61dBFS の実例）。極端に小さい音源だけを
//!    ピーク正規化して保存し直す（正常な音源は無加工）。デバイスゲインを openshoki 側から
//!    操作するのは会議アプリと奪い合いになるため行わない。
//! 2. **ミックス生成**: マイク（`mic.mp3`）とシステム音声（`system.mp3`）が両方あるセッションで
//!    `mix.mp3` を生成する。一覧選択のたびにミックスすると長い録音で UI が固まるため、重い
//!    デコード＋再エンコードは録音直後の 1 回へ移す（再生は `src/player.rs` がストリーミング）。
//! 3. **文字起こしの投入**: 設定 ON のとき `TranscribeWorker` へ投入する。正規化・ミックスの
//!    **後**に投入することで、文字起こしは正規化済みの音声を使う。
//!
//! 生成物は録音データと同じ機微ファイルなので所有者のみ読み書き可（Unix 0600）で作る
//! （`docs/rules/security.md`）。各段の失敗は worker スレッド内でログして次の段へ進み、
//! 常駐を続ける（`docs/rules/error-handling.md`）。

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};

use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, MonoPcm, Quality};
use rodio::conversions::{ChannelCountConverter, SampleRateConverter};
use rodio::{ChannelCount, Decoder, SampleRate, Source};

/// ミックス出力のファイル名。セッションディレクトリに固定名で置く（`mic.mp3` / `system.mp3` と同系統。
/// `recordings.rs` の再生対象判定と一致させること）。
pub const MIX_FILENAME: &str = "mix.mp3";
const MIC_FILENAME: &str = "mic.mp3";
const SYSTEM_FILENAME: &str = "system.mp3";

/// エンコード設定は `recorder.rs` の録音出力と揃える（同じ音質・容量特性で保存する）。
const BITRATE: Bitrate = Bitrate::Kbps128;
const QUALITY: Quality = Quality::Good;

/// 正規化を発動するピークのしきい値（dBFS）。正常な発話はピーク -12〜-3dB 程度で、実例の
/// 極小録音は -35dB。この間に置き、少し小さめの正常録音は触らない。
const NORMALIZE_TRIGGER_PEAK_DB: f32 = -30.0;

/// 正規化の目標ピーク（dBFS）。クリップしない単純増幅で歪ませない。
const NORMALIZE_TARGET_PEAK_DB: f32 = -3.0;

/// 正規化で掛けるゲインの上限（dB）。ノイズフロアの過剰増幅を防ぐ。
const NORMALIZE_MAX_GAIN_DB: f32 = 40.0;

/// 実質無音とみなすピーク（dBFS）。これ未満は「何も録れていない」ので増幅しない
/// （無音を爆音ノイズにしない）。
const SILENCE_FLOOR_PEAK_DB: f32 = -70.0;

/// 1 セッション分の後処理ジョブ。停止時点の保存結果と設定のスナップショットを持つ。
pub struct PostProcessJob {
    /// セッションディレクトリ（ミックス出力先）。
    pub session_dir: PathBuf,
    /// 停止時に保存できた音源ファイル（`mic.mp3` / `system.mp3`）。正規化の対象。
    pub saved: Vec<PathBuf>,
    /// 文字起こしの依頼（設定 OFF なら `None`）。正規化・ミックス完了後に投入する。
    pub transcribe: Option<crate::transcribe::TranscribeJob>,
}

/// 録音停止後の後処理（正規化→ミックス→文字起こし投入）のバックグラウンドワーカー。
/// 1 本のスレッドで逐次処理する（デコード＋再エンコードは重いため録音が連続してもスレッドを増やさない）。
pub struct PostProcessWorker {
    /// ワーカースレッドへの送信口。スレッド起動に失敗していたら `None`（後処理のみ縮退）。
    tx: Option<Sender<PostProcessJob>>,
}

impl PostProcessWorker {
    /// ワーカースレッドを起動する。`transcriber` は後処理完了後の文字起こし投入に使う
    /// （このワーカーが所有し、停止フックからの直接投入は行わない）。スレッド生成に失敗しても
    /// 常駐アプリは落とさず、後処理だけを無効化してログを残す（スレッドは detach。終了時に
    /// 処理中のジョブは中断される）。
    pub fn start(transcriber: crate::transcribe::TranscribeWorker) -> Self {
        let (tx, rx) = mpsc::channel::<PostProcessJob>();
        let spawned = std::thread::Builder::new()
            .name("postprocess-worker".into())
            .spawn(move || {
                // 送信側（アプリ本体）が落ちてチャネルが閉じたら自然に終了する。
                while let Ok(job) = rx.recv() {
                    run_job(job, &transcriber);
                }
            });
        match spawned {
            Ok(_handle) => Self { tx: Some(tx) },
            Err(err) => {
                eprintln!(
                    "Disabling post-processing because the worker thread failed to start: {err}"
                );
                Self { tx: None }
            }
        }
    }

    /// セッションの後処理を投入する。ワーカーが動いていない場合はログのみ
    /// （ミックス・文字起こしが走らないだけで録音は保存済み）。
    pub fn submit(&self, job: PostProcessJob) {
        let Some(tx) = &self.tx else {
            eprintln!("Skipping post-processing because the post-process worker is not running");
            return;
        };
        if tx.send(job).is_err() {
            eprintln!("Skipping post-processing because the post-process worker is not running");
        }
    }
}

/// 1 セッションの後処理を実行する。各段の失敗はログして次の段へ進む（正規化に失敗しても
/// 元の音声でミックス・文字起こしは行える。逆に文字起こしだけのために全体を止めない）。
fn run_job(job: PostProcessJob, transcriber: &crate::transcribe::TranscribeWorker) {
    // 1) 極端に小さい音源の正規化（対象は保存済みの各音源）。
    for path in &job.saved {
        let name = file_name_for_log(path);
        match normalize_if_quiet(path) {
            Ok(NormalizeOutcome::Normalized { peak_db, gain_db }) => println!(
                "Normalized {name} because it was too quiet (peak {peak_db:.1} dBFS, applied +{gain_db:.1} dB)"
            ),
            // 非発動でもピークをログし、音量問題の診断（本当に無音か・単に小さいか）に使えるようにする。
            Ok(NormalizeOutcome::Unchanged { peak_db }) => {
                println!("Peak level of {name}: {peak_db:.1} dBFS")
            }
            Err(err) => {
                eprintln!("Skipping normalization of {name} because it failed: {err}")
            }
        }
    }

    // 2) 両音源が揃っていればミックスを生成する（正規化後の音声を使う）。
    let has_name = |name: &str| {
        job.saved
            .iter()
            .any(|p| p.file_name().is_some_and(|f| f == name))
    };
    if has_name(MIC_FILENAME)
        && has_name(SYSTEM_FILENAME)
        && let Err(err) = generate_mix(&job.session_dir)
    {
        eprintln!("Skipping mixdown because generating the mixed file failed: {err}");
    }

    // 3) 文字起こしの投入（設定 ON のときだけ依頼が入っている）。
    if let Some(transcribe_job) = job.transcribe {
        transcriber.submit(transcribe_job);
    }
}

/// ログ用のファイル名（フルパスは機微情報なので出さない。既存の録音保存ログと同じ方針）。
fn file_name_for_log(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio".to_owned())
}

/// 正規化の結果（ログ用の計測値つき）。
enum NormalizeOutcome {
    /// 正規化して保存し直した。
    Normalized { peak_db: f32, gain_db: f32 },
    /// 対象外（正常レベル・実質無音）で無加工。
    Unchanged { peak_db: f32 },
}

/// 音源のピークを測り、極端に小さければピーク正規化して同名で保存し直す（一時ファイル→
/// rename の原子的置換、0600）。対象外なら何も書かない。
fn normalize_if_quiet(path: &Path) -> Result<NormalizeOutcome, Box<dyn std::error::Error>> {
    let decoded = decode(path)?;
    let peak = peak_of(&decoded.pcm);
    let peak_db = amplitude_to_db(peak);
    let Some(gain_db) = normalization_gain_db(peak_db) else {
        return Ok(NormalizeOutcome::Unchanged { peak_db });
    };

    let gain = db_to_amplitude(gain_db);
    let mut pcm = decoded.pcm;
    for sample in &mut pcm {
        *sample = (*sample * gain).clamp(-1.0, 1.0);
    }
    let mp3 = encode_mp3(&to_i16(&pcm), decoded.channels, decoded.sample_rate)?;
    drop(pcm);

    // 一時ファイルへ書いてから rename で置き換える（途中で失敗しても元ファイルが壊れない）。
    let part = path.with_extension(format!("mp3.part.{}", std::process::id()));
    let result = write_owner_only(&part, &mp3).and_then(|()| std::fs::rename(&part, path));
    if let Err(err) = result {
        // 後始末の失敗も黙って捨てない（docs/rules/error-handling.md）。
        if let Err(remove_err) = std::fs::remove_file(&part)
            && remove_err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("Failed to remove the partially written audio: {remove_err}");
        }
        return Err(err.into());
    }
    Ok(NormalizeOutcome::Normalized { peak_db, gain_db })
}

/// PCM の絶対値ピーク（0.0〜）。
fn peak_of(pcm: &[f32]) -> f32 {
    pcm.iter().fold(0.0f32, |max, s| max.max(s.abs()))
}

/// 振幅（0.0〜1.0）→ dBFS。0 は負の無限大になる（`normalization_gain_db` の無音ガードが弾く）。
fn amplitude_to_db(amplitude: f32) -> f32 {
    20.0 * amplitude.log10()
}

/// dB → 線形ゲイン。
fn db_to_amplitude(db: f32) -> f32 {
    10.0f32.powf(db / 20.0)
}

/// ピーク（dBFS）から、掛けるべき正規化ゲイン（dB）を決める純粋関数。
/// 対象外（正常レベル・実質無音）は `None`。ゲインは上限で頭打ちにする。
fn normalization_gain_db(peak_db: f32) -> Option<f32> {
    if !(SILENCE_FLOOR_PEAK_DB..NORMALIZE_TRIGGER_PEAK_DB).contains(&peak_db) {
        return None;
    }
    Some((NORMALIZE_TARGET_PEAK_DB - peak_db).min(NORMALIZE_MAX_GAIN_DB))
}

/// セッションの `mic.mp3` と `system.mp3` をミックスして `mix.mp3` を書き出す。
fn generate_mix(session_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mic = decode(&session_dir.join(MIC_FILENAME))?;
    let system = decode(&session_dir.join(SYSTEM_FILENAME))?;
    // 共通形式は両者の大きい方（ダウンサンプル/ダウンミックスによる劣化を避ける）。
    let channels = mic.channels.max(system.channels);
    let sample_rate = mic.sample_rate.max(system.sample_rate);
    let mic_pcm = conform(
        mic.pcm,
        mic.channels,
        mic.sample_rate,
        channels,
        sample_rate,
    );
    let system_pcm = conform(
        system.pcm,
        system.channels,
        system.sample_rate,
        channels,
        sample_rate,
    );
    // 長い方を move で受けて加算合成し、中間バッファの余分な確保を避ける。
    let mixed = if mic_pcm.len() >= system_pcm.len() {
        mix_into(mic_pcm, &system_pcm)
    } else {
        mix_into(system_pcm, &mic_pcm)
    };
    let mp3 = encode_mp3(&to_i16(&mixed), channels, sample_rate)?;
    write_owner_only(&session_dir.join(MIX_FILENAME), &mp3)?;
    Ok(())
}

/// デコード結果（PCM とその形式）。
struct DecodedAudio {
    pcm: Vec<f32>,
    channels: ChannelCount,
    sample_rate: SampleRate,
}

/// MP3 をデコードして f32 PCM（インターリーブ）と形式を得る。
fn decode(path: &Path) -> Result<DecodedAudio, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    let decoder = Decoder::try_from(file)?;
    let channels = decoder.channels();
    let sample_rate = decoder.sample_rate();
    let pcm: Vec<f32> = decoder.collect();
    Ok(DecodedAudio {
        pcm,
        channels,
        sample_rate,
    })
}

/// PCM を目標のチャンネル数・サンプルレートへ整える（チャンネル変換 → レート変換の順）。
/// 形式が一致していれば変換せずそのまま返す（move パススルーでコピーしない）。
fn conform(
    pcm: Vec<f32>,
    from_channels: ChannelCount,
    from_rate: SampleRate,
    to_channels: ChannelCount,
    to_rate: SampleRate,
) -> Vec<f32> {
    if from_channels == to_channels && from_rate == to_rate {
        return pcm;
    }
    // レート変換は変換後のチャンネル数を要求するため、チャンネル変換を先に行う。
    let channel_adjusted = ChannelCountConverter::new(pcm.into_iter(), from_channels, to_channels);
    if from_rate == to_rate {
        channel_adjusted.collect()
    } else {
        SampleRateConverter::new(channel_adjusted, from_rate, to_rate, to_channels).collect()
    }
}

/// `base`（長い方の PCM を move で受ける）へ `other` を要素ごとに加算合成して返す。両者は同一
/// チャンネル数・サンプルレートに整え済みとする。`other` が長ければ 0 で伸長する。
///
/// 和が [-1, 1] を超えたらハードクリップする（＝そこで歪む）。マイクとシステム音声が同時に
/// フルスケール近くになると歪むが、通話録音で同時最大は稀なため、音量を保つ単純加算を優先する。
fn mix_into(mut base: Vec<f32>, other: &[f32]) -> Vec<f32> {
    if other.len() > base.len() {
        base.resize(other.len(), 0.0);
    }
    for (slot, sample) in base.iter_mut().zip(other.iter()) {
        *slot = (*slot + *sample).clamp(-1.0, 1.0);
    }
    base
}

/// f32 PCM（[-1, 1]）を i16 PCM に変換する（LAME エンコーダは i16 を取る）。
fn to_i16(pcm: &[f32]) -> Vec<i16> {
    pcm.iter()
        .map(|&sample| (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

/// i16 インターリーブ PCM を MP3 へエンコードする（`recorder.rs` と同じビットレート・品質）。
fn encode_mp3(
    pcm: &[i16],
    channels: ChannelCount,
    sample_rate: SampleRate,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let mut builder = Builder::new().ok_or("Failed to create the LAME encoder builder")?;
    builder.set_num_channels(channels.get() as u8)?;
    builder.set_sample_rate(sample_rate.get())?;
    builder.set_brate(BITRATE)?;
    builder.set_quality(QUALITY)?;
    let mut encoder = builder.build()?;

    let mut mp3 = Vec::with_capacity(mp3lame_encoder::max_required_buffer_size(pcm.len()));
    // モノラルは MonoPcm、ステレオはインターリーブ（誤ると LAME 内部でバッファ不整合になる）。
    if channels.get() == 1 {
        encoder.encode_to_vec(MonoPcm(pcm), &mut mp3)?;
    } else {
        encoder.encode_to_vec(InterleavedPcm(pcm), &mut mp3)?;
    }
    mp3.reserve(mp3lame_encoder::max_required_buffer_size(0));
    encoder.flush_to_vec::<FlushNoGap>(&mut mp3)?;
    Ok(mp3)
}

/// 機微ファイル（録音データ由来）として所有者のみ読み書き可（Unix 0600）で書き出す。
fn write_owner_only(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(data)
}

#[cfg(test)]
mod tests {
    use super::{
        NORMALIZE_MAX_GAIN_DB, amplitude_to_db, conform, db_to_amplitude, mix_into,
        normalization_gain_db, peak_of, to_i16,
    };
    use std::num::NonZero;

    fn nz_u16(v: u16) -> super::ChannelCount {
        NonZero::new(v).unwrap()
    }
    fn nz_u32(v: u32) -> super::SampleRate {
        NonZero::new(v).unwrap()
    }

    #[test]
    fn mix_into_adds_and_zero_pads_shorter() {
        assert_eq!(
            mix_into(vec![0.1, 0.2, 0.3], &[0.4, 0.4]),
            vec![0.5, 0.6, 0.3]
        );
        assert_eq!(
            mix_into(vec![0.4, 0.4], &[0.1, 0.2, 0.3]),
            vec![0.5, 0.6, 0.3]
        );
    }

    #[test]
    fn mix_into_clamps_to_valid_range() {
        assert_eq!(mix_into(vec![0.8, -0.8], &[0.5, -0.5]), vec![1.0, -1.0]);
    }

    #[test]
    fn conform_returns_input_unchanged_when_formats_match() {
        let pcm = vec![0.1, -0.2, 0.3, -0.4];
        let out = conform(
            pcm.clone(),
            nz_u16(2),
            nz_u32(48_000),
            nz_u16(2),
            nz_u32(48_000),
        );
        assert_eq!(out, pcm);
    }

    #[test]
    fn conform_upmixes_mono_to_stereo_by_duplicating_samples() {
        // レート同一・モノラル→ステレオ。各サンプルが左右に複製され、サンプル数が倍になる。
        let stereo = conform(
            vec![0.5, -0.5],
            nz_u16(1),
            nz_u32(16_000),
            nz_u16(2),
            nz_u32(16_000),
        );
        assert_eq!(stereo, vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn peak_of_finds_absolute_maximum() {
        assert_eq!(peak_of(&[0.1, -0.5, 0.3]), 0.5);
        assert_eq!(peak_of(&[]), 0.0);
    }

    #[test]
    fn db_conversions_round_trip() {
        // -20dB は振幅 0.1。往復して誤差が小さいこと。
        let amp = db_to_amplitude(-20.0);
        assert!((amp - 0.1).abs() < 1e-6);
        assert!((amplitude_to_db(amp) - (-20.0)).abs() < 1e-4);
    }

    #[test]
    fn normalization_gain_covers_boundaries() {
        // 実例（ピーク -35dBFS）: 発動して目標 -3dBFS までの +32dB。
        let gain = normalization_gain_db(-35.0).expect("quiet audio should be normalized");
        assert!((gain - 32.0).abs() < 1e-4);
        // 正常レベル（-20dBFS）としきい値ちょうど（-30dBFS）は発動しない。
        assert!(normalization_gain_db(-20.0).is_none());
        assert!(normalization_gain_db(-30.0).is_none());
        // しきい値の直下は発動する。
        assert!(normalization_gain_db(-30.1).is_some());
        // 非常に小さい（-60dBFS）はゲイン上限で頭打ち（必要 +57dB → +40dB）。
        assert_eq!(normalization_gain_db(-60.0), Some(NORMALIZE_MAX_GAIN_DB));
        // 実質無音（-70dBFS 未満）と完全無音（-inf）は発動しない。
        assert!(normalization_gain_db(-70.1).is_none());
        assert!(normalization_gain_db(f32::NEG_INFINITY).is_none());
    }

    #[test]
    fn normalize_if_quiet_boosts_quiet_mp3_end_to_end() {
        // 実例（ピーク約 -35dBFS の Meet 録音）相当の極小 MP3 を合成し、正規化パイプライン全体
        // （デコード→判定→ゲイン→再エンコード→原子的置換）で可聴レベルへ上がることを確認する。
        let sample_rate = 48_000u32;
        let amplitude = super::db_to_amplitude(-35.0);
        let pcm: Vec<i16> = (0..sample_rate as usize)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                ((2.0 * std::f32::consts::PI * 440.0 * t).sin() * amplitude * i16::MAX as f32)
                    as i16
            })
            .collect();
        let mp3 = super::encode_mp3(
            &pcm,
            std::num::NonZero::new(1u16).unwrap(),
            std::num::NonZero::new(sample_rate).unwrap(),
        )
        .expect("encoding the quiet test tone should succeed");
        let dir = std::env::temp_dir().join(format!("openshoki-normalize-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creating the temp dir should succeed");
        let path = dir.join("mic.mp3");
        std::fs::write(&path, &mp3).expect("writing the test MP3 should succeed");

        let outcome = super::normalize_if_quiet(&path).expect("normalization should succeed");
        assert!(
            matches!(outcome, super::NormalizeOutcome::Normalized { .. }),
            "a -35 dBFS recording should be normalized"
        );

        // 正規化後のピークが目標（-3dBFS）付近まで上がっている（MP3 の非可逆誤差を許容）。
        let normalized = super::decode(&path).expect("the normalized file should decode");
        let peak_db = super::amplitude_to_db(super::peak_of(&normalized.pcm));
        assert!(
            (-6.0..=0.0).contains(&peak_db),
            "expected the peak near -3 dBFS, got {peak_db:.1} dBFS"
        );

        // 正常レベルになったので 2 回目は無加工（冪等）。
        let second = super::normalize_if_quiet(&path).expect("second pass should succeed");
        assert!(matches!(second, super::NormalizeOutcome::Unchanged { .. }));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path)
                .expect("metadata should be readable")
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "the rewritten file must stay 0600");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn to_i16_scales_and_clamps() {
        assert_eq!(to_i16(&[0.0]), vec![0]);
        assert_eq!(to_i16(&[1.0]), vec![i16::MAX]);
        // 範囲外は [-1, 1] に丸めてから変換する。
        assert_eq!(to_i16(&[2.0]), vec![i16::MAX]);
        assert_eq!(to_i16(&[-2.0]), vec![-i16::MAX]);
    }
}
