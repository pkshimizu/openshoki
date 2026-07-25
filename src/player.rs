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

    /// 再生対象を手放して「何もロードされていない」状態へ戻す（キューを空にし、対象パス・
    /// 全体長を破棄して一時停止にする）。対象を保持したまま先頭へ戻す `stop` と対で、こちらは
    /// **対象そのものを捨てる**。呼ぶのは「再生する対象が無くなった／別の対象に切り替わる」
    /// タイミング（`load` も冒頭で呼ぶ）。
    ///
    /// 手放した後は `is_loaded` が false、`position` は 0、`duration` は `None`、`is_playing` は
    /// false を返す（`position` の doc も参照）。削除の前に呼べば、削除済みファイルを
    /// `play_pause` / `seek` の開き直し経路が参照しなくなる。
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

    /// 指定位置へシークする（文字起こしのセグメントクリック・シークバーの操作で使う）。
    /// 再生/一時停止の状態は変えない（再生中はその位置から続行、一時停止中は位置だけ移動）。
    ///
    /// シークできない（`try_seek` 非対応・デコーダのエラー）ときは `Err` を返す。呼び出し側は
    /// それを見て表示（進捗バー・時刻・ハイライト）を**その場で**進めない。ただし縮退は完全では
    /// ないので、次を前提にすること:
    ///
    /// - rodio は `try_seek` が失敗しても内部の位置を要求値へ書くため、直後の再生 tick が
    ///   シークできなかった位置を表示しうる（ソースが生きていれば数 ms で実位置へ戻る）。
    /// - デコーダが目的位置の手前でエラーを返す（破損・途中で切れた MP3 など）と、そのソースは
    ///   以後パケットを返せずキューが空になり**再生が止まる**（Play で頭から鳴らし直せる）。
    /// - 終端・停止後でキューが空のときは対象ファイルを積み直してからシークする。そこで失敗したら
    ///   積み直しを巻き戻すため、位置は先頭へ落ちる（終端で止まっていたのと同じく、Play で頭から
    ///   鳴らし直せる状態）。
    ///
    /// 対象ファイルを開き直して目的位置まで読み飛ばすフォールバック（`Source::skip_duration`）は
    /// **持たない**。読み飛ばしは目的位置までの全サンプルを同期デコードするため長い録音では
    /// 数百 ms 級になり、Slint のコールバック（＝イベントループ）上で呼ぶと UI が固まる。`try_seek`
    /// も MP3 ではフレームヘッダを走査するので目的位置に比例するが、ms 級で桁が違う
    /// （計測条件と実測値は PR #95 を参照）。
    pub fn seek(&self, pos: Duration) -> Result<(), Box<dyn std::error::Error>> {
        // 積み直したかを覚えておき、シーク失敗時に巻き戻す。終端後は一時停止状態でないため、
        // 積んだままにすると「シークに失敗したのに先頭から鳴り出す」ことになる。
        let mut appended = false;
        if self.player.empty() {
            self.append_from_start();
            // 積み直せない（未ロード・開き直し失敗。理由は append_from_start がログする）なら
            // シーク対象が無い。rodio はキューが空のとき try_seek が何もせず Ok を返すため、
            // ここで明示的に Err にする。
            if self.player.empty() {
                return Err("no audio is queued for playback".into());
            }
            appended = true;
        }
        if let Err(err) = self.player.try_seek(pos) {
            if appended {
                // clear() はキューを空にして一時停止にする。空キューでは再生フラグが音にも
                // is_playing() にも出ないため、これでシークを試す前と同じ「鳴っていない」状態に戻る。
                self.player.clear();
            }
            return Err(err.into());
        }
        Ok(())
    }

    /// 現在の再生位置。対象を手放している間（`is_loaded` が false）は 0 を返す。
    ///
    /// rodio の `clear()` は**キューが空のときは内部位置を戻さない**（空にする対象が無いため）。
    /// 終端まで再生した後に手放すと前の対象の位置が残り、表示（進捗バー・時刻・セグメントの
    /// ハイライト）がそれで駆動されてしまうので、ここで 0 に揃える。
    ///
    /// 同じ癖は `load` / `stop` の積み直し直後にも残る（`path` が `Some` なので上のガードは効かず、
    /// 積んだソースが最初にポーリングされるまでの数 ms は前の対象の位置が返る）。次のティックで
    /// 実位置へ戻るため、表示が一瞬ずれるだけの既知の縮退として許容している。
    pub fn position(&self) -> Duration {
        if self.path.is_none() {
            return Duration::ZERO;
        }
        self.player.get_pos()
    }

    /// 再生対象がロードされているか（`load` 成功後 `unload` まで true。`load` 失敗時は false）。
    /// false のとき `play_pause` は何も鳴らさず、`seek` は必ず `Err` を返すため、呼び出し側は
    /// 音に依存しない縮退（表示だけ更新する等）を選べる。
    pub fn is_loaded(&self) -> bool {
        self.path.is_some()
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

#[cfg(test)]
mod tests {
    use std::num::NonZero;
    use std::time::Duration;

    use rodio::Source;

    /// テスト用トーン。振幅は正規化の対象にならない普通の音量（ピーク -12dBFS 相当）。
    const TONE_HZ: f32 = 440.0;
    const TONE_AMPLITUDE: f32 = 0.25;
    const FIXTURE_SECS: u32 = 3;

    /// `seek` が読み飛ばしフォールバックを持たない前提（アプリが出力する MP3 は全体長が分かり、
    /// `try_seek` が成立する）を固定する。ここが崩れると全てのシークが `Err` になり、クリックしても
    /// 再生位置が動かない（機能が事実上死ぬ）ため、依存（rodio / symphonia）の更新やデコーダの
    /// 開き方の変更で気づけるようにする。
    ///
    /// `AudioPlayer` は出力デバイスを開くため CI では作れないが、この前提は `open_decoder` が返す
    /// `Decoder` 単体で検証できるのでデバイス不要で決定的に確認できる。
    #[test]
    fn open_decoder_reports_duration_and_supports_seeking() {
        // 一時ディレクトリはプロセス固有にし、前回の残骸を消してから作る（他のテストと同じ作法）。
        let dir = std::env::temp_dir().join(format!("openshoki-seek-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the temp dir should succeed");

        // 再生対象になりうる組み合わせ（モノラル/ステレオ、44.1k/48k）で確認する。ビットレート・
        // 品質は `mixdown::encode_mp3` の設定（録音出力と揃えてある）。
        for (channels, sample_rate) in [(1u16, 44_100u32), (2, 48_000)] {
            let samples = sample_rate * FIXTURE_SECS * u32::from(channels);
            let pcm: Vec<i16> = (0..samples)
                .map(|i| {
                    // ステレオはインターリーブなので、フレーム単位で時間を進める。
                    let t = (i / u32::from(channels)) as f32 / sample_rate as f32;
                    ((t * TONE_HZ * std::f32::consts::TAU).sin() * TONE_AMPLITUDE * i16::MAX as f32)
                        as i16
                })
                .collect();
            let mp3 = crate::mixdown::encode_mp3(
                &pcm,
                NonZero::new(channels).expect("the channel count is non-zero"),
                NonZero::new(sample_rate).expect("the sample rate is non-zero"),
            )
            .expect("encoding the test tone should succeed");
            let path = dir.join(format!("tone-{channels}ch-{sample_rate}hz.mp3"));
            std::fs::write(&path, &mp3).expect("writing the test MP3 should succeed");

            let mut source =
                super::open_decoder(&path).expect("opening the test MP3 should succeed");
            assert!(
                source.total_duration().is_some(),
                "the duration must be known ({channels}ch/{sample_rate}Hz), \
                 otherwise the seek bar degrades to display-only"
            );
            // 終端より手前（3 秒のうち 2 秒）へシークする。
            assert!(
                source.try_seek(Duration::from_secs(2)).is_ok(),
                "seeking must be supported ({channels}ch/{sample_rate}Hz), \
                 otherwise every seek fails and the playback position never moves"
            );
        }

        let _ = std::fs::remove_dir_all(&dir);
    }
}
