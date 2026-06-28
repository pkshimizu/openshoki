//! 録音セッションの開始・停止と MP3 ファイルへの書き出し。
//!
//! トレイ／UI から独立した録音モジュール。`cpal` で既定の入力デバイスからマイク音声 (PCM) を
//! 取得し、専用スレッドで `mp3lame-encoder` を使って MP3 にエンコードしてファイルへ書き出す。
//!
//! 音声コールバック内ではエンコードせず、サンプルをチャネルへ送るだけにして、エンコードと
//! ファイル書き込みは writer スレッドで行う（リアルタイムコールバックを軽く保ち、音飛びを避ける）。
//! 将来システム音声を 2 つ目の音源として足せるよう、音源 1 つ = 1 ファイルの単位で組む。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, MonoPcm, Quality};

/// スレッドを跨ぐためのエラー型（`Send + Sync` が必要）。
type RecordError = Box<dyn std::error::Error + Send + Sync>;

/// エンコードのビットレート。音声録音として十分な品質と容量のバランスで 128 kbps。
const BITRATE: Bitrate = Bitrate::Kbps128;
/// エンコード品質（0=最良〜9=最低）。速度と品質のバランスで Good。
const QUALITY: Quality = Quality::Good;
/// 音源を区別するためのファイル名接尾辞。マイクは `-mic`（後でシステム音声は `-system`）。
const MIC_SUFFIX: &str = "mic";

/// 実行中の録音セッション（マイク音源 1 つ）。
///
/// `cpal::Stream` は `Send` でないため、メインスレッド上でのみ保持する。`stop()` で
/// ストリームを止め、writer スレッドの flush 完了を待ってファイルを確定する。
pub struct Recorder {
    /// 保持している間だけ録音が続く。drop でコールバックが止まり、サンプル送信側も閉じる。
    stream: cpal::Stream,
    /// エンコード・書き込みを行う writer スレッド。
    writer: JoinHandle<Result<(), RecordError>>,
    /// 出力先 MP3 ファイルのパス。
    path: PathBuf,
}

impl Recorder {
    /// 既定の入力デバイスからマイク録音を開始する。`recording_dir` 配下にタイムスタンプ名の
    /// `-mic.mp3` を作る（ディレクトリが無ければ作成する）。
    pub fn start(recording_dir: &Path, timestamp: &str) -> Result<Self, RecordError> {
        std::fs::create_dir_all(recording_dir)?;

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("既定の入力デバイスが見つからない")?;
        let supported = device.default_input_config()?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();
        // cpal 0.18 では SampleRate は u32 の型エイリアス。
        let sample_rate = config.sample_rate;
        let channels = config.channels;

        // 未対応のサンプル形式なら、ファイルやスレッドを作る前に弾く（副作用を残さない）。
        if !matches!(
            sample_format,
            SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
        ) {
            return Err(format!("未対応のサンプル形式: {sample_format:?}").into());
        }
        // LAME はモノラル(1)/ステレオ(2)のみ対応。それ以外は弾く。
        if !matches!(channels, 1 | 2) {
            return Err(format!("未対応のチャンネル数: {channels}").into());
        }

        let path = recording_dir.join(format!("openshoki-{timestamp}-{MIC_SUFFIX}.mp3"));
        let file = File::create(&path)?;

        // 音声コールバック → writer スレッドへ PCM を渡すチャネル。
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        let writer = std::thread::Builder::new()
            .name("openshoki-mp3-writer".to_owned())
            .spawn(move || run_writer(rx, sample_rate, channels, file))?;

        let stream = match sample_format {
            SampleFormat::F32 => build_input_stream::<f32>(&device, config, tx),
            SampleFormat::I16 => build_input_stream::<i16>(&device, config, tx),
            SampleFormat::U16 => build_input_stream::<u16>(&device, config, tx),
            // 上で対応形式に絞っているため到達しない。
            other => Err(format!("未対応のサンプル形式: {other:?}").into()),
        }?;
        stream.play()?;

        Ok(Self {
            stream,
            writer,
            path,
        })
    }

    /// 録音を停止し、ファイルを確定して保存先パスを返す。
    ///
    /// ストリームを止めてコールバックとサンプル送信側を閉じてから、writer スレッドの
    /// flush 完了を待つ（末尾フレームを取りこぼさない順序）。
    pub fn stop(self) -> Result<PathBuf, RecordError> {
        let Self {
            stream,
            writer,
            path,
        } = self;
        // ストリームを止める → コールバックが止まり、tx が drop されてチャネルが閉じる。
        drop(stream);
        // writer スレッドの完了（flush・ファイル確定）を待ち、結果を伝播する。
        writer
            .join()
            .map_err(|_| "録音書き込みスレッドがパニックした")??;
        Ok(path)
    }
}

/// 指定のサンプル形式で入力ストリームを構築する。コールバックは PCM を i16 に変換して送るだけ。
fn build_input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    tx: Sender<Vec<i16>>,
) -> Result<cpal::Stream, RecordError>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let err_fn = |err| eprintln!("入力ストリームでエラーが発生した: {err}");
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let pcm: Vec<i16> = data.iter().map(|&s| i16::from_sample(s)).collect();
            if tx.send(pcm).is_err() {
                // writer スレッドが終了済み（エラー等）。その原因は stop() の join() で
                // 受け取るため、ここでは送れなかったサンプルを捨ててよい。
            }
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

/// writer スレッド本体。チャネルから受け取った PCM を MP3 にエンコードして書き込み、
/// チャネルが閉じたら flush してファイルを確定する。
fn run_writer(
    rx: Receiver<Vec<i16>>,
    sample_rate: u32,
    channels: u16,
    file: File,
) -> Result<(), RecordError> {
    let mut encoder = {
        let mut builder = Builder::new().ok_or("LAME エンコーダのビルダー生成に失敗")?;
        builder.set_num_channels(channels as u8)?;
        builder.set_sample_rate(sample_rate)?;
        builder.set_brate(BITRATE)?;
        builder.set_quality(QUALITY)?;
        builder.build()?
    };

    let mut writer = BufWriter::new(file);
    let mut mp3 = Vec::new();

    while let Ok(pcm) = rx.recv() {
        mp3.clear();
        // encode_to_vec は spare capacity にのみ書き込み、自分では reserve しない。
        // 出力に必要な分を先に確保しておく（不足すると LAME がバッファ外へ書きクラッシュする）。
        mp3.reserve(mp3lame_encoder::max_required_buffer_size(pcm.len()));
        // モノラルは MonoPcm、ステレオはインターリーブ。誤ると LAME 内部でバッファ不整合になる。
        // channels は start() で 1/2 のみに絞っている。
        if channels == 1 {
            encoder.encode_to_vec(MonoPcm(pcm.as_slice()), &mut mp3)?;
        } else {
            encoder.encode_to_vec(InterleavedPcm(pcm.as_slice()), &mut mp3)?;
        }
        writer.write_all(&mp3)?;
    }

    // 末尾フレームを書き出してファイルを確定する。flush は最低 7200 バイト必要。
    mp3.clear();
    mp3.reserve(mp3lame_encoder::max_required_buffer_size(0));
    encoder.flush_to_vec::<FlushNoGap>(&mut mp3)?;
    writer.write_all(&mp3)?;
    writer.flush()?;
    Ok(())
}
