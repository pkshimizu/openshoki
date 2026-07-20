//! 録音停止後に、マイク（`mic.mp3`）とシステム音声（`system.mp3`）をミックスした `mix.mp3` を
//! **バックグラウンドで生成**する。
//!
//! 一覧選択のたびに両音源をデコード＋ミックスすると、長い録音では UI が数秒〜数十秒固まる。
//! そこで重いデコード＋ミックス＋再エンコードを録音直後の 1 回へ移し、Recordings の再生は
//! 生成済み `mix.mp3` をそのままストリーミング再生する（`src/player.rs`）。両音源があるセッション
//! だけ生成し、単一音源セッションは元ファイルを直接再生するため生成しない。
//!
//! 生成物は録音データと同じ機微ファイルなので所有者のみ読み書き可（Unix 0600）で作る
//! （`docs/rules/security.md`）。生成の失敗は worker スレッド内でログして常駐を続ける
//! （`docs/rules/error-handling.md`）。

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

/// ミックス生成のバックグラウンドワーカー。録音停止のたびにセッションディレクトリを `submit` する。
/// 1 本のスレッドで逐次処理する（デコード＋再エンコードは重いため録音が連続してもスレッドを増やさない）。
pub struct MixWorker {
    /// ワーカースレッドへの送信口。スレッド起動に失敗していたら `None`（ミックス生成のみ縮退）。
    tx: Option<Sender<PathBuf>>,
}

impl MixWorker {
    /// ワーカースレッドを起動する。スレッド生成に失敗しても常駐アプリは落とさず、ミックス生成
    /// だけを無効化してログを残す（スレッドは detach。終了時に処理中のジョブは中断される）。
    pub fn start() -> Self {
        let (tx, rx) = mpsc::channel::<PathBuf>();
        let spawned = std::thread::Builder::new()
            .name("mixdown-worker".into())
            .spawn(move || {
                // 送信側（アプリ本体）が落ちてチャネルが閉じたら自然に終了する。
                while let Ok(session_dir) = rx.recv() {
                    if let Err(err) = generate_mix(&session_dir) {
                        eprintln!(
                            "Skipping mixdown because generating the mixed file failed: {err}"
                        );
                    }
                }
            });
        match spawned {
            Ok(_handle) => Self { tx: Some(tx) },
            Err(err) => {
                eprintln!("Disabling mixdown because the worker thread failed to start: {err}");
                Self { tx: None }
            }
        }
    }

    /// セッションディレクトリのミックス生成を投入する。両音源が揃っているセッションだけ渡すこと。
    /// ワーカーが動いていない場合はログのみ（再生できないだけで録音は保存済み）。
    pub fn submit(&self, session_dir: PathBuf) {
        let Some(tx) = &self.tx else {
            eprintln!("Skipping mixdown because the mixdown worker is not running");
            return;
        };
        if tx.send(session_dir).is_err() {
            eprintln!("Skipping mixdown because the mixdown worker is not running");
        }
    }
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
    use super::{conform, mix_into, to_i16};
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
    fn to_i16_scales_and_clamps() {
        assert_eq!(to_i16(&[0.0]), vec![0]);
        assert_eq!(to_i16(&[1.0]), vec![i16::MAX]);
        // 範囲外は [-1, 1] に丸めてから変換する。
        assert_eq!(to_i16(&[2.0]), vec![i16::MAX]);
        assert_eq!(to_i16(&[-2.0]), vec![-i16::MAX]);
    }
}
