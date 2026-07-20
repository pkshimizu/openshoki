//! 録音セッションの音声再生。`rodio` で既定の出力デバイスへ、マイク（`mic.mp3`）とシステム音声
//! （`system.mp3`）を**ミックスした単一タイムライン**として再生する。
//!
//! 両音源があるセッションは、各 MP3 をデコードして PCM（f32）にし、共通のチャンネル数・サンプル
//! レートへ整えてから加算合成し、1 本の `SamplesBuffer`（メモリ内・シーク可能）にして再生する。
//! 単一タイムラインなので、経過時間・シーク（#54 の文字起こしスキップ）が素直に扱える。片方のみの
//! セッションはその音源だけを（同じくメモリ内バッファで）再生する。
//!
//! ロードしたソースは保持し、Play/Stop で再投入する。`rodio` の再生キューはソースを消費し、終端に
//! 達すると空になるため、保持していないと「終端後に再生できない」状態になる。保持ソースは
//! `SamplesBuffer`（内部 `Arc<[f32]>`）で、`clone` しても PCM は共有されメモリは増えない。
//!
//! 出力ストリーム（`cpal`）は録音側と同じくメインスレッドで保持する（`!Send` を跨がせない）。
//! デコード・再生の失敗はハンドル生成時・ロード時に `Result` で返し、呼び出し側はログして常駐を
//! 続ける（`docs/rules/error-handling.md`）。

use std::fs::File;
use std::path::Path;
use std::time::Duration;

use rodio::buffer::SamplesBuffer;
use rodio::conversions::{ChannelCountConverter, SampleRateConverter};
use rodio::{
    ChannelCount, Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, SampleRate, Source,
};

/// 再生の制御ハンドル。既定出力デバイスへ接続した状態を保持する。
///
/// `_sink`（`MixerDeviceSink`）は drop すると出力ストリームが止まるため、再生中は保持し続ける。
/// `cpal::Stream` を内包し `!Send` の可能性があるため、メインスレッド上でのみ扱う。
pub struct AudioPlayer {
    /// 出力ストリーム。保持のみ（drop で停止）。
    _sink: MixerDeviceSink,
    /// 再生キュー（旧 Sink 相当）。play/pause/seek/位置取得を担う。
    player: Player,
    /// 現在ロード中のソース。終端後や停止後に再投入するため保持する（`clone` は PCM 共有で軽い）。
    source: Option<SamplesBuffer>,
    /// 現在ロード中ソースの全体長（分かる場合）。
    duration: Option<Duration>,
}

impl AudioPlayer {
    /// 既定の出力デバイスへ接続して再生ハンドルを作る。デバイスが無い等で失敗したらエラーを返す
    /// （呼び出し側は再生機能無しで続行する）。
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let mut sink = DeviceSinkBuilder::open_default_sink()?;
        // drop 時の stderr 警告は自前のログ方針に委ねるため抑制する。
        sink.log_on_drop(false);
        let player = Player::connect_new(sink.mixer());
        // ロード前は停止状態にしておく（ロード後の Play で鳴らす）。
        player.pause();
        Ok(Self {
            _sink: sink,
            player,
            source: None,
            duration: None,
        })
    }

    /// セッションの音源をロードして再生準備する（停止状態でセット。`play_pause` で再生開始）。
    ///
    /// 両音源があればミックスした単一ソース、片方のみならその音源を、メモリ内バッファにして
    /// キューへ積む。どちらも無ければエラー。
    pub fn load_session(
        &mut self,
        mic: Option<&Path>,
        system: Option<&Path>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let (source, duration) = match (mic, system) {
            (Some(mic_path), Some(system_path)) => {
                let mic = decode(mic_path)?;
                let system = decode(system_path)?;
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
                let duration = frames_duration(mixed.len(), channels, sample_rate);
                (
                    SamplesBuffer::new(channels, sample_rate, mixed),
                    Some(duration),
                )
            }
            (Some(path), None) | (None, Some(path)) => {
                let audio = decode(path)?;
                let duration = frames_duration(audio.pcm.len(), audio.channels, audio.sample_rate);
                (
                    SamplesBuffer::new(audio.channels, audio.sample_rate, audio.pcm),
                    Some(duration),
                )
            }
            (None, None) => return Err("the session has no audio to play".into()),
        };

        self.duration = duration;
        // ソースを保持し（終端後・停止後の再投入用）、キューへは複製を積む。
        self.source = Some(source.clone());
        self.player.clear();
        self.player.append(source);
        self.player.pause();
        Ok(())
    }

    /// 再生と一時停止をトグルする。終端に達して（または停止後で）キューが空なら、保持ソースを
    /// 頭から再投入して再生する。
    pub fn play_pause(&self) {
        if self.player.empty() {
            if let Some(source) = &self.source {
                self.player.append(source.clone());
                self.player.play();
            }
        } else if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    /// 停止して先頭へ戻す。キューを作り直して保持ソースを頭から積み直し、一時停止状態にする
    /// （`play_pause` で頭から再生できる）。
    pub fn stop(&self) {
        self.player.clear();
        if let Some(source) = &self.source {
            self.player.append(source.clone());
        }
        self.player.pause();
    }

    /// 現在の再生位置。
    pub fn position(&self) -> Duration {
        self.player.get_pos()
    }

    /// ロード中ソースの全体長（分かる場合）。
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// 再生中か（一時停止でなく、キューが空でない）。
    pub fn is_playing(&self) -> bool {
        !self.player.is_paused() && !self.player.empty()
    }
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
    // File 用の TryFrom は BufReader 化と byte_len 設定を行う。
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

/// インターリーブ PCM のサンプル総数から再生時間を求める。
fn frames_duration(
    total_samples: usize,
    channels: ChannelCount,
    sample_rate: SampleRate,
) -> Duration {
    let frames = total_samples as f64 / channels.get() as f64;
    Duration::from_secs_f64(frames / sample_rate.get() as f64)
}

#[cfg(test)]
mod tests {
    use super::{conform, frames_duration, mix_into};
    use std::num::NonZero;
    use std::time::Duration;

    fn nz_u16(v: u16) -> super::ChannelCount {
        NonZero::new(v).unwrap()
    }
    fn nz_u32(v: u32) -> super::SampleRate {
        NonZero::new(v).unwrap()
    }

    #[test]
    fn mix_into_adds_and_zero_pads_shorter() {
        // base（長い方）へ短い方を加算し、余りは base のまま（＝短い方を 0 埋め加算した形）。
        assert_eq!(
            mix_into(vec![0.1, 0.2, 0.3], &[0.4, 0.4]),
            vec![0.5, 0.6, 0.3]
        );
        // base が短い場合は 0 伸長してから加算する。
        assert_eq!(
            mix_into(vec![0.4, 0.4], &[0.1, 0.2, 0.3]),
            vec![0.5, 0.6, 0.3]
        );
    }

    #[test]
    fn mix_into_clamps_to_valid_range() {
        // 和が ±1 を超えたらハードクリップする。
        assert_eq!(mix_into(vec![0.8, -0.8], &[0.5, -0.5]), vec![1.0, -1.0]);
    }

    #[test]
    fn conform_returns_input_unchanged_when_formats_match() {
        // 形式一致なら変換せずそのまま返す（move パススルー）。
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
        let mono = vec![0.5, -0.5];
        let stereo = conform(mono, nz_u16(1), nz_u32(16_000), nz_u16(2), nz_u32(16_000));
        assert_eq!(stereo, vec![0.5, 0.5, -0.5, -0.5]);
    }

    #[test]
    fn frames_duration_matches_sample_count() {
        // ステレオ 48kHz で 1 秒 = 96000 サンプル。
        assert_eq!(
            frames_duration(96_000, nz_u16(2), nz_u32(48_000)),
            Duration::from_secs(1)
        );
        // モノラル 16kHz で 8000 サンプル = 0.5 秒。
        assert_eq!(
            frames_duration(8_000, nz_u16(1), nz_u32(16_000)),
            Duration::from_secs_f64(0.5)
        );
    }
}
