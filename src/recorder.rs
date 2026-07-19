//! 録音セッションの開始・停止と MP3 ファイルへの書き出し。
//!
//! トレイ／UI から独立した録音モジュール。1 回の録音セッションは複数の音源を持ちうる:
//!   - マイク音源（全 OS 共通）: `cpal` で既定入力デバイスから PCM を取得し、`mic.mp3` に書く。
//!   - システム音源（macOS のみ）: ScreenCaptureKit でスピーカー出力を取得し、`system.mp3` に書く
//!     （`crate::system_audio`）。
//!
//! いずれの音源も、音声コールバック内ではエンコードせず PCM をチャネルへ送るだけにし、エンコードと
//! ファイル書き込みは writer スレッド（`run_writer`）で行う（リアルタイムコールバックを軽く保ち
//! 音飛びを避ける）。音源 1 つ = 1 ファイルの単位で、同じセッションディレクトリへ書き出す。

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample};
use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, MonoPcm, Quality};

/// スレッドを跨ぐためのエラー型（`Send + Sync` が必要）。
pub(crate) type RecordError = Box<dyn std::error::Error + Send + Sync>;

/// エンコードのビットレート。音声録音として十分な品質と容量のバランスで 128 kbps。
const BITRATE: Bitrate = Bitrate::Kbps128;
/// エンコード品質（0=最良〜9=最低）。速度と品質のバランスで Good。
const QUALITY: Quality = Quality::Good;
/// マイク音源の出力ファイル名。録音セッションのディレクトリ内に固定名で置く
/// （システム音声は `system.mp3`、文字起こし結果なども同じディレクトリへ）。
const MIC_FILENAME: &str = "mic.mp3";

/// 実行中の録音セッション。マイク音源は必須、システム音源（macOS）は任意。
///
/// `cpal::Stream`（マイク）や `SCStream`（システム）は `Send` でないため、メインスレッド上でのみ
/// 保持する。`stop()` で各音源を止め、writer スレッドの flush 完了を待ってファイルを確定する。
pub struct Recorder {
    /// マイク音源（必須）。開始に失敗するとセッション自体が始まらない。
    mic: MicSource,
    /// 録音開始時刻。経過時間表示の基準。システム時計の変更に影響されないよう `Instant` を使う。
    started_at: Instant,
    /// システム音声音源（macOS のみ・任意）。権限拒否や取得失敗時は `None`（マイクのみで続行）。
    #[cfg(target_os = "macos")]
    system: Option<crate::system_audio::SystemAudioSource>,
}

impl Recorder {
    /// 録音セッションを開始する。`session_dir`（`<保存先>/<日時>`）配下に音源ごとのファイルを作る。
    ///
    /// マイクは必須で、失敗すればセッションを開始しない。システム音声（macOS）は任意で、開始に
    /// 失敗してもログを残してマイクのみで続行する（アプリ・常駐は巻き込まない）。
    pub fn start(session_dir: &Path) -> Result<Self, RecordError> {
        let mic = MicSource::start(session_dir)?;
        let started_at = Instant::now();

        #[cfg(target_os = "macos")]
        let system = match crate::system_audio::SystemAudioSource::start(session_dir) {
            Ok(system) => Some(system),
            Err(err) => {
                eprintln!(
                    "Could not start system-audio recording, continuing with mic only: {err}"
                );
                None
            }
        };

        Ok(Self {
            mic,
            started_at,
            #[cfg(target_os = "macos")]
            system,
        })
    }

    /// 録音開始からの経過時間。表示更新側がメニューバーの経過時間表示に使う。
    pub fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    /// 録音を停止し、各音源のファイルを確定して、保存できたパスの一覧を返す。
    ///
    /// 1 音源の停止・保存が失敗しても他の音源は止め、握りつぶさずログに残す（常駐は継続）。
    pub fn stop(self) -> Vec<PathBuf> {
        let Self {
            mic,
            started_at: _,
            #[cfg(target_os = "macos")]
            system,
        } = self;

        let mut saved = Vec::new();
        match mic.stop() {
            Ok(path) => saved.push(path),
            Err(err) => eprintln!("Failed to stop and save the mic recording: {err}"),
        }
        #[cfg(target_os = "macos")]
        if let Some(system) = system {
            match system.stop() {
                Ok(path) => saved.push(path),
                Err(err) => eprintln!("Failed to stop and save the system audio: {err}"),
            }
        }
        saved
    }
}

/// マイク音源（`cpal` の既定入力デバイス）。保持している間だけ録音が続く。
struct MicSource {
    /// drop でコールバックが止まり、サンプル送信側も閉じる。
    stream: cpal::Stream,
    /// エンコード・書き込みを行う writer スレッド。
    writer: JoinHandle<Result<(), RecordError>>,
    /// 出力先 MP3 ファイルのパス。
    path: PathBuf,
}

impl MicSource {
    /// 既定の入力デバイスからマイク録音を開始し、`session_dir` 内に `mic.mp3` を作る
    /// （セッションディレクトリが無ければ作成する。同じセッションの他音源もここへ書く）。
    fn start(session_dir: &Path) -> Result<Self, RecordError> {
        // session_dir は設定の保存先（手編集されうる信頼境界外）から組み立てた値だが、ユーザー
        // 自身が選んだ保存先配下であり、ここではそのまま使う（パスの正当性は設定 UI 側の責務）。
        create_session_dir(session_dir)?;

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or("No default input device found")?;
        let supported = device.default_input_config()?;
        let sample_format = supported.sample_format();
        let config: cpal::StreamConfig = supported.config();
        // cpal 0.18 では SampleRate は u32 の型エイリアス。
        let sample_rate = config.sample_rate;
        let input_channels = config.channels;

        // 未対応のサンプル形式なら、ファイルやスレッドを作る前に弾く（副作用を残さない）。
        if !matches!(
            sample_format,
            SampleFormat::F32 | SampleFormat::I16 | SampleFormat::U16
        ) {
            return Err(format!("Unsupported sample format: {sample_format:?}").into());
        }
        // LAME はモノラル(1)/ステレオ(2)のみ対応。入力デバイスは 3ch 以上を報告することがある
        // （例: ブラウザ/会議アプリが VoiceProcessingIO でマイクを開いている間や、複数マイクを
        // 束ねたデバイス）。その場合はモノラルへダウンミックスして録音する。1/2ch はそのまま。
        let output_channels: u16 = match input_channels {
            0 => return Err("Unsupported channel count: 0".into()),
            1 | 2 => input_channels,
            _ => 1,
        };

        let path = session_dir.join(MIC_FILENAME);
        // 録音は機微データのため、所有者のみ読み書き可で作成する（Unix）。
        let file = create_recording_file(&path)?;

        // 音声コールバック → writer スレッドへ PCM を渡す無制限チャネル。コールバックを
        // ブロックさせないため上限を設けない。通常 writer は入力レートに追いつくが、ディスク
        // 滞留時はメモリが増える。問題が出たら上限付き＋超過時ドロップへ見直す（TODO）。
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        let writer = std::thread::Builder::new()
            .name("openshoki-mic-writer".to_owned())
            .spawn(move || run_writer(rx, sample_rate, output_channels, file))?;

        // ストリームを構築して再生開始する。冒頭で対応形式に絞っているため match は網羅済み。
        // ストリームは入力デバイス本来の channel 数（config.channels）で開き、コールバック内で
        // 必要ならモノラルへダウンミックスして writer（output_channels）へ渡す。
        let built = match sample_format {
            SampleFormat::F32 => build_input_stream::<f32>(&device, config, output_channels, tx),
            SampleFormat::I16 => build_input_stream::<i16>(&device, config, output_channels, tx),
            SampleFormat::U16 => build_input_stream::<u16>(&device, config, output_channels, tx),
            other => unreachable!("start() already restricted to supported formats: {other:?}"),
        };
        // 構築・再生に失敗したら副作用を残さない（writer を終了・回収し、作成済みの空ファイルを消す）。
        // tx を落としてチャネルを閉じないと writer の recv が閉じず join が返らないため、先に
        // tx を手放す（build 失敗時は build_input_stream 内で tx が drop 済み、play 失敗時は
        // stream を drop して tx を落とす）。
        let stream = match built {
            Ok(stream) => stream,
            Err(err) => {
                discard_partial_recording(writer, &path);
                return Err(err);
            }
        };
        if let Err(err) = stream.play() {
            drop(stream);
            discard_partial_recording(writer, &path);
            return Err(err.into());
        }

        Ok(Self {
            stream,
            writer,
            path,
        })
    }

    /// マイク録音を停止し、ファイルを確定して保存先パスを返す。
    ///
    /// ストリームを止めてコールバックとサンプル送信側を閉じてから、writer スレッドの
    /// flush 完了を待つ（末尾フレームを取りこぼさない順序）。
    fn stop(self) -> Result<PathBuf, RecordError> {
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
            .map_err(|_| "The mic recording writer thread panicked")??;
        Ok(path)
    }
}

/// 指定のサンプル形式で入力ストリームを構築する。コールバックは PCM を i16 に変換し、必要なら
/// モノラルへダウンミックスして送るだけ（エンコードは writer スレッドに任せる）。
fn build_input_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    output_channels: u16,
    tx: Sender<Vec<i16>>,
) -> Result<cpal::Stream, RecordError>
where
    T: SizedSample,
    i16: FromSample<T>,
{
    let input_channels = config.channels as usize;
    let output_channels = output_channels as usize;
    let err_fn = |err| eprintln!("An error occurred on the input stream: {err}");
    let stream = device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let pcm = to_i16_pcm(data, input_channels, output_channels);
            // writer 終了済み（エラー等）なら送信は失敗するが、その原因は stop() の join() で
            // 受け取るため、取りこぼしたサンプルはここでは捨てる。
            let _ = tx.send(pcm);
        },
        err_fn,
        None,
    )?;
    Ok(stream)
}

/// インターリーブ PCM を i16 に変換する。`input_channels == output_channels` ならそのまま変換し、
/// それ以外（入力が 3ch 以上で `output_channels == 1`）は各フレームの全チャンネルを平均して
/// モノラルへダウンミックスする（LAME はモノラル/ステレオのみ対応のため）。
fn to_i16_pcm<T>(data: &[T], input_channels: usize, output_channels: usize) -> Vec<i16>
where
    T: Copy,
    i16: FromSample<T>,
{
    if input_channels == output_channels || input_channels == 0 {
        return data.iter().map(|&s| i16::from_sample(s)).collect();
    }
    // モノラルへダウンミックス。フレーム（input_channels サンプル）ごとに平均を取る。
    // i32 で合算してからチャンネル数で割り、桁あふれを避ける。
    data.chunks_exact(input_channels)
        .map(|frame| {
            let sum: i32 = frame.iter().map(|&s| i16::from_sample(s) as i32).sum();
            (sum / input_channels as i32) as i16
        })
        .collect()
}

/// writer スレッド本体。チャネルから受け取った PCM を MP3 にエンコードして書き込み、
/// チャネルが閉じたら flush してファイルを確定する。マイク・システム両音源で共用する。
pub(crate) fn run_writer(
    rx: Receiver<Vec<i16>>,
    sample_rate: u32,
    channels: u16,
    file: File,
) -> Result<(), RecordError> {
    let mut encoder = {
        let mut builder = Builder::new().ok_or("Failed to create the LAME encoder builder")?;
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
        // channels は呼び出し側で 1/2 のみに絞っている。
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

/// 録音セッションのディレクトリを作成する。録音は機微データのため、Unix では所有者のみ
/// アクセス可(0700)で作る（ファイルを 0600 にしても、ディレクトリが緩いと中身の一覧が
/// 他ユーザーに漏れる）。`mode` は新規作成するディレクトリにのみ効き、既存ディレクトリの
/// 権限は変えない。親が無ければ再帰的に作る（`create_dir_all` 相当）。
fn create_session_dir(session_dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(session_dir)
}

/// 録音ファイルを作成する。録音は機微データのため、Unix では所有者のみ読み書き可(0600)で作る。
/// マイク・システム両音源で共用する。
pub(crate) fn create_recording_file(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// 開始失敗時の後始末: writer スレッドを終了・回収し、作成済みの空ファイルを消す。
/// いずれの失敗も握りつぶさずログに残す。呼び出し側は **先にチャネルを閉じてから** 呼ぶこと
/// （でないと writer の `recv` が終わらず `join` が返らない）。マイク・システム両音源で共用する。
pub(crate) fn discard_partial_recording(writer: JoinHandle<Result<(), RecordError>>, path: &Path) {
    match writer.join() {
        Ok(Ok(())) => {}
        Ok(Err(err)) => {
            eprintln!("Error while shutting down the writer after a failed recording start: {err}")
        }
        Err(_) => eprintln!("The recording writer thread panicked (during failed start)"),
    }
    if let Err(err) = std::fs::remove_file(path) {
        eprintln!("Failed to delete the empty file after a failed start: {err}");
    }
}

#[cfg(test)]
mod tests {
    use super::to_i16_pcm;

    #[test]
    fn passthrough_when_input_equals_output() {
        // 入力と出力のチャンネル数が同じならそのまま i16 化する（T=i16 は恒等変換）。
        let data: [i16; 4] = [10, -20, 30, -40];
        assert_eq!(to_i16_pcm(&data, 2, 2), vec![10, -20, 30, -40]);
        assert_eq!(to_i16_pcm(&data, 1, 1), vec![10, -20, 30, -40]);
    }

    #[test]
    fn downmixes_multichannel_to_mono_by_averaging() {
        // 3ch → mono。フレームごとに全チャンネルの平均を取る。
        let data: [i16; 6] = [3, 6, 9, 30, 60, 90];
        assert_eq!(to_i16_pcm(&data, 3, 1), vec![6, 60]);
    }

    #[test]
    fn downmix_averages_with_i32_accumulation() {
        // 大きな値でも i32 で合算するため桁あふれしない（(32767+32767+(-32768))/3 = 10922）。
        let data: [i16; 3] = [i16::MAX, i16::MAX, i16::MIN];
        assert_eq!(to_i16_pcm(&data, 3, 1), vec![10922]);
    }

    #[test]
    fn downmix_drops_trailing_partial_frame() {
        // フレーム境界に満たない端数は無視する（通常はフレーム整列しているため発生しない）。
        let data: [i16; 4] = [3, 6, 9, 12];
        assert_eq!(to_i16_pcm(&data, 3, 1), vec![6]);
    }
}
