//! 録音セッションの音声再生。`rodio` で既定の出力デバイスへ、**1 つの音声ファイルをストリーミング
//! 再生**する（全 PCM を先読みしない）。
//!
//! 再生対象は呼び出し側（`recordings::RecordingSession::playback_path`）が決める: 両音源のセッションは
//! 録音後に生成された `mix.mp3`（`src/mixdown.rs`）、単一音源のセッションは `mic.mp3` / `system.mp3`
//! そのもの。いずれも 1 ファイルなので、選択時に重いデコード＋ミックスをせず即座に再生を準備できる。
//!
//! `rodio` の再生キューはソースを消費し、終端に達すると空になる。終端後や停止後に再生し直せるよう、
//! 再生対象パスを保持し、`Decoder` を作り直して積み直す（`Decoder` はストリーミングなのでメモリは
//! ファイル全体を展開しない）。
//!
//! 出力ストリーム（`cpal`）は録音側と同じくメインスレッドで保持する（`!Send` を跨がせない）。
//! デバイス生成・ファイル読み込みの失敗は `Result` で返し、呼び出し側はログして常駐を続ける
//! （`docs/rules/error-handling.md`）。

use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player, Source};

/// 再生の制御ハンドル。既定出力デバイスへ接続した状態を保持する。
///
/// `_sink`（`MixerDeviceSink`）は drop すると出力ストリームが止まるため、再生中は保持し続ける。
/// `cpal::Stream` を内包し `!Send` の可能性があるため、メインスレッド上でのみ扱う。
pub struct AudioPlayer {
    /// 出力ストリーム。保持のみ（drop で停止）。
    _sink: MixerDeviceSink,
    /// 再生キュー（旧 Sink 相当）。play/pause/seek/位置取得を担う。
    player: Player,
    /// 現在の再生対象ファイル。終端後・停止後に `Decoder` を作り直すため保持する。
    path: Option<PathBuf>,
    /// 現在ロード中ファイルの全体長（分かる場合）。
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
            path: None,
            duration: None,
        })
    }

    /// 再生対象を手放す（キューを空にし、対象パス・全体長を破棄して一時停止にする）。
    /// セッション削除の前に呼び、削除済みファイルを `play_pause` / `seek` の開き直し経路が
    /// 参照しないようにする。
    pub fn unload(&mut self) {
        self.player.clear();
        self.path = None;
        self.duration = None;
        self.player.pause();
    }

    /// 再生対象ファイルをロードして再生準備する（停止状態でセット。`play_pause` で再生開始）。
    /// 失敗時は前のセッションの状態を残さない（stale な `path` が残ると、後続の seek /
    /// play_pause が前のセッションの音声を開き直し、表示中のトランスクリプトと食い違う）。
    pub fn load(&mut self, path: &Path) -> Result<(), Box<dyn std::error::Error>> {
        self.unload();
        let source = open_decoder(path)?;
        self.duration = source.total_duration();
        self.path = Some(path.to_path_buf());
        self.player.append(source);
        self.player.pause();
        Ok(())
    }

    /// 再生と一時停止をトグルする。終端に達して（または停止後で）キューが空なら、対象ファイルを
    /// 頭から開き直して再生する。
    pub fn play_pause(&self) {
        if self.player.empty() {
            self.append_from_start();
            if !self.player.empty() {
                self.player.play();
            }
        } else if self.player.is_paused() {
            self.player.play();
        } else {
            self.player.pause();
        }
    }

    /// 停止して先頭へ戻す。キューを作り直して対象ファイルを頭から積み直し、一時停止状態にする
    /// （`play_pause` で頭から再生できる）。
    pub fn stop(&self) {
        self.player.clear();
        self.append_from_start();
        self.player.pause();
    }

    /// 対象ファイルを頭から開き直してキューへ積む（再生状態は変えない）。失敗はログして続行。
    fn append_from_start(&self) {
        let Some(path) = &self.path else {
            return;
        };
        match open_decoder(path) {
            Ok(source) => self.player.append(source),
            Err(err) => eprintln!("Failed to reopen the audio for playback: {err}"),
        }
    }

    /// 指定位置へシークする（文字起こしのセグメントクリックで使う）。再生/一時停止の状態は
    /// 変えない（再生中はその位置から続行、一時停止中は位置だけ移動）。
    ///
    /// 終端・停止後でキューが空なら、まず対象ファイルを積み直してからシークする。`try_seek` が
    /// 効かない（byte_len 不明などでシーク非対応の）ときは、対象ファイルを開き直して先頭を読み
    /// 飛ばすフォールバックにする（この経路では再生位置表示の基準が 0 に戻りうる）。
    pub fn seek(&self, pos: Duration) {
        if self.player.empty() {
            self.append_from_start();
        }
        if let Err(err) = self.player.try_seek(pos) {
            eprintln!("Seeking via try_seek failed; re-decoding from the position: {err}");
            self.append_skipping(pos);
        }
    }

    /// 対象ファイルを開き直し、先頭 `pos` を読み飛ばしてキューへ積み直す（`try_seek` 非対応時の
    /// フォールバック）。失敗はログして続行。
    fn append_skipping(&self, pos: Duration) {
        let Some(path) = &self.path else {
            return;
        };
        match open_decoder(path) {
            Ok(source) => {
                self.player.clear();
                self.player.append(source.skip_duration(pos));
            }
            Err(err) => eprintln!("Failed to reopen the audio for seeking: {err}"),
        }
    }

    /// 現在の再生位置。
    pub fn position(&self) -> Duration {
        self.player.get_pos()
    }

    /// ロード中ファイルの全体長（分かる場合）。
    pub fn duration(&self) -> Option<Duration> {
        self.duration
    }

    /// 再生中か（一時停止でなく、キューが空でない）。
    pub fn is_playing(&self) -> bool {
        !self.player.is_paused() && !self.player.empty()
    }
}

/// ファイルをストリーミングデコードする `Decoder` を開く。`File` 用 `TryFrom` は BufReader 化と
/// byte_len 設定を行い、MP3 でも `total_duration` / シークが有効になりやすい。
fn open_decoder(path: &Path) -> Result<Decoder<BufReader<File>>, Box<dyn std::error::Error>> {
    let file = File::open(path)?;
    Ok(Decoder::try_from(file)?)
}
