//! macOS のシステム音声（スピーカー出力）を ScreenCaptureKit で取得する音源。
//!
//! 録音セッションの 2 つ目の音源（`crate::recorder` 参照）。音声のみのキャプチャを構成し、
//! 届くオーディオサンプルバッファを i16 インターリーブ PCM に変換して、マイクと同じ
//! writer スレッド（`crate::recorder::run_writer`）で `system.mp3` に書き出す。
//!
//! macOS 限定。`#[cfg(target_os = "macos")]` で隔離する（他 OS は当面システム音源を持たない）。
//! 画面収録権限（TCC）が無い／取得に失敗した場合は開始時にエラーを返し、呼び出し側
//! （`Recorder::start`）がマイクのみで続行する。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use screencapturekit::cm::AudioBufferList;
use screencapturekit::prelude::*;
use screencapturekit::stream::configuration::audio::{AudioChannelCount, AudioSampleRate};

use crate::recorder::{RecordError, create_recording_file, discard_partial_recording, run_writer};

/// システム音源の出力ファイル名（セッションディレクトリ内に固定名で置く）。
const SYSTEM_FILENAME: &str = "system.mp3";
/// 取得フォーマット。ScreenCaptureKit が対応する 48kHz / ステレオ。LAME もこの値で初期化する。
const SAMPLE_RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
/// 画面は使わないが ScreenCaptureKit は寸法を要求するため、最小の寸法を与える
/// （Screen ハンドラは登録しないので画面フレームは消費されない）。
const MIN_DIMENSION: u32 = 2;

/// ハンドラと共有する送信側。`Option` を `None` にすることでチャネルを閉じ、writer を終わらせる。
type SharedSender = Arc<Mutex<Option<Sender<Vec<i16>>>>>;

/// 実行中のシステム音声音源。保持している間だけキャプチャが続く。
pub(crate) struct SystemAudioSource {
    /// ScreenCaptureKit のストリーム。保持し続ける必要がある。
    stream: SCStream,
    /// 停止時に `None` にしてチャネルを閉じる。`SCStream` はハンドラを drop 時に同期解放しない
    /// ため、チャネルの閉鎖を「ハンドラの drop」に頼らず、この送信側を明示的に落として行う。
    sender: SharedSender,
    /// エンコード・書き込みを行う writer スレッド。
    writer: JoinHandle<Result<(), RecordError>>,
    /// 出力先 MP3 ファイルのパス。
    path: PathBuf,
}

/// ScreenCaptureKit のオーディオ出力ハンドラ。ディスパッチキューから複数スレッドで呼ばれうる
/// ため `Send + Sync` が要る（`SharedSender` は満たす）。コールバックは変換して送るだけにする。
struct SystemAudioHandler {
    sender: SharedSender,
}

impl SCStreamOutputTrait for SystemAudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
        if of_type != SCStreamOutputType::Audio {
            return;
        }
        let Some(list) = sample.audio_buffer_list() else {
            return;
        };
        let pcm = to_interleaved_i16(&list, CHANNELS as usize);
        if pcm.is_empty() {
            return;
        }
        // 停止後（sender = None）は送らない。writer 終了済みなら送信失敗するが、その原因は
        // stop() の join() で受け取るため、ここでは捨ててよい。
        if let Ok(guard) = self.sender.lock()
            && let Some(tx) = guard.as_ref()
        {
            let _ = tx.send(pcm);
        }
    }
}

impl SystemAudioSource {
    /// システム音声のキャプチャを開始し、`session_dir` 内に `system.mp3` を作る。
    /// セッションディレクトリはマイク音源が先に作成済みである前提（同じセッションへ書く）。
    pub(crate) fn start(session_dir: &Path) -> Result<Self, RecordError> {
        let path = session_dir.join(SYSTEM_FILENAME);
        // 録音は機微データのため、所有者のみ読み書き可で作成する（Unix）。
        let file = create_recording_file(&path)?;

        let (tx, rx) = std::sync::mpsc::channel::<Vec<i16>>();
        let writer = std::thread::Builder::new()
            .name("openshoki-system-writer".to_owned())
            .spawn(move || run_writer(rx, SAMPLE_RATE, CHANNELS, file))?;
        let sender: SharedSender = Arc::new(Mutex::new(Some(tx)));

        // 共有可能コンテンツ → 先頭ディスプレイのフィルタ → 音声のみ構成、の順に組む。
        // 失敗時は空ファイル・writer を後始末してから返す（副作用を残さない）。
        let content = match SCShareableContent::get() {
            Ok(content) => content,
            Err(err) => {
                abort(&sender, writer, &path);
                return Err(format!("共有可能コンテンツの取得に失敗した: {err}").into());
            }
        };
        let displays = content.displays();
        let Some(display) = displays.first() else {
            abort(&sender, writer, &path);
            return Err("ディスプレイが見つからない".into());
        };
        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();
        let config = SCStreamConfiguration::new()
            .with_width(MIN_DIMENSION)
            .with_height(MIN_DIMENSION)
            .with_captures_audio(true)
            .with_sample_rate(AudioSampleRate::Rate48000)
            .with_channel_count(AudioChannelCount::Stereo)
            // 自プロセス（openshoki）の音は拾わない（フィードバック防止）。
            .with_excludes_current_process_audio(true);

        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            SystemAudioHandler {
                sender: Arc::clone(&sender),
            },
            SCStreamOutputType::Audio,
        );
        if let Err(err) = stream.start_capture() {
            abort(&sender, writer, &path);
            return Err(format!("システム音声のキャプチャ開始に失敗した: {err}").into());
        }

        Ok(Self {
            stream,
            sender,
            writer,
            path,
        })
    }

    /// キャプチャを停止し、ファイルを確定して保存先パスを返す。
    ///
    /// 「キャプチャ停止 → 送信側を落としてチャネルを閉じる → writer の flush 完了を待つ」の順で、
    /// マイク音源の「停止 → flush → 確定」と揃える。
    pub(crate) fn stop(self) -> Result<PathBuf, RecordError> {
        let Self {
            stream,
            sender,
            writer,
            path,
        } = self;
        if let Err(err) = stream.stop_capture() {
            eprintln!("システム音声のキャプチャ停止に失敗した: {err}");
        }
        // 送信側を落としてチャネルを閉じる（ハンドラ保持に依存せず writer を終わらせる）。
        close_sender(&sender);
        drop(stream);
        writer
            .join()
            .map_err(|_| "システム音声書き込みスレッドがパニックした")??;
        Ok(path)
    }
}

/// 開始失敗時の後始末。送信側を落として writer を終わらせ、作成済みの空ファイルを消す。
fn abort(sender: &SharedSender, writer: JoinHandle<Result<(), RecordError>>, path: &Path) {
    close_sender(sender);
    discard_partial_recording(writer, path);
}

/// 送信側を落としてチャネルを閉じる。Mutex が poison していてもガードを取り出して必ず閉じる
/// （閉じないと writer の `recv` が終わらず `join`／後始末がハングするため）。
fn close_sender(sender: &SharedSender) {
    let mut guard = match sender.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *guard = None;
}

/// `AudioBufferList` を i16 インターリーブ PCM に変換する。ScreenCaptureKit は Float32 で渡す。
/// プレーナ（バッファ数 == チャンネル数）なら各バッファを 1 チャンネルとしてインターリーブし、
/// 単一バッファならそのまま順に変換する。
fn to_interleaved_i16(list: &AudioBufferList, channels: usize) -> Vec<i16> {
    let planes: Vec<Vec<f32>> = list.iter().map(|b| bytes_to_f32(b.data())).collect();
    if planes.is_empty() {
        return Vec::new();
    }
    if planes.len() == channels && channels > 1 {
        // プレーナ。全チャンネルで揃う最小フレーム数までをインターリーブする（取りこぼし防止）。
        let frames = planes.iter().map(Vec::len).min().unwrap_or(0);
        let mut out = Vec::with_capacity(frames * channels);
        for f in 0..frames {
            for plane in &planes {
                out.push(f32_to_i16(plane[f]));
            }
        }
        out
    } else {
        // 単一バッファ（インターリーブ or モノ）。順序のまま変換する。
        planes[0].iter().map(|&s| f32_to_i16(s)).collect()
    }
}

fn bytes_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[cfg(test)]
mod tests {
    use super::{bytes_to_f32, f32_to_i16};

    #[test]
    fn f32_to_i16_saturates_out_of_range() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), i16::MAX);
        assert_eq!(f32_to_i16(-1.0), -i16::MAX);
        // 範囲外は ±1.0 にクランプしてから変換する（i16 の上下限を超えない）。
        assert_eq!(f32_to_i16(2.0), i16::MAX);
        assert_eq!(f32_to_i16(-2.0), -i16::MAX);
    }

    #[test]
    fn bytes_to_f32_reads_little_endian() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&1.0_f32.to_le_bytes());
        bytes.extend_from_slice(&(-0.5_f32).to_le_bytes());
        assert_eq!(bytes_to_f32(&bytes), vec![1.0, -0.5]);
    }

    #[test]
    fn bytes_to_f32_ignores_partial_trailing_bytes() {
        // 4 バイト境界に満たない端数は無視する（f32 は常に 4 バイトのため実際には発生しない）。
        assert!(bytes_to_f32(&[0, 0, 0]).is_empty());
    }
}
