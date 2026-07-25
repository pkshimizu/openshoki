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
/// 本番（`new`）では常に出力デバイスへ接続済み。デバイスを開かないテスト構築は `connected_to` 参照。
pub struct AudioPlayer {
    /// 出力ストリーム。保持のみ（drop で停止）。テストは出力デバイスを開けないため、
    /// ミキサーへ直接繋ぐ構築（`connected_to`）では `None` になる。
    _sink: Option<MixerDeviceSink>,
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
        // ミキサーの借用と sink の move が重なるため、先に繋いでから出力ストリームを預ける。
        let mut player = Self::connect(sink.mixer());
        player._sink = Some(sink);
        Ok(player)
    }

    /// ミキサーへ繋いだ再生ハンドルを組み立てる（出力ストリームはまだ持たない）。`new` が使うほか、
    /// **テストが出力デバイス無しで `AudioPlayer` を作る入口**でもある（`rodio::mixer::mixer()` の
    /// 片割れを渡す）。
    ///
    /// テストで使うときの前提: rodio の再生位置更新もキューのクリアも「出力側がサンプルを引いた
    /// とき」に進む。実機ではオーディオデバイスがその役をするので、テストは対になる `MixerSource`
    /// を読み進める役を用意すること。引かないと位置が動かず、`unload` / `stop`（内部でクリアの
    /// 完了を待つ）は戻ってこない。ブロックする操作を呼ぶなら、読み進める役は別スレッドに置く。
    fn connect(mixer: &rodio::mixer::Mixer) -> Self {
        let player = Player::connect_new(mixer);
        // ロード前は停止状態にしておく（ロード後の Play で鳴らす）。
        player.pause();
        Self {
            _sink: None,
            player,
            path: None,
            duration: None,
        }
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
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread::JoinHandle;
    use std::time::{Duration, Instant};

    use rodio::mixer::MixerSource;
    use rodio::{ChannelCount, SampleRate, Source};

    use super::AudioPlayer;

    /// テスト用トーン。振幅は正規化の対象にならない普通の音量（ピーク -12dBFS 相当）。
    const TONE_HZ: f32 = 440.0;
    const TONE_AMPLITUDE: f32 = 0.25;
    const FIXTURE_SECS: u32 = 3;
    /// シーク先。フィクスチャの終端（`FIXTURE_SECS`）より手前にすること。
    const SEEK_TARGET: Duration = Duration::from_secs(2);

    /// 疑似オーディオ出力のフォーマット。フィクスチャと揃えて、引いたサンプル数＝音源の再生時間に
    /// 対応させる（モノラルなのでフレーム数＝サンプル数）。
    const OUTPUT_CHANNELS: u16 = 1;
    const OUTPUT_SAMPLE_RATE: u32 = 44_100;
    /// 出力を引く 1 周（ティック）の長さ。rodio が位置を更新する周期（5ms）と同じにして、
    /// 1 ティックで最低 1 回は位置が反映されるようにする。
    const PULL_INTERVAL_MS: u64 = 5;
    const PULL_INTERVAL: Duration = Duration::from_millis(PULL_INTERVAL_MS);
    /// 1 ティックで引くサンプル数（上の定数から導出する。手で書くと周期を変えたときにずれる）。
    const PULL_SAMPLES_PER_TICK: usize =
        (OUTPUT_SAMPLE_RATE as usize * OUTPUT_CHANNELS as usize * PULL_INTERVAL_MS as usize)
            / 1_000;
    /// 別スレッドが引くのを待つ上限。成功時の所要時間には影響しない（落ちるときだけ待つ）。
    const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

    /// テスト用の一時ディレクトリ。アサート失敗（panic）でも片付くよう `Drop` で消す。
    struct TempDir(PathBuf);

    impl TempDir {
        /// プロセス固有の名前で作る（前回の残骸は先に消す）。
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("openshoki-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("creating the temp dir should succeed");
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// テストスレッド自身がミキサー出力を引く同期ドライバ。sleep を挟まないので位置の検証が
    /// 決定的になる。ただしクリアの完了を待つ操作（`unload` / `stop`）は自分で引けなくなるため
    /// 使えない（それらは `FakeOutput` を使う）。
    struct ManualOutput(MixerSource);

    impl ManualOutput {
        /// rodio の位置更新 1 周期ぶんを引く。
        fn tick(&mut self) {
            for _ in 0..PULL_SAMPLES_PER_TICK {
                // Player が生きている間はキューが無音を流すので None は来ない。Player の drop 後は
                // None が返るため、その場合はこの周を打ち切る。
                if self.0.next().is_none() {
                    break;
                }
            }
        }

        fn tick_times(&mut self, times: usize) {
            for _ in 0..times {
                self.tick();
            }
        }
    }

    /// ミキサー出力を引き続ける疑似オーディオスレッド（実機のオーディオデバイスの代役。
    /// 必要な理由は `AudioPlayer::connect` の doc コメント参照）。`unload` / `stop` のように
    /// クリアの完了を待つ操作を呼ぶテストで使う。**テストの終わりまで保持すること**（drop すると
    /// 引くのが止まり、以降は位置が動かない）。
    struct FakeOutput {
        shutdown: Arc<AtomicBool>,
        handle: Option<JoinHandle<()>>,
    }

    impl FakeOutput {
        fn spawn(source: MixerSource) -> Self {
            let shutdown = Arc::new(AtomicBool::new(false));
            let flag = Arc::clone(&shutdown);
            let handle = std::thread::spawn(move || {
                let mut output = ManualOutput(source);
                while !flag.load(Ordering::Relaxed) {
                    output.tick();
                    std::thread::sleep(PULL_INTERVAL);
                }
            });
            Self {
                shutdown,
                handle: Some(handle),
            }
        }
    }

    impl Drop for FakeOutput {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// 出力デバイスを開かない `AudioPlayer` と、その出力（引く役はまだ決めない）。
    fn player_without_device() -> (AudioPlayer, MixerSource) {
        let (mixer, source) = rodio::mixer::mixer(
            ChannelCount::new(OUTPUT_CHANNELS).expect("the channel count is non-zero"),
            SampleRate::new(OUTPUT_SAMPLE_RATE).expect("the sample rate is non-zero"),
        );
        (AudioPlayer::connect(&mixer), source)
    }

    /// 疑似オーディオスレッドに駆動される `AudioPlayer`。第 2 要素はテストの終わりまで保持する
    /// こと（drop すると引くのが止まり、`wait_until` が必ずタイムアウトする）。
    fn player_driven_by_fake_output() -> (AudioPlayer, FakeOutput) {
        let (player, source) = player_without_device();
        (player, FakeOutput::spawn(source))
    }

    /// 条件が満たされるまで上限つきで待つ（別スレッドが引くのを待つ。`docs/rules/error-handling.md`
    /// の「完了待ちポーリングには必ず上限を設ける」に従う）。
    fn wait_until(condition_name: &str, mut condition: impl FnMut() -> bool) {
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        while Instant::now() < deadline {
            if condition() {
                return;
            }
            std::thread::sleep(PULL_INTERVAL);
        }
        panic!(
            "timed out after {SETTLE_TIMEOUT:?} waiting until {condition_name}; \
             the fake audio output may have stopped pulling samples"
        );
    }

    /// 録音・ミックスと同じエンコード設定（`mixdown::encode_mp3`）で試験用の MP3 を書き出す。
    fn write_test_mp3(dir: &Path, channels: u16, sample_rate: u32) -> PathBuf {
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
        path
    }

    /// 疑似出力のフォーマットに合わせたフィクスチャ（一時ディレクトリごと返す）。
    fn fixture(name: &str) -> (TempDir, PathBuf) {
        let dir = TempDir::new(name);
        let path = write_test_mp3(dir.path(), OUTPUT_CHANNELS, OUTPUT_SAMPLE_RATE);
        (dir, path)
    }

    /// `seek` が読み飛ばしフォールバックを持たない前提（アプリが出力する MP3 は全体長が分かり、
    /// `try_seek` が成立する）を固定する。ここが崩れると全てのシークが `Err` になり、クリックしても
    /// 再生位置が動かない（機能が事実上死ぬ）ため、依存（rodio / symphonia）の更新やデコーダの
    /// 開き方の変更で気づけるようにする。
    #[test]
    fn open_decoder_reports_duration_and_supports_seeking() {
        let dir = TempDir::new("decoder");
        // 再生対象になりうる組み合わせ（モノラル/ステレオ、44.1k/48k）で確認する。
        for (channels, sample_rate) in [(1u16, 44_100u32), (2, 48_000)] {
            let path = write_test_mp3(dir.path(), channels, sample_rate);
            let mut source =
                super::open_decoder(&path).expect("opening the test MP3 should succeed");
            assert!(
                source.total_duration().is_some(),
                "the duration must be known ({channels}ch/{sample_rate}Hz), \
                 otherwise the seek bar degrades to display-only"
            );
            assert!(
                source.try_seek(SEEK_TARGET).is_ok(),
                "seeking must be supported ({channels}ch/{sample_rate}Hz), \
                 otherwise every seek fails and the playback position never moves"
            );
        }
    }

    /// 位置は「再生中だけ進む」。表示（進捗バー・時刻・セグメントのハイライト）は位置と
    /// `is_playing` で駆動するため、ここがずれると「音は止まっているのに表示だけ動く」不整合になる。
    /// このテストは出力を自分で引く同期ドライバで動かすので、待ち時間に依存せず決定的に判定できる。
    #[test]
    fn playing_advances_the_position_and_pausing_holds_it() {
        let (_dir, path) = fixture("play");
        let (mut player, source) = player_without_device();
        // `load` はキューが空の状態で呼ぶためクリアの完了待ちが無く、同期ドライバで足りる。
        let mut output = ManualOutput(source);

        player
            .load(&path)
            .expect("loading the test MP3 should succeed");
        assert!(player.is_loaded(), "loading must set the playback target");
        assert!(
            player.duration().is_some(),
            "the duration must be known for the test fixture"
        );
        // 新しく作ったハンドルなので rodio 内部の位置も 0（既存ハンドルへの再ロードでは
        // `position` の doc にある数 ms のズレが出るため、その場合は待ちが要る）。
        assert_eq!(
            player.position(),
            Duration::ZERO,
            "a freshly created handle reports the beginning right after loading"
        );
        assert!(!player.is_playing(), "loading must not start playback");

        player.play_pause();
        assert!(player.is_playing(), "the first toggle must start playback");
        output.tick_times(4);
        assert!(
            player.position() > Duration::ZERO,
            "the position must advance while playing"
        );

        player.play_pause();
        assert!(
            !player.is_playing(),
            "the second toggle must pause playback"
        );
        // 一時停止は次の位置更新で効くので、反映のティックを回してから固定を見る。
        output.tick_times(2);
        let paused_at = player.position();
        output.tick_times(20);
        assert_eq!(
            player.position(),
            paused_at,
            "the position must not advance while paused"
        );
    }

    /// 終端まで再生してキューが空になった後に手放しても、観測値が「何もロードされていない」で
    /// 揃う（#96 の症状。rodio の `clear()` はキューが空だと内部位置を戻さないため、
    /// `position()` のガードが無いと前のセッションの位置が残り、表示がそれで駆動される）。
    #[test]
    fn unload_after_the_queue_drains_resets_the_position() {
        let (_dir, path) = fixture("unload-drained");
        let (mut player, _output) = player_driven_by_fake_output();

        player
            .load(&path)
            .expect("loading the test MP3 should succeed");
        let total = player.duration().expect("the duration must be known");
        // 終端の手前まで飛ばしてから再生し、キューが空になる（自然終端）のを待つ。
        player
            .seek(total.saturating_sub(Duration::from_millis(200)))
            .expect("seeking near the end should succeed");
        player.play_pause();
        wait_until("the queue drains at the end of the track", || {
            !player.is_playing()
        });
        assert!(
            player.position() > Duration::ZERO,
            "the position must sit near the end before unloading, otherwise this test would \
             not reproduce the empty-queue case"
        );

        player.unload();
        assert!(!player.is_loaded(), "unloading must drop the target");
        assert_eq!(
            player.position(),
            Duration::ZERO,
            "unloading must report the beginning, otherwise the display keeps the old position"
        );
        assert_eq!(player.duration(), None, "unloading must drop the duration");
        assert!(!player.is_playing(), "unloading must stop playback");
    }

    /// 再生中に手放した場合（キューが空でない経路）も同じく観測値が揃う。
    #[test]
    fn unload_while_playing_resets_every_observable_value() {
        let (_dir, path) = fixture("unload-playing");
        let (mut player, _output) = player_driven_by_fake_output();

        player
            .load(&path)
            .expect("loading the test MP3 should succeed");
        player.play_pause();
        wait_until("the position advances before unloading", || {
            player.position() > Duration::ZERO
        });

        player.unload();
        assert!(!player.is_loaded(), "unloading must drop the target");
        assert_eq!(
            player.position(),
            Duration::ZERO,
            "unloading must report the beginning"
        );
        assert_eq!(player.duration(), None, "unloading must drop the duration");
        assert!(!player.is_playing(), "unloading must stop playback");
    }

    /// 停止は対象を保持したまま先頭へ戻す（`unload` との違い）。対象や全体長まで落とすと詳細ペインが
    /// 「未選択」相当に見え、キューを積み直さないと停止後の Play が無反応になる。
    #[test]
    fn stop_keeps_the_target_and_rewinds() {
        let (_dir, path) = fixture("stop");
        let (mut player, _output) = player_driven_by_fake_output();

        player
            .load(&path)
            .expect("loading the test MP3 should succeed");
        player.play_pause();
        wait_until("the position advances before stopping", || {
            player.position() > Duration::ZERO
        });

        player.stop();
        assert!(player.is_loaded(), "stopping must keep the target loaded");
        assert!(
            player.duration().is_some(),
            "stopping must keep the duration"
        );
        assert!(!player.is_playing(), "stopping must leave playback paused");
        wait_until("the position rewinds to the beginning", || {
            player.position() == Duration::ZERO
        });

        player.play_pause();
        assert!(
            player.is_playing(),
            "playing after a stop must restart from the beginning"
        );
    }

    /// ロードに失敗したら前の対象を残さない（stale な対象を後続の seek / play_pause が
    /// 開き直すと、表示中のセッションと音が食い違う）。
    #[test]
    fn failed_load_leaves_nothing_loaded() {
        let (dir, path) = fixture("load-failure");
        let (mut player, _output) = player_driven_by_fake_output();

        player
            .load(&path)
            .expect("loading the test MP3 should succeed");
        assert!(
            player.is_loaded(),
            "the fixture must load before exercising the failure path"
        );

        // MP3 として解釈できない別ファイルを読ませて失敗経路へ入れる。
        let broken = dir.path().join("broken.mp3");
        std::fs::write(&broken, b"not an audio file").expect("writing the broken file should work");
        assert!(
            player.load(&broken).is_err(),
            "loading a non-audio file must fail"
        );
        assert!(
            !player.is_loaded(),
            "a failed load must not keep the previous target"
        );
        assert_eq!(
            player.duration(),
            None,
            "a failed load must not keep the previous duration"
        );
        assert!(
            player.seek(Duration::from_secs(1)).is_err(),
            "seeking without a target must report an error instead of silently doing nothing"
        );
    }

    /// ロード済みならシークでき、その位置から再生が続く。未ロードなら `Err`（rodio の `try_seek` は
    /// キューが空だと何もせず `Ok` を返すので、ラッパ側で `Err` に倒していることの確認）。
    #[test]
    fn seek_moves_the_position_and_reports_missing_target() {
        let (_dir, path) = fixture("seek");
        let (mut player, _output) = player_driven_by_fake_output();

        assert!(
            player.seek(SEEK_TARGET).is_err(),
            "seeking before loading must report an error"
        );

        player
            .load(&path)
            .expect("loading the test MP3 should succeed");
        player
            .seek(SEEK_TARGET)
            .expect("seeking a loaded target should succeed");
        assert!(!player.is_playing(), "seeking must not start playback");

        // 位置の値そのものは rodio が要求値を書くだけなので、再生を再開して「シーク先から先へ
        // 進む」ことまで見る（デコーダが先頭に戻っていれば進まない）。
        player.play_pause();
        wait_until("the position advances past the seek target", || {
            player.position() > SEEK_TARGET + PULL_INTERVAL * 4
        });
    }

    /// 対象ファイルが消えた後は、黙って鳴り出さずに縮退する（削除前に `unload` する設計の裏側。
    /// 開き直しの失敗はログのみで、対象は保持したまま「鳴らない」状態になる）。
    #[test]
    fn missing_file_degrades_without_playing() {
        let (_dir, path) = fixture("missing-file");
        let (mut player, _output) = player_driven_by_fake_output();

        player
            .load(&path)
            .expect("loading the test MP3 should succeed");
        player.stop();
        std::fs::remove_file(&path).expect("removing the fixture should succeed");
        // 停止後はキューが空なので、次の操作は必ず開き直しを試みる（それが失敗する）。
        player.stop();

        player.play_pause();
        assert!(
            !player.is_playing(),
            "playback must not start when the file is gone"
        );
        assert!(
            player.seek(SEEK_TARGET).is_err(),
            "seeking must report an error when the file cannot be reopened"
        );
        assert!(
            player.is_loaded(),
            "the target stays loaded; the caller decides whether to unload"
        );
    }
}
