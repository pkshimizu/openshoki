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
        // recording_dir は設定由来（手編集されうる信頼境界外）だが、ユーザー自身が選んだ保存先
        // であり、ここではそのまま使う（パスの正当性は設定 UI 側の責務）。
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
        // 録音は機微データのため、所有者のみ読み書き可で作成する（Unix）。
        let file = create_recording_file(&path)?;

        // 音声コールバック → writer スレッドへ PCM を渡す無制限チャネル。コールバックを
        // ブロックさせないため上限を設けない。通常 writer は入力レートに追いつくが、ディスク
        // 滞留時はメモリが増える。問題が出たら上限付き＋超過時ドロップへ見直す（TODO）。
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        let writer = std::thread::Builder::new()
            .name("openshoki-mp3-writer".to_owned())
            .spawn(move || run_writer(rx, sample_rate, channels, file))?;

        // ストリームを構築して再生開始する。冒頭で対応形式に絞っているため match は網羅済み。
        let built = match sample_format {
            SampleFormat::F32 => build_input_stream::<f32>(&device, config, tx),
            SampleFormat::I16 => build_input_stream::<i16>(&device, config, tx),
            SampleFormat::U16 => build_input_stream::<u16>(&device, config, tx),
            other => unreachable!("start() の冒頭で対応形式に絞り済み: {other:?}"),
        };
        // 構築・再生に失敗したら副作用を残さない（作成済みの空ファイルを消し、writer を終了・join）。
        // stream を先に drop して tx を落とさないと、writer の recv が閉じず join が返らない。
        let stream = match built {
            Ok(stream) => stream,
            Err(err) => {
                let _ = std::fs::remove_file(&path);
                let _ = writer.join();
                return Err(err);
            }
        };
        if let Err(err) = stream.play() {
            drop(stream);
            let _ = std::fs::remove_file(&path);
            let _ = writer.join();
            return Err(err.into());
        }

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
            // LAME 入力に合わせて i16 へ変換し、エンコードは writer スレッドに任せる。
            let pcm: Vec<i16> = data.iter().map(|&s| i16::from_sample(s)).collect();
            // writer 終了済み（エラー等）なら送信は失敗するが、その原因は stop() の join() で
            // 受け取るため、取りこぼしたサンプルはここでは捨てる。
            let _ = tx.send(pcm);
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

    // 末尾フレームを書き出してファイルを確定する。flush 用に LAME が要求する下限
    // （max_required_buffer_size(0) が内部的に約 7200 バイトを返す）を確保する。
    mp3.clear();
    mp3.reserve(mp3lame_encoder::max_required_buffer_size(0));
    encoder.flush_to_vec::<FlushNoGap>(&mut mp3)?;
    writer.write_all(&mp3)?;
    writer.flush()?;
    Ok(())
}

/// 録音ファイルを作成する。録音は機微データのため、Unix では所有者のみ読み書き可(0600)で作る。
fn create_recording_file(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}
