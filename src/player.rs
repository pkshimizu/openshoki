//! 録音セッションの音声再生。`rodio` で既定の出力デバイスへ、マイク（`mic.mp3`）とシステム音声
//! （`system.mp3`）を**ミックスした単一タイムライン**として再生する。
//!
//! 両音源があるセッションは、各 MP3 をデコードして PCM（f32）にし、共通のチャンネル数・サンプル
//! レートへ整えてから加算合成し、1 本の `SamplesBuffer`（メモリ内・シーク可能）にして再生する。
//! 単一タイムラインなので、経過時間・シーク（#54 の文字起こしスキップ）が素直に扱える。片方のみの
//! セッションはその音源だけを（同じくメモリ内バッファで）再生する。
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
        // 直前のソースを除去してから積み直す。
        self.player.clear();

        let (source, duration) = match (mic, system) {
            (Some(mic), Some(system)) => {
                let a = decode(mic)?;
                let b = decode(system)?;
                // 共通形式は両者の大きい方（ダウンサンプル/ダウンミックスによる劣化を避ける）。
                let channels = a.channels.max(b.channels);
                let sample_rate = a.sample_rate.max(b.sample_rate);
                let am = conform(a.pcm, a.channels, a.sample_rate, channels, sample_rate);
                let bm = conform(b.pcm, b.channels, b.sample_rate, channels, sample_rate);
                let mixed = mix_sum(&am, &bm);
                let duration = frames_duration(mixed.len(), channels, sample_rate);
                (
                    SamplesBuffer::new(channels, sample_rate, mixed),
                    Some(duration),
                )
            }
            (Some(path), None) | (None, Some(path)) => {
                let d = decode(path)?;
                let duration = frames_duration(d.pcm.len(), d.channels, d.sample_rate);
                (
                    SamplesBuffer::new(d.channels, d.sample_rate, d.pcm),
                    Some(duration),
                )
            }
            (None, None) => return Err("the session has no audio to play".into()),
        };

        self.duration = duration;
        self.player.append(source);
        self.player.pause();
        Ok(())
    }

    /// 再生と一時停止をトグルする。
    pub fn play_pause(&self) {
        if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    /// 停止して先頭へ戻す（キューは保持し、Play で頭から再生できる）。
    pub fn stop(&self) {
        self.player.pause();
        // 先頭へシーク。メモリ内バッファはシーク可能なので通常成功する。失敗しても停止は済む。
        if let Err(err) = self.player.try_seek(Duration::ZERO) {
            eprintln!("Failed to rewind to the start on stop: {err}");
        }
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
/// 形式が一致していれば変換せずそのまま返す。
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

/// 2 つの PCM（同一チャンネル数・サンプルレートに整え済み）を要素ごとに加算合成する。
/// 長さは長い方に合わせ、短い方は無音（0）で埋める。加算でクリップしないよう [-1, 1] に丸める。
fn mix_sum(a: &[f32], b: &[f32]) -> Vec<f32> {
    let len = a.len().max(b.len());
    let mut mixed = Vec::with_capacity(len);
    for i in 0..len {
        let sample = a.get(i).copied().unwrap_or(0.0) + b.get(i).copied().unwrap_or(0.0);
        mixed.push(sample.clamp(-1.0, 1.0));
    }
    mixed
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
    use super::{frames_duration, mix_sum};
    use std::num::NonZero;
    use std::time::Duration;

    #[test]
    fn mix_sum_adds_and_zero_pads_shorter() {
        // 長い方に合わせ、短い方は 0 埋めして加算する。
        let a = [0.1, 0.2, 0.3];
        let b = [0.4, 0.4];
        assert_eq!(mix_sum(&a, &b), vec![0.5, 0.6, 0.3]);
    }

    #[test]
    fn mix_sum_clamps_to_valid_range() {
        // 加算で ±1 を超えたらクリップする（音割れの原因の桁あふれを防ぐ）。
        let a = [0.8, -0.8];
        let b = [0.5, -0.5];
        assert_eq!(mix_sum(&a, &b), vec![1.0, -1.0]);
    }

    #[test]
    fn frames_duration_matches_sample_count() {
        let ch = NonZero::new(2).unwrap();
        let sr = NonZero::new(48_000).unwrap();
        // ステレオ 48kHz で 1 秒 = 96000 サンプル。
        assert_eq!(frames_duration(96_000, ch, sr), Duration::from_secs(1));
        // モノラル 16kHz で 8000 サンプル = 0.5 秒。
        let mono = NonZero::new(1).unwrap();
        let sr16 = NonZero::new(16_000).unwrap();
        assert_eq!(
            frames_duration(8_000, mono, sr16),
            Duration::from_secs_f64(0.5)
        );
    }
}
