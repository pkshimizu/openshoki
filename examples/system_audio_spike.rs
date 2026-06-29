//! システム音声キャプチャの単体確認用スパイク（macOS 専用）。
//!
//! issue #5 のプラン（`docs/plans/done/20260628-system-audio-capture.md`）のステップ3に対応。
//! ScreenCaptureKit の「音声のみ」キャプチャを数秒動かし、次を実機で確認するための使い捨て
//! バイナリ:
//!   - 画面収録権限のプロンプト／拒否時の挙動
//!   - オーディオサンプルバッファが届くか（受信フレーム数・サンプル数を表示）
//!   - 生成した `system.mp3` を再生してシステム音声が入っているか
//!
//! 実行: 何か音を再生しながら `cargo run --example system_audio_spike`
//! 確認後、本体（`src/recorder.rs`）へ統合する際にこのスパイクは削除する。
//!
//! 注意: ScreenCaptureKit は画面収録権限を要求する。素の `cargo run` ではプロンプトや権限の
//! 効き方が不安定なことがある（配布バンドル化はパッケージング段階で扱う）。権限が無いと
//! サンプルは届かない。

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("このスパイクは macOS 専用です（ScreenCaptureKit を使うため）。");
}

#[cfg(target_os = "macos")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    macos::run()
}

#[cfg(target_os = "macos")]
mod macos {
    use std::fs::File;
    use std::io::{BufWriter, Write};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use mp3lame_encoder::{Bitrate, Builder, FlushNoGap, InterleavedPcm, MonoPcm, Quality};
    use screencapturekit::cm::AudioBufferList;
    use screencapturekit::prelude::*;
    use screencapturekit::stream::configuration::audio::{AudioChannelCount, AudioSampleRate};

    /// 取得する音声フォーマット。ScreenCaptureKit が対応する値（48kHz / ステレオ）。
    const SAMPLE_RATE: i32 = 48_000;
    const CHANNELS: u16 = 2;
    /// キャプチャを回す秒数。
    const CAPTURE_SECS: u64 = 5;
    /// 確認用の出力先（一時ディレクトリ）。
    const OUTPUT_NAME: &str = "openshoki-system-spike.mp3";

    /// オーディオハンドラが持つ状態。ScreenCaptureKit のディスパッチキューから複数スレッドで
    /// 呼ばれうるため `Send + Sync` が要る。LAME エンコーダ・出力・カウンタを Mutex で守る。
    struct SpikeSink {
        encoder: mp3lame_encoder::Encoder,
        writer: BufWriter<File>,
        mp3: Vec<u8>,
        callbacks: u64,
        frames: u64,
    }

    struct AudioHandler {
        sink: Arc<Mutex<SpikeSink>>,
    }

    impl SCStreamOutputTrait for AudioHandler {
        fn did_output_sample_buffer(&self, sample: CMSampleBuffer, of_type: SCStreamOutputType) {
            if of_type != SCStreamOutputType::Audio {
                return;
            }
            let Some(list) = sample.audio_buffer_list() else {
                return;
            };
            // ScreenCaptureKit は Float32 で渡す。プレーナ（チャンネルごとに 1 バッファ）か、
            // 単一インターリーブのどちらか。LAME 入力に合わせて i16 インターリーブへ変換する。
            let pcm = to_interleaved_i16(&list, CHANNELS as usize);
            if pcm.is_empty() {
                return;
            }
            let frames = (pcm.len() / CHANNELS as usize) as u64;

            let mut sink = self.sink.lock().expect("SpikeSink のロックは毒化しない");
            sink.callbacks += 1;
            sink.frames += frames;
            if let Err(err) = sink.encode(&pcm) {
                eprintln!("エンコードに失敗した: {err}");
            }
        }
    }

    impl SpikeSink {
        fn encode(&mut self, pcm: &[i16]) -> Result<(), Box<dyn std::error::Error>> {
            self.mp3.clear();
            self.mp3
                .reserve(mp3lame_encoder::max_required_buffer_size(pcm.len()));
            if CHANNELS == 1 {
                self.encoder.encode_to_vec(MonoPcm(pcm), &mut self.mp3)?;
            } else {
                self.encoder
                    .encode_to_vec(InterleavedPcm(pcm), &mut self.mp3)?;
            }
            self.writer.write_all(&self.mp3)?;
            Ok(())
        }

        fn finalize(mut self) -> Result<(), Box<dyn std::error::Error>> {
            self.mp3.clear();
            self.mp3
                .reserve(mp3lame_encoder::max_required_buffer_size(0));
            self.encoder.flush_to_vec::<FlushNoGap>(&mut self.mp3)?;
            self.writer.write_all(&self.mp3)?;
            self.writer.flush()?;
            Ok(())
        }
    }

    /// AudioBufferList を i16 インターリーブ PCM に変換する。プレーナ（バッファ数 == チャンネル数）
    /// なら各バッファを 1 チャンネルとしてインターリーブ。単一バッファならそのまま i16 化する。
    fn to_interleaved_i16(list: &AudioBufferList, channels: usize) -> Vec<i16> {
        let planes: Vec<Vec<f32>> = list.iter().map(|b| bytes_to_f32(b.data())).collect();
        if planes.is_empty() {
            return Vec::new();
        }
        if planes.len() == channels && channels > 1 {
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

    pub fn run() -> Result<(), Box<dyn std::error::Error>> {
        let output_path: PathBuf = std::env::temp_dir().join(OUTPUT_NAME);
        println!("出力先: {}", output_path.display());

        // LAME エンコーダを取得フォーマットで初期化する。
        let encoder = {
            let mut builder = Builder::new().ok_or("LAME エンコーダのビルダー生成に失敗")?;
            builder.set_num_channels(CHANNELS as u8)?;
            builder.set_sample_rate(SAMPLE_RATE as u32)?;
            builder.set_brate(Bitrate::Kbps128)?;
            builder.set_quality(Quality::Good)?;
            builder.build()?
        };
        let writer = BufWriter::new(File::create(&output_path)?);
        let sink = Arc::new(Mutex::new(SpikeSink {
            encoder,
            writer,
            mp3: Vec::new(),
            callbacks: 0,
            frames: 0,
        }));

        // (1) 共有可能コンテンツ → (2) コンテンツフィルタ（先頭ディスプレイ）。
        // 音声のみが目的だが ScreenCaptureKit はフィルタにディスプレイ等を要求するため設定する。
        let content = SCShareableContent::get()?;
        let displays = content.displays();
        let display = displays.first().ok_or("ディスプレイが見つからない")?;
        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();

        // (3) 音声を有効化した構成。画面は使わないので最小寸法にする（Screen ハンドラは付けない）。
        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_captures_audio(true)
            .with_sample_rate(AudioSampleRate::Rate48000)
            .with_channel_count(AudioChannelCount::Stereo)
            // 自プロセスの音は拾わない（フィードバック防止）。
            .with_excludes_current_process_audio(true);

        // (4) Audio ハンドラを登録して開始。
        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            AudioHandler {
                sink: Arc::clone(&sink),
            },
            SCStreamOutputType::Audio,
        );

        println!("{CAPTURE_SECS} 秒間キャプチャする。何か音を再生してください…");
        stream.start_capture()?;
        std::thread::sleep(Duration::from_secs(CAPTURE_SECS));
        stream.stop_capture()?;

        // 統計を表示してファイルを確定する。
        let (callbacks, frames) = {
            let sink = sink.lock().expect("SpikeSink のロックは毒化しない");
            (sink.callbacks, sink.frames)
        };
        println!("受信コールバック数: {callbacks} / 合計フレーム数: {frames}");
        if callbacks == 0 {
            eprintln!(
                "サンプルが 1 つも届かなかった。画面収録権限が無い可能性が高い\
                 （システム設定 > プライバシーとセキュリティ > 画面収録 を確認）。"
            );
        }

        // Arc から SpikeSink を取り出して flush（他に参照が無ければ成功）。
        match Arc::try_unwrap(sink) {
            Ok(mutex) => {
                let sink = mutex.into_inner().expect("SpikeSink のロックは毒化しない");
                sink.finalize()?;
                println!("system.mp3 を確定した: {}", output_path.display());
            }
            Err(_) => eprintln!("ハンドラの参照が残っており flush できなかった"),
        }
        Ok(())
    }
}
