//! shoki — メニューバー／タスクバーに常駐する録音アプリのエントリポイント。
//!
//! 起動時はウィンドウを表示せずトレイに常駐し、トレイメニューから設定ウィンドウ・Recordings
//! ウィンドウの表示/非表示とアプリ終了を行う。録音・文字起こし・議事録生成は各モジュール
//! （`recorder` / `transcribe` / `summarize`）が持ち、ここは UI との配線とタイマー駆動の
//! 状態追従（メニューバー表示・再生位置・進行状況）を担う。

#[cfg(target_os = "macos")]
mod app_audio_monitor;
mod atomic_replace;
mod config;
mod inference_slot;
mod mixdown;
mod model_download;
mod player;
mod private_file;
mod recorder;
mod recordings;
mod single_instance;
mod summarize;
mod summary_model;
#[cfg(target_os = "macos")]
mod system_audio;
mod transcribe;
mod transcript;
mod tray;
mod whisper_model;
mod windows;

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, SystemTime};

// VecModel の row_data / set_row_data（tick の行単位更新）に必要。
use slint::Model;

use tray_icon::menu::{IconMenuItem, MenuEvent};

use crate::config::Config;
use crate::recorder::Recorder;
use crate::tray::Tray;
use crate::windows::models::ModelsRefresh;

slint::include_modules!();

/// メニューイベントのポーリング周期。アイドル時の負荷を抑えつつ、操作の体感遅延が
/// 出ない程度の値にする。録音中のメニューバー表示更新（経過時間・点滅）もこの周期に相乗りする。
const MENU_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// 録音中アイコンの明滅（breathing）の 1 サイクル（明→暗→明）の秒数。サイン波でゆったり
/// 変化させる。実機の見え方で微調整しやすいよう定数化する。
const BLINK_CYCLE_SECS: f32 = 2.0;

/// ウィンドウの初期ジオメトリ。イベントループ稼働中に初めて show() すると、位置・サイズが
/// 確定されないまま高さ 0 で表示される。初回表示時にこの値を明示してジオメトリを確定させる。
/// 幅・高さは `ui/app-window.slint` の min/preferred と一致させること（片方だけ変えない）。
const WINDOW_WIDTH: f32 = 460.0;
const WINDOW_HEIGHT: f32 = 720.0;
/// 初回表示位置（画面左上からの暫定値）。中央寄せ等の調整は後続に回す。
const WINDOW_X: f32 = 240.0;
const WINDOW_Y: f32 = 160.0;

/// Recordings ウィンドウの初期ジオメトリ。幅・高さは `ui/recordings-window.slint` の
/// min/preferred と一致させること（片方だけ変えない）。設定ウィンドウと重ならない位置に出す。
const RECORDINGS_WIDTH: f32 = 1100.0;
const RECORDINGS_HEIGHT: f32 = 720.0;
const RECORDINGS_X: f32 = 200.0;
const RECORDINGS_Y: f32 = 120.0;

/// 文字起こしウィンドウの初期ジオメトリ。幅・高さは `ui/transcription-window.slint` の
/// min/preferred と一致させること（片方だけ変えない）。設定ウィンドウの扉から開くので、
/// それと重ならない位置に出す。
const TRANSCRIPTION_WIDTH: f32 = 620.0;
const TRANSCRIPTION_HEIGHT: f32 = 780.0;
const TRANSCRIPTION_X: f32 = 700.0;
const TRANSCRIPTION_Y: f32 = 160.0;

/// 議事録ウィンドウ（`ui/minutes-window.slint` と一致させる）。**兄弟と同じ幅で開く**
/// （行き来しても画面が動かないように）。
const MINUTES_WIDTH: f32 = 620.0;
const MINUTES_HEIGHT: f32 = 700.0;
const MINUTES_X: f32 = 740.0;
const MINUTES_Y: f32 = 200.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 多重起動ガード。取得したロックは _instance_lock でプロセス終了まで保持し続ける
    // （背景・各分岐の意味・保持理由は `single_instance` モジュール doc / `Acquire` 参照）。
    let _instance_lock = match single_instance::acquire() {
        single_instance::Acquire::Acquired(lock) => Some(lock),
        single_instance::Acquire::AlreadyRunning => {
            eprintln!("Exiting because another instance of shoki is already running.");
            return Ok(());
        }
        single_instance::Acquire::Unavailable => None,
    };

    // ウィンドウは生成するが表示はしない（起動時はトレイのみ）。
    let ui = AppWindow::new()?;

    // 設定を読み込み、現在の保存先・自動録音トグル・登録アプリ一覧を画面へ反映する。
    // 失敗時は load() がデフォルトを返す。
    let config = Rc::new(RefCell::new(Config::load()));

    // 保存先が無いまま録音を始めると、`create_session_dir` が黙って作り直すか（フォルダの
    // 移動・改名）、作成に失敗して録音そのものが始まらない（外付けディスクの未マウントなど。
    // `/Volumes` は書き込めない）。どちらも気づきにくいので、起動時に 1 回だけ知らせる。
    // 作成も選び直しもしない（勝手に既定へ戻すと利用者の設定を失う）。
    {
        let recording_dir = &config.borrow().recording_dir;
        if !recording_dir.exists() {
            eprintln!(
                "The configured recording folder is missing: {}",
                recording_dir.display()
            );
        }
    }

    // 強制終了などで残った取得中ファイル（数 GB になりうる）を回収する。走っている取得は
    // 対象にならない（判定は `atomic_replace::sweep_orphaned_parts` の doc）。
    model_download::sweep_orphaned_part_files();

    // 内蔵 whisper モデルのダウンロード・状態管理。設定画面（モデル選択・DL 状況表示）と
    // 文字起こしワーカーで同じ状態を共有し、同一モデルの二重ダウンロードを防ぐ。
    let model_downloader = model_download::ModelDownloader::new();

    ui.set_app_version(app_version_text());
    ui.set_recording_dir(recording_dir_text(&config.borrow().recording_dir));
    ui.set_auto_record_app(config.borrow().auto_record_on_app_mic);
    // 保存値は load 時に範囲へ正規化済みなので、そのまま表示へ渡す。
    ui.set_auto_stop_debounce_secs(config.borrow().auto_stop_debounce_secs as i32);
    // 扉の文言（機能の ON/OFF・構成・状態）。**初期化と更新が同じ関数を通る**ので、状態の
    // 導出が増えても初期化だけ取り残されない。
    windows::transcription::apply_door(&ui, &config.borrow(), &model_downloader);
    windows::minutes::apply_door(&ui, &config.borrow(), &model_downloader);
    // 登録アプリの一覧を Slint のモデルで持ち、追加/削除で更新する。
    let app_list_model = Rc::new(slint::VecModel::<TriggerApp>::from(
        config
            .borrow()
            .app_mic_triggers
            .iter()
            .map(trigger_app_row)
            .collect::<Vec<_>>(),
    ));
    ui.set_app_list(app_list_model.clone().into());

    // 「フォルダを選択」: ネイティブのフォルダ選択ダイアログで保存先を選び直し、保存・表示更新する。
    // コールバックはメインスレッド（Slint イベントループ）上で動くため、同期 API を使う。
    let config_for_pick = Rc::clone(&config);
    let ui_for_pick = ui.as_weak();
    ui.on_choose_folder(move || {
        let Some(ui) = ui_for_pick.upgrade() else {
            return;
        };
        // 現在の設定を複製し、選択結果を反映した候補を作る。
        let mut candidate = config_for_pick.borrow().clone();
        let mut dialog = rfd::FileDialog::new();
        if candidate.recording_dir.is_dir() {
            dialog = dialog.set_directory(&candidate.recording_dir);
        }
        let Some(folder) = dialog.pick_folder() else {
            return; // キャンセル時は何もしない。
        };
        candidate.recording_dir = folder;
        // 永続化に成功してからメモリ上の設定と画面表示を更新する。
        // 先に更新すると、保存失敗時に「表示は変わったのに保存されていない」不整合になる。
        if let Err(err) = candidate.save() {
            eprintln!(
                "Not changing the recording folder because saving the settings failed: {err}"
            );
            return;
        }
        ui.set_recording_dir(recording_dir_text(&candidate.recording_dir));
        *config_for_pick.borrow_mut() = candidate;
    });

    // 「登録アプリのマイク使用で自動録音」トグル: 永続化に成功してから反映する。
    // Slint 側は先にチェック状態を新値へ更新してからこのコールバックを呼ぶため、保存失敗時は
    // 表示を保存済みの値へ戻し、表示・メモリ・ディスクの食い違いを防ぐ（debounce 側と対称）。
    let config_for_auto_app = Rc::clone(&config);
    let ui_for_auto_app = ui.as_weak();
    ui.on_toggle_auto_record_app(move |enabled| {
        let Some(ui) = ui_for_auto_app.upgrade() else {
            return;
        };
        let mut candidate = config_for_auto_app.borrow().clone();
        candidate.auto_record_on_app_mic = enabled;
        if let Err(err) = candidate.save() {
            eprintln!(
                "Not changing the app-based auto-record setting because saving the settings failed: {err}"
            );
            ui.set_auto_record_app(config_for_auto_app.borrow().auto_record_on_app_mic);
            return;
        }
        *config_for_auto_app.borrow_mut() = candidate;
    });

    // 自動停止デバウンス秒数の変更: Stepper の値を範囲へ丸めて永続化し、成功後にメモリへ反映する。
    // Stepper 側でも minimum/maximum を持つが、手編集された設定値との整合のため保存側でも丸める。
    let config_for_debounce = Rc::clone(&config);
    let ui_for_debounce = ui.as_weak();
    ui.on_change_auto_stop_debounce(move |secs| {
        let Some(ui) = ui_for_debounce.upgrade() else {
            return;
        };
        let secs =
            config::clamp_debounce_secs(u32::try_from(secs).unwrap_or(config::DEBOUNCE_MIN_SECS));
        let mut candidate = config_for_debounce.borrow().clone();
        candidate.auto_stop_debounce_secs = secs;
        if let Err(err) = candidate.save() {
            eprintln!("Not changing the auto-stop delay because saving the settings failed: {err}");
            // 保存できなかったので表示を保存済みの値へ戻し、表示・メモリ・ディスクの食い違いを防ぐ。
            ui.set_auto_stop_debounce_secs(
                config_for_debounce.borrow().auto_stop_debounce_secs as i32,
            );
            return;
        }
        // 丸めた値を Stepper へ反映し、表示・メモリ・ディスクを一致させる。
        ui.set_auto_stop_debounce_secs(secs as i32);
        *config_for_debounce.borrow_mut() = candidate;
    });

    // 登録アプリの削除: 一覧のインデックスで設定とモデルから取り除く（永続化成功後に反映）。
    let config_for_remove = Rc::clone(&config);
    let model_for_remove = Rc::clone(&app_list_model);
    ui.on_remove_app(move |index| {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let mut candidate = config_for_remove.borrow().clone();
        if index >= candidate.app_mic_triggers.len() {
            return;
        }
        candidate.app_mic_triggers.remove(index);
        if let Err(err) = candidate.save() {
            eprintln!("Not removing the app because saving the settings failed: {err}");
            return;
        }
        model_for_remove.remove(index);
        *config_for_remove.borrow_mut() = candidate;
    });

    // 登録アプリの追加（macOS のみ）: ネイティブダイアログで .app を選び、バンドル ID・表示名を
    // 読んで登録する（永続化成功後に反映）。既に同じバンドル ID があれば追加しない。
    #[cfg(target_os = "macos")]
    {
        let config_for_add = Rc::clone(&config);
        let model_for_add = Rc::clone(&app_list_model);
        ui.on_add_app(move || {
            let Some(app_path) = rfd::FileDialog::new()
                .add_filter("Application", &["app"])
                .set_directory("/Applications")
                .pick_file()
            else {
                return; // キャンセル。
            };
            let Some(trigger) = app_audio_monitor::app_info_for_path(&app_path) else {
                eprintln!("Could not read the bundle identifier of the selected app");
                return;
            };
            let mut candidate = config_for_add.borrow().clone();
            if candidate
                .app_mic_triggers
                .iter()
                .any(|existing| existing.bundle_id == trigger.bundle_id)
            {
                return; // 登録済み。
            }
            let row = trigger_app_row(&trigger);
            candidate.app_mic_triggers.push(trigger);
            if let Err(err) = candidate.save() {
                eprintln!("Not adding the app because saving the settings failed: {err}");
                return;
            }
            model_for_add.push(row);
            *config_for_add.borrow_mut() = candidate;
        });
    }

    // Slint バックエンドの初期化後にトレイを常駐させる（macOS の NSApplication 初期化後）。
    let tray = Tray::new()?;

    // 登録アプリのマイク使用を監視するモニタ（macOS 14.4+）。照会は失敗しても落ちない設計のため、
    // 生成は常に成功する。実際に照会できるかはポーリング時に判定する。
    #[cfg(target_os = "macos")]
    let app_monitor = app_audio_monitor::AppAudioMonitor::new();

    // 重い ML 推論（whisper / 要約 LLM）の実行権。両ワーカーで 1 枠を共有し、ピークが
    // 加算されないようにする（理由は `src/inference_slot.rs`）。
    let inference_slot = inference_slot::InferenceSlot::new();

    // 議事録生成のバックグラウンドワーカー。文字起こしワーカーが成功時に投入する（設定
    // `auto_summarize` が ON のときだけ依頼が添えられる）。
    let summarizer =
        summarize::SummarizeWorker::start(model_downloader.clone(), inference_slot.clone());

    // 文字起こしのバックグラウンドワーカー。設定 OFF の間はジョブが来ないだけで、常駐コストは
    // アイドルなスレッド 1 本のみ。起動失敗時は文字起こしだけが無効化される（録音は無関係）。
    let transcriber = transcribe::TranscribeWorker::start(
        model_downloader.clone(),
        summarizer.clone(),
        inference_slot,
    );

    // 録音停止後の後処理（極小音量の正規化→文字起こし投入→ミックス生成）を直列に行う
    // バックグラウンドワーカー。自動経路の文字起こしは後処理ワーカーが完了後に投入する
    // （正規化後の音声で文字起こしさせる）。transcriber は Clone 共有で、Recordings ウィンドウの
    // 手動再実行・状態表示も同じワーカー・同じ状態マップを使う。
    let postprocessor = mixdown::PostProcessWorker::start(transcriber.clone());

    // ウィンドウを閉じても終了させず、非表示にして常駐を保つ。メニューからは開くだけで、
    // 閉じるのはウィンドウ自身の閉じるボタンに任せる。
    ui.window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);

    // Recordings ウィンドウ（録音一覧＋再生）。設定ウィンドウと同じく起動時に生成して隠しておき、
    // トレイの「Recordings…」で表示する。閉じても常駐を保つ。
    let recordings_ui = RecordingsWindow::new()?;
    recordings_ui
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);

    // 音声再生ハンドル。出力デバイスを開けない環境では再生機能なしで続行する（一覧・常駐は動く）。
    let player: Rc<RefCell<Option<player::AudioPlayer>>> = Rc::new(RefCell::new(
        match player::AudioPlayer::new() {
            Ok(p) => Some(p),
            Err(err) => {
                eprintln!(
                    "Continuing without audio playback because the output device could not be opened: {err}"
                );
                None
            }
        },
    ));
    // 一覧に表示中のセッション（選択インデックス→音源パスの解決に使う）。
    let sessions: Rc<RefCell<Vec<recordings::RecordingSession>>> =
        Rc::new(RefCell::new(Vec::new()));
    // 一覧の Slint モデル。開いたときの再構築に加え、文字起こし状態の変化を tick が
    // 行単位で反映する（set_row_data）ため、Rc で保持し続ける。
    let sessions_model: Rc<slint::VecModel<SessionRow>> = Rc::new(slint::VecModel::default());
    recordings_ui.set_sessions(sessions_model.clone().into());
    // 選択中セッションのトランスクリプト（セグメントクリック→開始秒の解決、tick→現在セグメントの
    // 算出に使う）。選択のたびに読み直す。
    let transcript_segments: Rc<RefCell<Vec<transcript::TranscriptSegment>>> =
        Rc::new(RefCell::new(Vec::new()));

    // 選んだ録音の読み込み結果を UI スレッドへ返す道（#152）。**tick が受け取る**——Slint の
    // プロパティも `Rc` の共有状態も UI スレッド専有なので、読み込みスレッドからは触れない。
    let (load_sender, load_receiver) = std::sync::mpsc::channel::<LoadedSession>();
    // 選択の世代。**遅れて届いた結果で新しい選択を上書きしない**ための番号で、選ぶたびに増やす
    // （速く切り替えると、前の読み込みがあとから返ってくる）。
    let load_generation = Rc::new(Cell::new(0u64));

    // セッション選択: 詳細を更新し、その音源を再生準備する。
    //
    // **重い読み込みは別スレッドへ出す**（#152）。文字起こし JSON のパースと、音声のデコーダを
    // 開く処理（MP3 は全長を得るためにファイルを走査する）は録音の長さに比例して重く、UI
    // スレッドでやると 1 時間の録音では数秒画面が固まる。ここでやるのは「すぐ出せるものを出す」
    // ことだけで、残りは届いた順に反映する。
    {
        let player = Rc::clone(&player);
        let sessions = Rc::clone(&sessions);
        let transcript_segments = Rc::clone(&transcript_segments);
        let transcriber = transcriber.clone();
        let summarizer = summarizer.clone();
        let rec_weak = recordings_ui.as_weak();
        let generation = Rc::clone(&load_generation);
        let load_sender = load_sender.clone();
        recordings_ui.on_select_session(move |index| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            let sessions_ref = sessions.borrow();
            let Some(session) = usize::try_from(index)
                .ok()
                .and_then(|i| sessions_ref.get(i))
            else {
                return;
            };
            let generation_id = generation.get().wrapping_add(1);
            generation.set(generation_id);

            // --- ここまでが即時。ディスクを読まずに出せるものだけを入れる ---
            rec.set_has_selection(true);
            // 一覧の行と**同じ組み立て**にする（`Aug 10, 2026 · 14:02`）。左右で日時の形が
            // 違うと、同じ録音を見ていることが読み取りにくい。
            rec.set_detail_datetime(
                format!("{} · {}", session.display_date(), session.display_time()).into(),
            );
            rec.set_detail_sources(session.source_summary().into());
            rec.set_has_transcript(session.has_transcript);
            // 文字起こしの状態テキストと Transcribe ボタンの活性を、ワーカーの進行状況＋
            // JSON の有無から設定する（以後の変化は tick が追従させる）。
            refresh_detail_transcript_status(&rec, &transcriber, session);
            // 議事録生成も同じ流儀で状態を設定する（中身の読み込みは下のスレッドで行う）。
            refresh_detail_summary_status(&rec, &summarizer, session);
            rec.set_playing(false);
            rec.set_current_segment(-1);
            // **前の録音の中身を残さない**。読み込みが終わるまで空にし、読み込み中であることを
            // 出す（前の文字起こしが表示されたままだと、別の録音の内容を読んでしまう）。
            rec.set_segments(Rc::new(slint::VecModel::default()).into());
            rec.set_summary_rows(Rc::new(slint::VecModel::default()).into());
            rec.set_loading(true);
            transcript_segments.borrow_mut().clear();
            // **読み込みが終わるまでは再生できない**（音源をまだ開いていない）。押しても無反応、
            // にしないため、ここでは押せない状態にしておく——鳴らせるかどうかは開いてみて
            // 初めて分かるので、`apply_loaded_session` が実際の結果で入れ直す。
            rec.set_playable(false);
            // 長さも分からないので、シークバーは表示専用に縮退させる。
            rec.set_seekable(false);
            apply_playback_position(&rec, Duration::ZERO, None);
            // 前の録音の音声は**すぐ手放す**（読み込みを待つ間に前の音が鳴らないように）。
            if let Some(p) = player.borrow_mut().as_mut() {
                p.unload();
            }

            // --- ここからが別スレッド。届いたら世代を確かめて反映する ---
            let dir = session.dir.clone();
            let playback_path = session.playback_path();
            let sender = load_sender.clone();
            std::thread::spawn(move || {
                let loaded = LoadedSession {
                    generation: generation_id,
                    segments: transcript::load_transcript(&dir),
                    summary: summarize::load_summary(&dir),
                    // 失敗はここでログして `None` に畳む（UI へ運ぶのは結果だけにする）。
                    playback: playback_path.and_then(|path| {
                        match player::AudioPlayer::prepare(&path) {
                            Ok(prepared) => Some(prepared),
                            Err(err) => {
                                eprintln!("Failed to load the recording for playback: {err}");
                                None
                            }
                        }
                    }),
                };
                // **結果は送るだけ**。Slint のプロパティも `Rc` の共有状態も UI スレッド専有なので、
                // ここからは触れない（受け取って反映するのは tick）。
                if sender.send(loaded).is_err() {
                    // 受け手が畳まれている（アプリ終了時）。表示は捨ててよい。
                    eprintln!("Skipping the loaded recording because the window is gone");
                }
            });
        });
    }

    // 再生/一時停止トグル。
    {
        let player = Rc::clone(&player);
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_play_pause(move || {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            if let Some(p) = player.borrow().as_ref() {
                p.play_pause();
                rec.set_playing(p.is_playing());
            }
        });
    }

    // 停止（先頭へ戻す）。
    {
        let player = Rc::clone(&player);
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_stop(move || {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            if let Some(p) = player.borrow().as_ref() {
                p.stop();
                rec.set_playing(false);
                apply_playback_position(&rec, Duration::ZERO, p.duration());
            }
        });
    }

    // トランスクリプトのセグメントクリック: その開始秒へ再生位置をスキップする。
    {
        let player = Rc::clone(&player);
        let transcript_segments = Rc::clone(&transcript_segments);
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_seek_to_segment(move |index| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            let segments = transcript_segments.borrow();
            let Some(segment) = usize::try_from(index).ok().and_then(|i| segments.get(i)) else {
                return;
            };
            // 音が鳴らない状況（出力デバイスを開けない・再生対象が無くて未ロード）ではシークせず
            // ハイライトだけ付ける。表示と食い違わず、読み進めの目印として機能する
            // （この間は再生 tick も表示を駆動しないのでハイライトが残る）。
            let player = player.borrow();
            let loaded = player.as_ref().filter(|p| p.is_loaded());
            if let Some(p) = loaded
                && let Err(err) = p.seek(segment.start_duration())
            {
                eprintln!(
                    "Skipping the highlight update because seeking to the segment failed: {err}"
                );
                return;
            }
            // クリックしたセグメントを即ハイライトする（次の tick で位置に追従する）。
            rec.set_current_segment(index);
        });
    }

    // シークバーのドラッグ中のプレビュー: 時刻表示だけをドラッグ位置へ追従させる
    // （分担は `.slint` の `SeekBar` の doc コメント参照）。
    {
        let player = Rc::clone(&player);
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_scrub_preview(move |ratio| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            let player = player.borrow();
            let Some(p) = player.as_ref() else {
                return;
            };
            let duration = p.duration();
            let Some(position) = seek_position_from_ratio(ratio, duration) else {
                return;
            };
            rec.set_time_text(format_playback_time(position, duration).into());
        });
    }

    // シークバーのクリック確定・ドラッグ終了: その位置へ再生位置を移動する（再生/一時停止の
    // 状態は変えない）。
    {
        let player = Rc::clone(&player);
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_seek_to_ratio(move |ratio| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            let player = player.borrow();
            let Some(p) = player.as_ref() else {
                return;
            };
            let duration = p.duration();
            let Some(position) = seek_position_from_ratio(ratio, duration) else {
                return;
            };
            if let Err(err) = p.seek(position) {
                eprintln!(
                    "Skipping the seek bar and time display update because seeking failed: {err}"
                );
                return;
            }
            // 離した位置で表示を即確定させる（次の tick を待たない）。
            apply_playback_position(&rec, position, duration);
        });
    }

    // 詳細ペインの Transcribe ボタン: 選択中セッションの文字起こしを（再）実行する。
    // 完了済みでも上書きで再実行できる（設定 `auto_transcribe` とは独立。#69 プラン）。
    // 設定値はここでスナップショットし、処理中の設定変更の影響を受けない。
    {
        let sessions = Rc::clone(&sessions);
        let config = Rc::clone(&config);
        let transcriber = transcriber.clone();
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_transcribe_session(move |index| {
            let sessions = sessions.borrow();
            let Some(session) = usize::try_from(index).ok().and_then(|i| sessions.get(i)) else {
                return;
            };
            let audio_paths = session.audio_source_paths();
            if audio_paths.is_empty() {
                return;
            }
            let config_ref = config.borrow();
            transcriber.submit(transcribe::TranscribeJob {
                session_dir: session.dir.clone(),
                audio_paths,
                model_id: config_ref.whisper_model.clone(),
                model_override: config_ref.whisper_model_path.clone(),
                language: config_ref.transcribe_language.clone(),
                // 手動の再実行でも、設定 ON なら要約を作り直す（作り直さないと `summary.md` が
                // 古い文字起こしのまま残り、内容が食い違う）。
                summarize: auto_summarize_job(&config_ref, &session.dir),
            });
            // 投入結果（通常は「文字起こし中」）を詳細ペインへ即反映し、次の tick を待つ間の
            // 2 連クリックによる多重投入を防ぐ。一覧行のドットは tick の差分更新に任せる。
            if let Some(rec) = rec_weak.upgrade() {
                refresh_detail_transcript_status(&rec, &transcriber, session);
            }
        });
    }

    // 「Summarize」: 選択中セッションの議事録生成を手動で（再）生成する。設定 `auto_summarize`
    // とは独立で、押されたら生成する（文字起こしが無いセッションは Slint 側でボタンが無効）。
    // ジョブの組み立て・設定のスナップショットは `manual_summarize_job`（その doc が正）。
    {
        let sessions = Rc::clone(&sessions);
        let config = Rc::clone(&config);
        let summarizer = summarizer.clone();
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_summarize_session(move |index| {
            let sessions = sessions.borrow();
            let Some(session) = usize::try_from(index).ok().and_then(|i| sessions.get(i)) else {
                return;
            };
            // 文字起こしが無ければ入力が無い（ボタンは無効なので通常は来ない。黙って戻ると
            // 「押しても何も起きない」になるのでログを残す）。
            if !session.has_transcript {
                eprintln!(
                    "Skipping the requested summarization because the session has no transcript"
                );
                return;
            }
            summarizer.submit(manual_summarize_job(&config.borrow(), &session.dir));
            // 投入結果（通常は「生成中」）を即反映し、2 連クリックの多重投入を防ぐ。
            if let Some(rec) = rec_weak.upgrade() {
                refresh_detail_summary_status(&rec, &summarizer, session);
            }
        });
    }

    // 「Cancel」: キュー待ちの要約ジョブを取り消す（走り出したものは取り消せない。理由は
    // `SummarizeWorker::cancel` の doc）。ボタンはキュー待ちの間だけ Cancel になる。
    {
        let sessions = Rc::clone(&sessions);
        let summarizer = summarizer.clone();
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_cancel_summary(move |index| {
            let sessions = sessions.borrow();
            let Some(session) = usize::try_from(index).ok().and_then(|i| sessions.get(i)) else {
                return;
            };
            if !summarizer.cancel(&session.dir) {
                // キュー待ちでなくなっていた（走り出した・既に終わった）。tick が状態を
                // 更新する前の数十 ms に押されると起こる。表示は次の tick が直す。
                eprintln!("Skipping the cancellation because the summary is no longer queued");
            }
            // 取り消し結果（通常は未生成／生成済みへ戻る）を即反映する。
            if let Some(rec) = rec_weak.upgrade() {
                refresh_detail_summary_status(&rec, &summarizer, session);
            }
        });
    }

    // 確認モーダルの Delete: 選択中セッションをディレクトリごと OS のゴミ箱へ移動し、
    // 一覧・メモリの両方から除去する（完全削除への自動フォールバックはしない）。
    // 失敗はログのみでアプリ・一覧を壊さない（`docs/rules/error-handling.md`）。
    {
        let sessions = Rc::clone(&sessions);
        let sessions_model = Rc::clone(&sessions_model);
        let player = Rc::clone(&player);
        let transcript_segments = Rc::clone(&transcript_segments);
        let load_generation = Rc::clone(&load_generation);
        let transcriber = transcriber.clone();
        let summarizer = summarizer.clone();
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_delete_session(move |index| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            let Some(i) = usize::try_from(index).ok() else {
                return;
            };
            // 境界チェックと要素取得を get(i) で一体にする（他ハンドラと同じパターン）。
            // 失敗時の積み直しに使う再生対象パスもここでまとめて取り出す。
            let Some((dir, playback_path)) = sessions
                .borrow()
                .get(i)
                .map(|s| (s.dir.clone(), s.playback_path()))
            else {
                return;
            };
            // **ゴミ箱へ移す前にワーカーの状態を確かめる**。UI のゲート（`detail-files-in-use`）は
            // 100ms の tick 遅れがあるので、「表示はキュー待ちだが、もう走り出している」瞬間に
            // Delete を押せてしまう。走行中のセッションを消すと、ワーカーは消えたディレクトリへ
            // 書きに行って失敗し、`forget` の後から状態を入れ直す（消えたセッションの記録が
            // 残る）。キュー待ちなら**先に取り消してから**消す（判定と取り消しは
            // `cancel_queued` が 1 回のロックでまとめて行う）。
            // 要約だけを見るのは、要約が**ワーカー経由でも投入される**（文字起こしの完了から
            // 自動生成）ため。文字起こしは投入が UI スレッドで、直後に
            // `refresh_detail_transcript_status` がゲートを閉じるので、この窓が開かない。
            // なお、このチェックの**後**に自動投入が届く窓は残る。その場合はワーカーが
            // 取り出したときに文字起こしが無く `Skipped` で消える（`summary.md` を書く前に
            // 返るので、消えたディレクトリへは書きに行かない）。
            match summarizer.cancel_queued(&dir) {
                summarize::CancelOutcome::Running => {
                    eprintln!("Skipping the deletion because the summary is still running");
                    return;
                }
                // 取り消したジョブは戻せない（`SummarizeJob` は破棄済み）。下でゴミ箱への
                // 移動に失敗しても、積んでいた生成は失われたままになる（選択し直して
                // Summarize を押せば積み直せる）。
                summarize::CancelOutcome::Cancelled | summarize::CancelOutcome::NotQueued => {}
            }
            // ゴミ箱への移動前に再生対象を手放す。削除済みファイルを play_pause / seek の
            // 開き直し経路が参照しないようにし、開いたままのハンドルが移動を妨げる OS でも
            // 失敗しないようにする。
            if let Some(p) = player.borrow_mut().as_mut() {
                p.unload();
            }
            if let Err(err) = move_recording_to_trash(&dir) {
                // trash::Error の Display/Debug はフルパスを含みうるため、ログへは流さず、
                // セッション名（日時ディレクトリ名）とパスを含まない種別だけを出す
                // （`docs/rules/security.md`）。
                let name = dir.file_name().map(|n| n.to_string_lossy());
                eprintln!(
                    "Skipping the deletion because moving the recording to the Trash failed \
                     (session: {}, reason: {})",
                    name.as_deref().unwrap_or("unknown"),
                    trash_error_kind(&err)
                );
                // 事前に手放した再生対象を積み直し、「選択中なのに再生が沈黙する」不整合を
                // 残さない（ベストエフォート。失敗はログのみで選択し直せば回復する）。
                let reloaded = match (&playback_path, player.borrow_mut().as_mut()) {
                    (Some(path), Some(p)) => match p.load(path) {
                        Ok(()) => true,
                        Err(err) => {
                            eprintln!("Failed to reload the recording for playback: {err}");
                            false
                        }
                    },
                    _ => false,
                };
                if !reloaded {
                    // 未ロードのままだと再生 tick が表示を駆動しないため、ここで停止表示へ
                    // 確定させる（選択中のまま「再生中の表示が固まる」ことを防ぐ）。
                    rec.set_playing(false);
                    apply_playback_position(&rec, Duration::ZERO, None);
                    rec.set_seekable(false);
                }
                return;
            }
            sessions.borrow_mut().remove(i);
            sessions_model.remove(i);
            // 削除で**隣接行の見出しと合計が変わる**。見出しは直前の行との比較で決まるので、
            // その日の先頭を消すと繰り上がった行が見出しを引き継ぐ必要がある。
            {
                let sessions = sessions.borrow();
                rec.set_library_summary(library_summary(sessions.len()).into());
                if let Some(mut row) = sessions_model.row_data(i) {
                    row.group_heading =
                        session_group_heading(&sessions, i, chrono::Local::now().naive_local())
                            .into();
                    sessions_model.set_row_data(i, row);
                }
            }
            // 進行状況マップに残ったエントリを掃除する（削除済みセッションの記録を残さない）。
            transcriber.forget(&dir);
            summarizer.forget(&dir);
            clear_recordings_selection(&rec, &transcript_segments, &load_generation);
        });
    }

    // 機能ごとの設定ウィンドウ（#141）。設定画面の「扉」から開く。設定・Recordings と同じく
    // 起動時に生成して隠しておき、閉じても常駐を保つ。
    //
    // **走査は 2 つで共有する**（`ModelLists`）。両方開いていてもディスクを見るのは 1 回だけで、
    // 片方での削除・選択は両方の素材へ同時に反映される（カタログ外ファイルは両方に出るため、
    // 片側だけ作り直すと消えた行が残る）。
    let model_lists = windows::models::ModelLists::new();

    // 「作り直して、生きているウィンドウへ反映する」処理。**操作した側と巻き込まれた側で理由を
    // 変える**（巻き込まれた側は並びが変わるのでモーダルは畳むが、通知は触らない——触っていない
    // 画面に他人の失敗を出さない／出ていた通知を黙って消さない）。
    let refresh_lists: windows::models::RefreshSlot = Rc::new(RefCell::new(None));

    let model_workers = windows::models::ModelWorkers {
        lists: model_lists.clone(),
        downloader: model_downloader.clone(),
        transcriber: transcriber.clone(),
        summarizer: summarizer.clone(),
    };
    let transcription_ui = windows::transcription::build(&config, &model_workers, {
        let cell = Rc::clone(&refresh_lists);
        Rc::new(move |cause, origin| {
            if let Some(refresh) = cell.borrow().clone() {
                refresh(cause, origin);
            }
        })
    });
    let minutes_ui = windows::minutes::build(&config, &model_workers, {
        let cell = Rc::clone(&refresh_lists);
        Rc::new(move |cause, origin| {
            if let Some(refresh) = cell.borrow().clone() {
                refresh(cause, origin);
            }
        })
    });
    // 閉じる（＝隠す）ときに**確認モーダルを畳む**。開いたまま隠すと、その間の作り直しで並びが
    // 変わっても畳まれず（tick のガードは表示中のウィンドウしか見ない）、再表示したときに
    // **古い添字を指したモーダル**が出る。
    for (window, folder) in [
        (
            transcription_ui.window(),
            Box::new({
                let weak = transcription_ui.as_weak();
                move || {
                    if let Some(window) = weak.upgrade() {
                        window.set_show_delete_confirm(false);
                        window.set_delete_index(0);
                    }
                }
            }) as Box<dyn Fn()>,
        ),
        (
            minutes_ui.window(),
            Box::new({
                let weak = minutes_ui.as_weak();
                move || {
                    if let Some(window) = weak.upgrade() {
                        window.set_show_delete_confirm(false);
                        window.set_delete_index(0);
                    }
                }
            }) as Box<dyn Fn()>,
        ),
    ] {
        window.on_close_requested(move || {
            folder();
            slint::CloseRequestResponse::HideWindow
        });
    }

    // 実体を入れる（2 つのウィンドウを作ってからでないと `Weak` が取れないので、後入れにする）。
    *refresh_lists.borrow_mut() = Some({
        let transcription_weak = transcription_ui.as_weak();
        let minutes_weak = minutes_ui.as_weak();
        let ui_weak = ui.as_weak();
        let workers = model_workers.clone();
        let config = Rc::clone(&config);
        Rc::new(
            move |cause: ModelsRefresh, origin: windows::models::ListOrigin| {
                let polling = matches!(cause, ModelsRefresh::Poll);
                let scan_notice = if polling {
                    None
                } else {
                    // 走査はここだけ。**ビューを見ない**ので、片方のウィンドウが取れなくても
                    // 素材とラッチは揃う（`rescan_model_lists` の doc）。
                    windows::models::rescan_model_lists(
                        &workers.lists,
                        &workers.downloader,
                        &config.borrow(),
                    )
                };
                let cause = windows::models::refresh_cause(cause, scan_notice);
                // **操作元だけが通知を差し替える**。巻き込まれた側はモーダルを畳むだけにする
                // （触っていない画面に他人の失敗を出さない／出ていた通知を黙って消さない）。
                let (for_transcription, for_minutes) = match origin {
                    windows::models::ListOrigin::Transcription => (cause, cause.elsewhere()),
                    windows::models::ListOrigin::Minutes => (cause.elsewhere(), cause),
                    // tick から来る理由は通知に触らないので、どちらへ配っても同じ。
                    windows::models::ListOrigin::Tick => (cause, cause),
                };
                // **表示していないウィンドウの行は組み直さない**（行ごとにワーカーのロックを取るので、
                // 100ms tick で 2 画面ぶん回すのは無駄。素材とラッチは上の走査で揃っているから、
                // 次に開くときの `AfterOperation` で追いつく）。
                if let Some(window) = transcription_weak.upgrade()
                    && window.window().is_visible()
                {
                    windows::models::apply_rows(
                        &window,
                        &workers.lists.transcription,
                        &workers,
                        &config.borrow(),
                        for_transcription,
                    );
                    if !polling {
                        windows::transcription::apply_settings(&window, &config.borrow());
                    }
                }
                if let Some(window) = minutes_weak.upgrade()
                    && window.window().is_visible()
                {
                    windows::models::apply_rows(
                        &window,
                        &workers.lists.minutes,
                        &workers,
                        &config.borrow(),
                        for_minutes,
                    );
                    if !polling {
                        windows::minutes::apply_settings(&window, &config.borrow());
                    }
                }
                // 設定画面の扉も追従させる（選択・ON/OFF が変わると要約行と状態行が変わる）。
                // **tick の `Poll` では触らない**——扉は tick 自身が別経路で追従させているので、
                // ここで組み直すと 100ms ごとにカタログ全件のロックと文字列生成が二重に走る。
                if let Some(ui) = ui_weak.upgrade()
                    && !polling
                {
                    windows::transcription::apply_door(&ui, &config.borrow(), &workers.downloader);
                    windows::minutes::apply_door(&ui, &config.borrow(), &workers.downloader);
                }
            },
        ) as windows::models::RefreshLists
    });

    // 設定画面の扉。開くたびに一覧を作り直す（走査の入口は `refresh_lists` の 1 つ）。
    {
        let transcription_weak = transcription_ui.as_weak();
        let refresh = Rc::clone(&refresh_lists);
        // 初回表示でジオメトリを確定させたか（`show_window` が `&mut bool` を取るので RefCell）。
        let geometry = RefCell::new(false);
        ui.on_open_transcription_window(move || {
            let Some(window) = transcription_weak.upgrade() else {
                return;
            };
            // **見せてから作り直す**。作り直しは表示中のウィンドウにしか行を流さないので、
            // 順番が逆だと開いた直後の一覧が空のままになる（次の tick まで埋まらない）。
            show_window(
                window.window(),
                &mut geometry.borrow_mut(),
                slint::LogicalPosition::new(TRANSCRIPTION_X, TRANSCRIPTION_Y),
                slint::LogicalSize::new(TRANSCRIPTION_WIDTH, TRANSCRIPTION_HEIGHT),
            );
            if let Some(refresh) = refresh.borrow().clone() {
                refresh(
                    ModelsRefresh::AfterOperation(None),
                    windows::models::ListOrigin::Transcription,
                );
            }
        });
    }
    {
        let minutes_weak = minutes_ui.as_weak();
        let refresh = Rc::clone(&refresh_lists);
        let geometry = RefCell::new(false);
        ui.on_open_minutes_window(move || {
            let Some(window) = minutes_weak.upgrade() else {
                return;
            };
            // **見せてから作り直す**。作り直しは表示中のウィンドウにしか行を流さないので、
            // 順番が逆だと開いた直後の一覧が空のままになる（次の tick まで埋まらない）。
            show_window(
                window.window(),
                &mut geometry.borrow_mut(),
                slint::LogicalPosition::new(MINUTES_X, MINUTES_Y),
                slint::LogicalSize::new(MINUTES_WIDTH, MINUTES_HEIGHT),
            );
            if let Some(refresh) = refresh.borrow().clone() {
                refresh(
                    ModelsRefresh::AfterOperation(None),
                    windows::models::ListOrigin::Minutes,
                );
            }
        });
    }
    // 議事録ウィンドウの注意書きから文字起こしウィンドウへ渡れるようにする（従属の理由を
    // 読んだその場で直せるように）。
    {
        let ui_weak = ui.as_weak();
        minutes_ui.on_open_transcription(move || {
            if let Some(ui) = ui_weak.upgrade() {
                ui.invoke_open_transcription_window();
            }
        });
    }

    // トレイのメニューイベントを Slint のイベントループ上でポーリングし、
    // ウィンドウ操作・終了へ橋渡しする。
    let timer = slint::Timer::default();
    timer.start(
        slint::TimerMode::Repeated,
        MENU_POLL_INTERVAL,
        build_menu_event_handler(
            ui.as_weak(),
            RecordingsHandles {
                ui: recordings_ui.as_weak(),
                player: Rc::clone(&player),
                load_receiver,
                load_generation: Rc::clone(&load_generation),
                sessions: Rc::clone(&sessions),
                sessions_model: Rc::clone(&sessions_model),
                transcript_segments: Rc::clone(&transcript_segments),
                transcriber: transcriber.clone(),
                summarizer: summarizer.clone(),
            },
            ModelsHandles {
                transcription: transcription_ui.as_weak(),
                minutes: minutes_ui.as_weak(),
                lists: model_lists.clone(),
                refresh: Rc::clone(&refresh_lists),
                downloader: model_downloader.clone(),
            },
            &tray,
            Rc::clone(&config),
            postprocessor,
            #[cfg(target_os = "macos")]
            app_monitor,
        ),
    );

    // Dock 非表示はイベントループ開始後に適用する必要があるため、ここで一度だけ予約する
    // （なぜループ開始後かは `hide_dock_icon` の doc コメント参照）。
    #[cfg(target_os = "macos")]
    if let Err(err) = slint::invoke_from_event_loop(hide_dock_icon) {
        eprintln!("Failed to schedule hiding the Dock icon: {err}");
    }

    // run_event_loop() は「最後のウィンドウが閉じ、かつ最後の Slint の SystemTrayIcon が
    // 隠れた」時点で return する。本アプリのトレイは tray-icon クレート製で Slint からは
    // 見えないため、ウィンドウを隠すと「表示物ゼロ」と判定されてループが終了し、プロセスが
    // 落ちてしまう。常駐を保つため until_quit 版を使い、終了は quit_event_loop() だけに限る。
    slint::run_event_loop_until_quit()?;

    // イベントループ終了後、トレイを明示的に解放してアイコンを残さない。
    drop(timer);
    drop(tray);
    Ok(())
}

/// Recordings ウィンドウの操作・再生に必要なハンドル一式。`build_menu_event_handler` の引数を
/// 増やしすぎないためにまとめる。
struct RecordingsHandles {
    ui: slint::Weak<RecordingsWindow>,
    player: Rc<RefCell<Option<player::AudioPlayer>>>,
    /// 選んだ録音の読み込み結果の受け口（#152）。**tick が拾って反映する**——読み込みスレッドは
    /// UI スレッド専有のものに触れないので、送るだけにしてある。
    load_receiver: std::sync::mpsc::Receiver<LoadedSession>,
    /// いま表示している選択の世代。届いた結果がこれと違えば**捨てる**（速く切り替えたときに、
    /// 前の読み込みがあとから返って新しい選択を上書きするのを防ぐ）。
    load_generation: Rc<Cell<u64>>,
    sessions: Rc<RefCell<Vec<recordings::RecordingSession>>>,
    sessions_model: Rc<slint::VecModel<SessionRow>>,
    transcript_segments: Rc<RefCell<Vec<transcript::TranscriptSegment>>>,
    transcriber: transcribe::TranscribeWorker,
    /// 詳細ペインの要約状態を tick で追従させるために読む（#81）。
    summarizer: summarize::SummarizeWorker,
}

/// 機能ウィンドウを tick で追従させるために必要なハンドル一式（`RecordingsHandles` と
/// 同じ理由でまとめる）。
struct ModelsHandles {
    transcription: slint::Weak<TranscriptionWindow>,
    minutes: slint::Weak<MinutesWindow>,
    /// 2 つの一覧の素材と、共有する走査の状態（`ModelLists`）。
    lists: windows::models::ModelLists,
    /// 「作り直して、生きているウィンドウへ反映する」処理（`main` が組み立てたもの）。
    /// **走査の入口をこの 1 つに絞る**ため、tick も同じ関数を通す。
    refresh: windows::models::RefreshSlot,
    /// モデルの取得状況（扉の状態行と、走査し直す契機の判定が読む）。
    downloader: model_download::ModelDownloader,
}

/// メニューイベントの処理と、録音中のメニューバー表示更新を毎ティック行うクロージャを作る。
///
/// 表示/非表示トグルや録音トグルは現在の状態（ウィンドウの可視状態・録音セッションの有無）から
/// 判断し、別途フラグを持たない（「ありえない状態」を作らないため）。
///
/// macOS では毎ティックで自動録音の開始／停止も駆動する: `app_monitor` の登録アプリのマイク使用の
/// 立ち上がりで（設定 ON・未録音なら）開始し、その録音は登録アプリのマイク使用の途絶がデバウンス
/// 継続したところで自動停止する。
///
/// 録音セッション（`Option<Recorder>`）と `cpal::Stream`(`!Send`)、および `app_monitor` は
/// このクロージャ内で所有する。クロージャはメインスレッド（Slint イベントループ）上でのみ
/// 実行されるため問題ない。
fn build_menu_event_handler(
    ui: slint::Weak<AppWindow>,
    recordings: RecordingsHandles,
    models: ModelsHandles,
    tray: &Tray,
    config: Rc<RefCell<Config>>,
    postprocessor: mixdown::PostProcessWorker,
    #[cfg(target_os = "macos")] app_monitor: app_audio_monitor::AppAudioMonitor,
) -> impl FnMut() + 'static {
    // Recordings ウィンドウ・再生・一覧のハンドルは RecordingsHandles にまとめたまま使う
    // （引数の氾濫を避ける。open_recordings_window にも構造体ごと渡す）。
    // クロージャは 'static のため &Tray を借用できない。必要な要素（各項目・ID・アイコン）
    // だけを複製して所有する。
    let toggle_id = tray.toggle_item.id().clone();
    let recordings_id = tray.recordings_item.id().clone();
    let record_item = tray.record_item.clone();
    let record_id = tray.record_item.id().clone();
    let quit_id = tray.quit_item.id().clone();
    let tray_icon = Rc::clone(&tray.icon);
    let menu_channel = MenuEvent::receiver();
    // 初回表示でジオメトリを確定させたか。2 回目以降は位置・サイズを動かさない。
    let mut geometry_committed = false;
    // Recordings ウィンドウの初回ジオメトリを確定させたか。
    let mut rec_geometry_committed = false;
    // 再生の経過時間テキストを、秒が変わったときだけ更新するための前回値。
    let mut last_play_secs: Option<u64> = None;
    // 実行中の録音セッション。None=待機中、Some=録音中。
    let mut recorder: Option<Recorder> = None;
    // 録音中の経過時間テキストを、秒が変わったときだけ更新するための前回値。
    // アイコンの明滅は毎ティック更新するのでここでは持たない。
    let mut last_rendered_secs: Option<u64> = None;
    // 直前 tick で録音中だったか。録音中→待機の遷移を 1 度だけ拾って待機表示へ戻すのに使う。
    let mut was_recording = false;
    // 実行中の録音が「登録アプリのマイク使用」由来の自動開始か。true のときだけ、登録アプリのマイク使用の途絶で
    // 自動停止する（手動開始の録音は app の沈黙では止めない）。
    #[cfg(target_os = "macos")]
    let mut recording_started_by_app = false;

    move || {
        while let Ok(event) = menu_channel.try_recv() {
            if event.id == toggle_id {
                let Some(ui) = ui.upgrade() else { continue };
                show_window(
                    ui.window(),
                    &mut geometry_committed,
                    slint::LogicalPosition::new(WINDOW_X, WINDOW_Y),
                    slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT),
                );
            } else if event.id == recordings_id {
                let Some(rec) = recordings.ui.upgrade() else {
                    continue;
                };
                open_recordings_window(
                    &rec,
                    &recordings,
                    &config,
                    &mut rec_geometry_committed,
                    &mut last_play_secs,
                );
            } else if event.id == record_id {
                toggle_recording(&mut recorder, &record_item, &config, &postprocessor);
                #[cfg(target_os = "macos")]
                {
                    // 手動トグルの録音は自動停止の対象にしない（開始でも停止でもフラグを下ろす）。
                    recording_started_by_app = false;
                    // 停止だったら開始検知を再初期化する。録音中は app の照会（take_activated）を
                    // 止めて prev_outputting が凍結されるため、再初期化しないと「録音中に出力を
                    // 始めた登録アプリ」を停止直後に立ち上がりとして誤検知して即再録音してしまう
                    // （app 自動停止経路の reset_after_stop と対称にする）。
                    if recorder.is_none() {
                        app_monitor.reset_after_stop();
                    }
                }
            } else if event.id == quit_id
                && let Err(err) = slint::quit_event_loop()
            {
                eprintln!("Failed to quit the event loop: {err}");
            }
        }

        // 登録アプリのマイク使用に連動した自動録音（macOS 14.4+）。未録音なら登録アプリのマイク使用の立ち上がりで
        // 開始する。「登録アプリのマイク使用」由来の録音中なら、登録アプリのいずれもマイクを使わなくなった状態が
        // デバウンス継続したところで自動停止する（通話終了の合図）。設定 OFF／登録なし／照会不能の
        // ときは開始・停止いずれも行わない。照会は録音中・未録音のどちらか一方だけで走る。
        #[cfg(target_os = "macos")]
        {
            let config_ref = config.borrow();
            let enabled = config_ref.auto_record_on_app_mic;
            if recorder.is_none() {
                let activated = app_monitor.take_activated(&config_ref.app_mic_triggers, enabled);
                drop(config_ref);
                if activated {
                    start_recording(&mut recorder, &record_item, &config);
                    // 実際に開始できたときだけ「app 由来」として自動停止の対象にする。
                    recording_started_by_app = recorder.is_some();
                }
            } else if recording_started_by_app {
                let debounce = config_ref.auto_stop_debounce();
                let stop = app_monitor.should_stop(&config_ref.app_mic_triggers, enabled, debounce);
                drop(config_ref);
                if stop {
                    stop_recording(&mut recorder, &record_item, &config, &postprocessor);
                    recording_started_by_app = false;
                    // 停止後は開始検知を再初期化する（録音中に出力を始めたアプリを誤検知しない）。
                    app_monitor.reset_after_stop();
                }
            }
        }

        // 録音中はメニューバーへ経過時間と明滅を反映する。100ms ポーリング（≈10fps）に相乗りし、
        // アイコンは毎ティック明度レベルを更新して滑らかに明滅させる。経過時間テキストは
        // 秒が変わったときだけ更新して無駄な再設定を避ける。
        if let Some(session) = recorder.as_ref() {
            let elapsed = session.elapsed();
            let level = breathing_level(elapsed, BLINK_CYCLE_SECS);
            let secs = elapsed.as_secs();
            let update_title = last_rendered_secs != Some(secs);
            tray::render_recording(&tray_icon, elapsed, level, update_title);
            last_rendered_secs = Some(secs);
            was_recording = true;
        } else if was_recording {
            // 録音中→待機へ移った最初の tick。待機表示へ戻し、表示状態をリセットする。
            tray::set_idle(&tray_icon);
            last_rendered_secs = None;
            was_recording = false;
        }

        // Recordings ウィンドウが開いている間だけ、再生の経過時間・進捗・再生状態を反映する
        // （閉じているときは更新しない＝アイドル時の無駄な描画をしない）。再生対象が
        // ロードされていない間は駆動しない: 表示は選択時に確定済みで、上書きするとユーザーが
        // クリックしたセグメントのハイライトを 100ms 後に奪ってしまう。
        if let Some(rec) = recordings.ui.upgrade()
            && rec.window().is_visible()
            && let Some(player) = recordings.player.borrow().as_ref()
            && player.is_loaded()
        {
            let position = player.position();
            let duration = player.duration();
            if rec.get_scrubbing() {
                // シークバーのドラッグ中はプレビュー表示を上書きしない（分担は `.slint` の
                // `SeekBar` の doc コメント参照）。経過秒の記録は捨てて、ドラッグ終了後の
                // tick が必ず時刻表示を出し直すようにする。
                last_play_secs = None;
            } else {
                let secs = position.as_secs();
                if last_play_secs != Some(secs) {
                    rec.set_time_text(format_playback_time(position, duration).into());
                    last_play_secs = Some(secs);
                }
                rec.set_progress(playback_progress(position, duration));
            }
            rec.set_playing(player.is_playing());
            // 再生位置に対応するトランスクリプトのセグメントをハイライトする（該当なしは -1）。
            let current = transcript::current_index(
                &recordings.transcript_segments.borrow(),
                position.as_secs_f64(),
            )
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1);
            rec.set_current_segment(current);
        }

        // 読み込みが終わった録音を表示へ入れる（#152）。**ウィンドウが閉じていても受け取る**——
        // 受け口に溜めたままにすると、次に開いたときに古い結果がまとめて流れ込む。
        while let Ok(loaded) = recordings.load_receiver.try_recv() {
            // **遅れて届いた結果は捨てる**。速く切り替えると前の読み込みがあとから返り、
            // いま選んでいる録音を別の録音の中身で上書きしてしまう。
            if recordings.load_generation.get() != loaded.generation {
                continue;
            }
            if let Some(rec) = recordings.ui.upgrade() {
                apply_loaded_session(
                    &rec,
                    &recordings.player,
                    &recordings.transcript_segments,
                    loaded,
                );
            }
        }

        // Recordings ウィンドウが開いている間だけ、文字起こし状態の変化を一覧・詳細ペインへ
        // 反映する（変化した行だけ set_row_data して無駄な再描画を避ける）。選択中セッションが
        // 文字起こし中→完了に変わったら、トランスクリプトを読み直して表示を差し替える。
        if let Some(rec) = recordings.ui.upgrade()
            && rec.window().is_visible()
        {
            let selected = usize::try_from(rec.get_selected_index()).ok();
            // 完了を観測して**生成物の有無を書き戻す**セッション。一覧の走査は不変借用で回すので、
            // 借用を手放してから反映する（`sessions` の `has_transcript` / `has_summary` が
            // 生成物の有無の正で、ボタンの活性・状態の解決がここを読む。ウィンドウを開いたまま
            // 実行したときに UI とメモリがずれないように、UI だけを直す形にはしない）。
            let mut transcribed: Vec<usize> = Vec::new();
            // 要約の追従は選択中セッションだけを見るので、書き戻す対象も高々 1 件。
            let mut summarized: Option<usize> = None;
            {
                let sessions_ref = recordings.sessions.borrow();
                for (i, session) in sessions_ref.iter().enumerate() {
                    let Some(mut row) = recordings.sessions_model.row_data(i) else {
                        continue;
                    };
                    let status = transcript_display_status(
                        recordings.transcriber.status_of(&session.dir),
                        session.has_transcript,
                    );
                    if row.transcript_status == status {
                        continue;
                    }
                    let previous = row.transcript_status;
                    row.transcript_status = status;
                    // **文言も一緒に組み直す**。行は状態を enum と文字列の 2 つで持っているので、
                    // 片方だけ更新すると「完了したのに `transcribing` のまま」が残る。
                    row.detail_text = session_detail_text(session, status).into();
                    recordings.sessions_model.set_row_data(i, row);
                    if previous == TranscriptStatus::Transcribing
                        && status == TranscriptStatus::Done
                    {
                        transcribed.push(i);
                    }
                    if selected == Some(i) {
                        apply_detail_transcript_status(&rec, status);
                        if previous == TranscriptStatus::Transcribing
                            && status == TranscriptStatus::Done
                        {
                            let segments = transcript::load_transcript(&session.dir);
                            rec.set_segments(
                                Rc::new(slint::VecModel::from(transcript_rows(&segments))).into(),
                            );
                            rec.set_current_segment(-1);
                            *recordings.transcript_segments.borrow_mut() = segments;
                        }
                    }
                }

                // 選択中セッションの要約状態も追従させる。一覧行には要約のインジケータが無い
                // （#81 のスコープ外）ので、行の差分更新ではなく詳細ペインの現在値と比べる。
                // **見ているのは選択中セッションだけ**: 他セッションの `has_summary` は次に
                // 一覧を開くまで古いままだが、状態の解決はワーカーの状態マップを優先するので
                // 表示は正しい（`summary_display_status`）。
                if let Some(i) = selected
                    && let Some(session) = sessions_ref.get(i)
                {
                    let status = summary_display_status(
                        recordings.summarizer.status_of(&session.dir),
                        session.has_summary,
                    );
                    let previous = rec.get_detail_summary_status();
                    if previous != status {
                        apply_detail_summary_status(&rec, status);
                        // 生成が終わった瞬間に表示を差し替える（失敗時は前の議事録を残す）。
                        // 通常は生成中を経るが、tick の間隔より短い経路もありうるので
                        // キュー待ちからの完了も拾う。
                        if matches!(previous, SummaryStatus::Queued | SummaryStatus::Summarizing)
                            && status == SummaryStatus::Done
                        {
                            refresh_detail_summary_rows(&rec, &session.dir);
                            summarized = Some(i);
                        }
                    }
                }
            }

            if !transcribed.is_empty() || summarized.is_some() {
                let mut sessions_mut = recordings.sessions.borrow_mut();
                for i in transcribed {
                    if let Some(session) = sessions_mut.get_mut(i) {
                        session.has_transcript = true;
                    }
                }
                if let Some(session) = summarized.and_then(|i| sessions_mut.get_mut(i)) {
                    session.has_summary = true;
                }
                // 選択中セッションのボタン活性（Summarize は文字起こしの有無で決まる）を、
                // 書き戻した値から更新する。
                if let Some(session) = selected.and_then(|i| sessions_mut.get(i)) {
                    rec.set_has_transcript(session.has_transcript);
                }
            }
        }

        // 設定ウィンドウが開いている間だけ、扉の文言（構成・状態）を追従させる。
        //
        // **前回値を別に覚えない**。扉は機能ウィンドウ側の操作でも書き換わるので、こちらの記憶と
        // UI が食い違いうる（覚えたままだと「導出値＝記憶」で一致してスキップし、古い表示が
        // 固定される）。UI の現在値と比べれば、誰が書いた後でも正しく追いつく。
        if let Some(ui) = ui.upgrade()
            && ui.window().is_visible()
        {
            windows::transcription::apply_door(&ui, &config.borrow(), &models.downloader);
            windows::minutes::apply_door(&ui, &config.borrow(), &models.downloader);
        }

        // 機能ウィンドウが開いている間だけ、行の状態（取得の進捗・完了・失敗、ジョブの開始・
        // 終了）を追従させる。**ディスクは走査しない**（状態は状態マップだけで分かる。
        // `docs/rules/performance.md`）。取得が完了した行だけは実サイズと合計を追いつかせたい
        // ので、**記録が増えたときに 1 回だけ**走査し直す（毎 tick 走査しないためのラッチ。
        // 「記録は取得済みだが実体が無い」を条件にすると、外部でファイルを消された場合などに
        // 条件が解消せず走査が止まらない）。
        // **確認モーダルのガードは「表示中の」ウィンドウだけで取る**。隠したウィンドウに残った
        // フラグまで見ると、そのモーダルが走査を恒久的に止めてしまう（隠すときに畳んでいるので
        // 実際には残らないが、ガードをそれに依存させない）。
        // 型が違うので個別に畳む（`Some(モーダルが開いているか)` = 表示中、`None` = 隠れている）。
        let shown_modals = [
            models
                .transcription
                .upgrade()
                .filter(|window| window.window().is_visible())
                .map(|window| window.get_show_delete_confirm()),
            models
                .minutes
                .upgrade()
                .filter(|window| window.window().is_visible())
                .map(|window| window.get_show_delete_confirm()),
        ];
        if shown_modals.iter().any(Option::is_some) {
            let modal_open = shown_modals.iter().flatten().any(|open| *open);
            // ラッチの比較・消費は 1 か所（`ModelLists`）。消費したら両方の素材を同時に作り直す。
            let rescan = models.lists.downloads_changed(&models.downloader) && !modal_open;
            if let Some(refresh) = models.refresh.borrow().clone() {
                refresh(
                    if rescan {
                        ModelsRefresh::Rescan
                    } else {
                        ModelsRefresh::Poll
                    },
                    windows::models::ListOrigin::Tick,
                );
            }
        }
    }
}

/// 一覧の行に出す見出し（その日の最初の行だけが持つ）。
///
/// **直前の行と比べて決める**。見出しを別の配列に分けると、行の添字とセッションの添字がずれて
/// 別の録音を選ぶことになる（`SessionRow` の doc）。
fn session_group_heading(
    sessions: &[recordings::RecordingSession],
    index: usize,
    now: chrono::NaiveDateTime,
) -> String {
    let Some(session) = sessions.get(index) else {
        return String::new();
    };
    // 直前と同じ日なら出さない（同じ語が並ぶと、どこで日が変わったか分からない）。**日付で
    // 比べる**——文言で比べると、比較のためだけに全行ぶんの文字列を作って捨てることになる。
    let same_day = index
        .checked_sub(1)
        .and_then(|prev| sessions.get(prev))
        .is_some_and(|previous| previous.date() == session.date());
    if same_day {
        return String::new();
    }
    session.group_heading(now)
}

/// 一覧の行の 3 行目（`Mic + system · transcribed`）。音源と文字起こしの状態を 1 行にまとめる。
///
/// **行の高さを固定してある**ので、ここは 1 行に収める（溢れたらクリップされる）。
fn session_detail_text(session: &recordings::RecordingSession, status: TranscriptStatus) -> String {
    // 音源の語は `source_summary` の 1 箇所に持つ（詳細ヘッダと削除の確認も同じ語を使うので、
    // ここで別の表を持つと片方だけ直って表記が割れる）。
    format!(
        "{} · {}",
        session.source_summary(),
        session_transcript_word(status)
    )
}

/// 一覧の行に出す文字起こしの状態（**網羅 match**。状態を足したら語を決めるまで通らない）。
fn session_transcript_word(status: TranscriptStatus) -> &'static str {
    match status {
        TranscriptStatus::NotTranscribed => "not transcribed",
        TranscriptStatus::Transcribing => "transcribing",
        TranscriptStatus::Done => "transcribed",
        TranscriptStatus::Failed => "transcription failed",
    }
}

/// 一覧の下端に出す合計。**件数だけ**にする——容量を出すには全セッションのファイルを開く必要が
/// あり、一覧を開くたびに走らせるには重い。
fn library_summary(count: usize) -> String {
    match count {
        1 => "1 recording".to_owned(),
        count => format!("{count} recordings"),
    }
}

/// 選んだ録音の**重い読み込みの結果**（別スレッドで作り、UI スレッドへ渡す）。
///
/// Slint の型を持たせないのは、生成をイベントループの外で行うため。UI へ入れる形への変換は
/// `apply_loaded_session` が行う。
struct LoadedSession {
    /// どの選択に対する結果か。**受け取る側が世代を確かめて、古い結果を捨てる**。
    generation: u64,
    segments: Vec<transcript::TranscriptSegment>,
    summary: Option<String>,
    /// 再生の準備（対象が無い・開けなかったときは `None`）。
    playback: Option<player::PreparedSource>,
}

/// 読み込みの結果を表示へ入れる（**イベントループ上でだけ呼ぶ**）。
///
/// ここに来た時点で世代の確認は済んでいる。やることは「読み込み中」を解いて、届いた中身を
/// 一度に入れることだけ——段階的に入れると、文字起こしだけ出て再生がまだ、という中途半端な
/// 表示が挟まる。
fn apply_loaded_session(
    rec: &RecordingsWindow,
    player: &Rc<RefCell<Option<player::AudioPlayer>>>,
    segments_cell: &Rc<RefCell<Vec<transcript::TranscriptSegment>>>,
    loaded: LoadedSession,
) {
    let LoadedSession {
        segments,
        summary,
        playback,
        ..
    } = loaded;

    rec.set_segments(Rc::new(slint::VecModel::from(transcript_rows(&segments))).into());
    *segments_cell.borrow_mut() = segments;
    let summary_rows = summary.map(|text| summary_rows(&text)).unwrap_or_default();
    rec.set_summary_rows(Rc::new(slint::VecModel::from(summary_rows)).into());

    let duration = playback.as_ref().and_then(player::PreparedSource::duration);
    // **開けたかどうかが「再生できる」の答え**。両音源で mix.mp3 が未生成のセッションや、
    // ファイルを開けなかったセッションはここで `None` になる（選択時にその場でミックスして
    // UI を固めることはしない）。
    let playable = playback.is_some();
    if let Some(prepared) = playback
        && let Some(p) = player.borrow_mut().as_mut()
    {
        p.adopt(prepared);
    }
    rec.set_playable(playable);
    apply_playback_position(rec, Duration::ZERO, duration);
    // 全体長が分からないと比率→秒の換算ができないため、その場合はシークバーを表示専用に
    // 縮退させる。
    rec.set_seekable(playable && duration.is_some());
    rec.set_loading(false);
}

/// セッションディレクトリを OS のゴミ箱へ移動する。macOS では `NsFileManager` 方式を明示する:
/// `trash` の既定（Finder 方式）は osascript の子プロセス経由で Finder を操作するため、
/// 初回に Automation 権限プロンプトが出て、拒否されると以後の削除が全て失敗するうえ、
/// 録音のフルパスが子プロセスの引数へ渡る（`docs/rules/security.md`）。NsFileManager 方式は
/// 追加権限も子プロセスも不要で同じ「ゴミ箱へ移動」になる。
fn move_recording_to_trash(dir: &std::path::Path) -> Result<(), trash::Error> {
    #[cfg(target_os = "macos")]
    {
        use trash::TrashContext;
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut ctx = TrashContext::default();
        ctx.set_delete_method(DeleteMethod::NsFileManager);
        ctx.delete(dir)
    }
    #[cfg(not(target_os = "macos"))]
    {
        trash::delete(dir)
    }
}

/// ゴミ箱移動の失敗理由を、パスを含まない固定文字列に落とす（ログの切り分け用。
/// `trash::Error` のフィールドにはフルパスが入りうるため出力しない）。
fn trash_error_kind(err: &trash::Error) -> String {
    match err {
        trash::Error::Os { code, .. } => format!("os error {code}"),
        trash::Error::Unknown { .. } => "unknown".to_owned(),
        trash::Error::TargetedRoot => "targeted a root folder".to_owned(),
        trash::Error::CouldNotAccess { .. } => "could not access the target".to_owned(),
        trash::Error::CanonicalizePath { .. } => "could not canonicalize the path".to_owned(),
        trash::Error::ConvertOsString { .. } => "could not convert the path string".to_owned(),
        _ => "other".to_owned(),
    }
}

/// Recordings ウィンドウの選択・再生表示を未選択状態へ初期化する
/// （ウィンドウを開いたとき・セッション削除後に共用する）。
///
/// 表示中だった文字起こし・議事録も手放す: どちらも発話由来の機微データで、詳細ペインが
/// 隠れている間もモデルとして持ち続ける理由が無い（削除したセッションの内容が残らないように。
/// `docs/rules/security.md`）。
/// 選択を解除して、詳細ペインを未選択の状態へ畳む。
///
/// **世代も進める**（#152）。進めないと、解除の直前に始まった読み込みがあとから届いて、選択が
/// 無いのに中身だけ入る（削除した録音の文字起こしが残る、という形で出る）。
fn clear_recordings_selection(
    rec: &RecordingsWindow,
    transcript_segments: &RefCell<Vec<transcript::TranscriptSegment>>,
    load_generation: &Cell<u64>,
) {
    load_generation.set(load_generation.get().wrapping_add(1));
    rec.set_loading(false);
    rec.set_selected_index(-1);
    rec.set_has_selection(false);
    rec.set_playing(false);
    rec.set_seekable(false);
    rec.set_has_transcript(false);
    rec.set_segments(Rc::new(slint::VecModel::<TranscriptRow>::default()).into());
    rec.set_current_segment(-1);
    transcript_segments.borrow_mut().clear();
    rec.set_summary_rows(Rc::new(slint::VecModel::<SummaryRow>::default()).into());
    // 状態も未実施へ畳む（次の選択で必ず上書きされるが、`detail-files-in-use` /
    // `detail-jobs-pending` の入力なので前の
    // セッションの「実行中」を持ち越さない）。文字起こし・要約で対称にする。
    apply_detail_transcript_status(rec, TranscriptStatus::NotTranscribed);
    apply_detail_summary_status(rec, SummaryStatus::NotSummarized);
    apply_playback_position(rec, Duration::ZERO, None);
}

/// トレイの「Recordings…」で Recordings ウィンドウを開く。保存先を走査して一覧を更新し、
/// 選択・再生状態を初期化してから表示する（初回表示はジオメトリを明示する。`docs/rules/slint.md`）。
fn open_recordings_window(
    rec: &RecordingsWindow,
    handles: &RecordingsHandles,
    config: &Rc<RefCell<Config>>,
    geometry_committed: &mut bool,
    last_play_secs: &mut Option<u64>,
) {
    let list = recordings::list_sessions(&config.borrow().recording_dir);
    // 一覧に出たセッションに取り残された一時ファイルを回収する（強制終了などで残ったもの。
    // 範囲と時期の判断は `recordings::spawn_session_part_sweep` の doc）。表示には使わない
    // 副作用なので、完了は待たない。
    recordings::spawn_session_part_sweep(&list, SystemTime::now());
    let now = chrono::Local::now().naive_local();
    let rows: Vec<SessionRow> = list
        .iter()
        .enumerate()
        .map(|(index, session)| {
            let status = transcript_display_status(
                handles.transcriber.status_of(&session.dir),
                session.has_transcript,
            );
            SessionRow {
                // 見出しは**その日の最初の行だけ**が持つ（直前の行と比べて決める）。行に持たせる
                // 理由は `SessionRow` の doc。
                group_heading: session_group_heading(&list, index, now).into(),
                time_text: session.display_time().into(),
                date_text: session.display_date().into(),
                detail_text: session_detail_text(session, status).into(),
                transcript_status: status,
            }
        })
        .collect();
    handles.sessions_model.set_vec(rows);
    rec.set_library_summary(library_summary(list.len()).into());
    // 開くたびに未選択・停止表示へ初期化する。
    clear_recordings_selection(rec, &handles.transcript_segments, &handles.load_generation);
    *handles.sessions.borrow_mut() = list;
    *last_play_secs = None;
    // 再生ハンドルがあれば前回の再生対象を手放す（未選択表示に合わせて「何もロードされて
    // いない」状態へ揃える。理由は `AudioPlayer::unload` の doc コメント参照）。
    if let Some(p) = handles.player.borrow_mut().as_mut() {
        p.unload();
    }

    show_window(
        rec.window(),
        geometry_committed,
        slint::LogicalPosition::new(RECORDINGS_X, RECORDINGS_Y),
        slint::LogicalSize::new(RECORDINGS_WIDTH, RECORDINGS_HEIGHT),
    );
}

/// トランスクリプトの各セグメントを Slint 表示行へ変換する。表示ラベルと配色判定（is_mic）を
/// 分けて渡す（不正な開始秒の丸めは `TranscriptSegment::start_duration` に集約）。
fn transcript_rows(segments: &[transcript::TranscriptSegment]) -> Vec<TranscriptRow> {
    segments
        .iter()
        .map(|seg| TranscriptRow {
            speaker: seg.speaker.label().into(),
            is_mic: seg.speaker == transcript::Speaker::Mic,
            time: tray::format_elapsed(seg.start_duration()).into(),
            text: seg.text.as_str().into(),
        })
        .collect()
}

/// 再生位置に対応する表示（シークバーの塗りと時刻テキスト）をまとめて更新する。片方だけ
/// 更新して「塗りは新しい位置・時刻は古い位置」という食い違いを作らないよう、対の更新を
/// 1 箇所で保証する。個別に set するのは意図的な 2 経路だけ: 再生 tick（時刻は秒が変わった
/// ときだけ更新して無駄な再設定を避ける）と、ドラッグ中のプレビュー（塗りは Slint 側が
/// プレビュー比率で描くため時刻だけを更新する）。
fn apply_playback_position(rec: &RecordingsWindow, position: Duration, duration: Option<Duration>) {
    rec.set_progress(playback_progress(position, duration));
    rec.set_time_text(format_playback_time(position, duration).into());
}

/// シークバー上の比率（0.0〜1.0）を再生位置へ換算する。全体長が不明なら `None`（シークしない）。
///
/// 比率は Slint 側で丸めてから渡ってくるが、ここでも範囲外・NaN を 0.0〜1.0 に丸める
/// （`Duration::mul_f64` は負値・NaN・オーバーフローでパニックするため、掛ける前に潰す）。
fn seek_position_from_ratio(ratio: f32, duration: Option<Duration>) -> Option<Duration> {
    let total = duration?;
    // f32::clamp は NaN をそのまま返すため、先に 0.0 へ落とす（比率不明は先頭扱い）。
    let ratio = if ratio.is_nan() {
        0.0
    } else {
        ratio.clamp(0.0, 1.0)
    };
    Some(total.mul_f64(f64::from(ratio)))
}

/// 再生位置の進捗比率（0.0〜1.0。シークバーの塗り・つまみの位置）。全体長が不明・0 のときは 0.0。
fn playback_progress(position: Duration, duration: Option<Duration>) -> f32 {
    match duration {
        Some(total) if total > Duration::ZERO => {
            (position.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0)
        }
        _ => 0.0,
    }
}

/// 再生時間の表示文字列（`mm:ss / mm:ss`）。全体長が不明なときは `--:--` を出す。
fn format_playback_time(position: Duration, duration: Option<Duration>) -> String {
    let total = duration
        .map(tray::format_elapsed)
        .unwrap_or_else(|| "--:--".to_string());
    format!("{} / {}", tray::format_elapsed(position), total)
}

/// 録音セッションの有無に応じて、録音の開始／停止を切り替える。録音セッションの開始・停止と
/// メニュー項目のラベル・アイコン切替に専念する。メニューバーのトレイアイコン／経過時間の表示は
/// タイマー closure が録音状態（`Option<Recorder>`）を見て駆動するため、ここでは触らない。
///
/// 失敗してもアプリ（常駐）は落とさず、状態は変えずにログを残す。
fn toggle_recording(
    recorder: &mut Option<Recorder>,
    record_item: &IconMenuItem,
    config: &Rc<RefCell<Config>>,
    postprocessor: &mixdown::PostProcessWorker,
) {
    if recorder.is_none() {
        start_recording(recorder, record_item, config);
    } else {
        stop_recording(recorder, record_item, config, postprocessor);
    }
}

/// 録音セッションを停止する。手動トグルと自動停止（登録アプリのマイク使用の途絶）で共用する
/// （`start_recording` と対称）。stop() が各音源のストリーム停止→flush→ファイル確定まで行う。
/// 録音していなければ何もしない。メニューバーのトレイアイコン／経過時間の表示はタイマー closure が
/// 録音状態を見て駆動するため、ここではメニュー項目のラベル・アイコンを待機表示へ戻すだけにする。
///
/// 保存後、（設定 ON なら）文字起こしをワーカーへ投入し、両音源が保存できていれば Recordings 用の
/// ミックス音声（mix.mp3）生成もワーカーへ投入する（手動・自動どちらの停止経路もここを通る）。
fn stop_recording(
    recorder: &mut Option<Recorder>,
    record_item: &IconMenuItem,
    config: &Rc<RefCell<Config>>,
    postprocessor: &mixdown::PostProcessWorker,
) {
    let Some(session) = recorder.take() else {
        return;
    };
    let saved = session.stop();
    if saved.is_empty() {
        eprintln!("Failed to stop and save the recording (no files were saved)");
    } else {
        // 保存先のフルパスは機微情報（録音データの所在・フォルダ構造がプライバシーに関わる）
        // なので出さない。完了が分かるように、保存できたファイル数だけを知らせる。
        println!("Saved the recording ({} files)", saved.len());
        submit_post_processing(&saved, config, postprocessor);
    }
    tray::set_record_item_idle(record_item);
}

/// 保存済みセッションの後処理（正規化→文字起こし投入→ミックス生成）を組み立てて投入する。
/// 文字起こしの依頼は設定 ON のときだけ添える（オプトイン。モデルは内蔵で、未取得なら
/// ワーカーが自動ダウンロードする）。設定値はここでスナップショットし、処理中の設定変更の
/// 影響を受けない。
fn submit_post_processing(
    saved: &[std::path::PathBuf],
    config: &Rc<RefCell<Config>>,
    postprocessor: &mixdown::PostProcessWorker,
) {
    let Some(session_dir) = saved.first().and_then(|p| p.parent()) else {
        return;
    };
    let config_ref = config.borrow();
    let transcribe = config_ref
        .auto_transcribe
        .then(|| transcribe::TranscribeJob {
            session_dir: session_dir.to_path_buf(),
            audio_paths: saved.to_vec(),
            model_id: config_ref.whisper_model.clone(),
            model_override: config_ref.whisper_model_path.clone(),
            language: config_ref.transcribe_language.clone(),
            summarize: auto_summarize_job(&config_ref, session_dir),
        });
    postprocessor.submit(mixdown::PostProcessJob {
        session_dir: session_dir.to_path_buf(),
        saved: saved.to_vec(),
        transcribe,
    });
}

/// 手動（Recordings ウィンドウの Summarize）の議事録生成の依頼を組み立てる。設定値
/// （モデル・言語）は**ここでスナップショット**し、処理中の設定変更の影響を受けない。
/// エンジンはいまオンデバイスのみ。
///
/// 既存の `summary.md` は現在の文字起こしと整合した有効なデータなので、`existing_is_stale` は
/// `false`（生成に失敗しても失わせない。理由はそのフィールドの doc）。
///
/// 投入は 1 セッション 1 本（実行中は Slint 側でボタンが無効）だが、**セッションをまたいだ
/// 投入は制限しない**（順に選んで押せばキューに積める）。取り消し手段を持たせるかは
/// 後続の検討事項で、#81 のスコープ外。
fn manual_summarize_job(config: &Config, session_dir: &std::path::Path) -> summarize::SummarizeJob {
    summarize::SummarizeJob {
        session_dir: session_dir.to_path_buf(),
        engine: summarize::SummaryEngine::OnDevice,
        model_id: config.summary_model.clone(),
        model_override: config.summary_model_path.clone(),
        language: config.transcribe_language.clone(),
        existing_is_stale: false,
    }
}

/// 文字起こしに添える議事録生成の依頼を組み立てる。設定 OFF なら `None`（要約は走らない）。
///
/// 要約は文字起こし結果を入力にするため、必ず文字起こしジョブへぶら下げる（成功時のみ
/// `TranscribeWorker` が要約ワーカーへ投入する）。この経路では既存の `summary.md` は**前の
/// 文字起こし**の議事録なので `existing_is_stale` を `true` にする。
fn auto_summarize_job(
    config: &Config,
    session_dir: &std::path::Path,
) -> Option<summarize::SummarizeJob> {
    config.auto_summarize.then(|| summarize::SummarizeJob {
        existing_is_stale: true,
        ..manual_summarize_job(config, session_dir)
    })
}

/// 録音セッションを開始する。手動トグルと自動開始（登録アプリのマイク使用検知）で共用する。
///
/// 保存先は設定の現在値を使う。セッションごとに `<保存先>/<日時>` のディレクトリを作り、その中に
/// 音源（将来は文字起こしも）をまとめる。日時はローカル時刻で衝突を避ける。既に録音中なら何もしない
/// （多重開始を防ぐ）。失敗してもアプリ（常駐）は落とさず、状態は変えずにログを残す。
/// トレイアイコン／経過時間の表示はタイマー closure が録音状態を見て駆動するため、ここでは触らない。
fn start_recording(
    recorder: &mut Option<Recorder>,
    record_item: &IconMenuItem,
    config: &Rc<RefCell<Config>>,
) {
    if recorder.is_some() {
        return;
    }
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let session_dir = config.borrow().recording_dir.join(&timestamp);
    match Recorder::start(&session_dir) {
        Ok(session) => {
            *recorder = Some(session);
            tray::set_record_item_recording(record_item);
        }
        Err(err) => eprintln!("Failed to start recording: {err}"),
    }
}

/// ウィンドウを表示する。設定・Recordings の両ウィンドウで共用する。
///
/// 初回表示時のみジオメトリ（位置・サイズ）を明示する（`geometry_committed` で一度きりに保つ）。
/// なぜ初回にジオメトリを明示するかは `docs/rules/slint.md` を参照。
/// 表示済み・最小化中のウィンドウでも前面化・キー化まで行う（背景は `bring_to_front` 参照）。
fn show_window(
    window: &slint::Window,
    geometry_committed: &mut bool,
    position: slint::LogicalPosition,
    size: slint::LogicalSize,
) {
    if !*geometry_committed {
        window.set_position(position);
        window.set_size(size);
        *geometry_committed = true;
    }
    if let Err(err) = window.show() {
        // 表示に失敗したウィンドウを前面化すると「Slint 側は非表示のつもりなのに
        // NSWindow だけ前面に出る」不整合になりうるため、ここで打ち切る。
        eprintln!("Failed to show the window: {err}");
        return;
    }
    bring_to_front(window);
}

/// ウィンドウを前面に出してキーウィンドウにする（macOS）。
///
/// `slint::Window::show()` は**表示中のウィンドウには no-op** で、前面化もフォーカスもしない。
/// さらに本アプリは Dock に出ない Accessory 常駐アプリ（`hide_dock_icon` 参照）のため、
/// トレイメニューのクリックではアプリ自体がアクティブ化されず、表示中のウィンドウは他アプリの
/// 背後に残る（「メニューを押したのに反応しない」ように見える）。そこで raw-window-handle
/// 連携で対象の NSWindow を取得し、最小化からの復元・前面化・キー化とアプリのアクティブ化を
/// 行う。対象の NSWindow を直接キー化するため、設定・Recordings の両方が開いていても選んだ
/// メニューに対応する方がキーになる。
///
/// ハンドルが取得できない場合（非 AppKit バックエンド等）はログして何もしない
/// （`show()` のみの従来挙動に縮退）。
#[cfg(target_os = "macos")]
fn bring_to_front(window: &slint::Window) {
    use raw_window_handle::HasWindowHandle;
    // slint::Window::window_handle() は Slint のラッパー（slint::WindowHandle）を返し、
    // そこから raw-window-handle の trait で raw ハンドルを取り出す。
    let raw = window
        .window_handle()
        .window_handle()
        .map(|handle| handle.as_raw());
    // SAFETY: raw は直前に、生きている slint::Window（呼び出し元が参照を保持）から
    // 取得したハンドルで、AppKit バリアントなら表示中ウィンドウの有効な NSView を指す。
    // この呼び出し中は Slint がウィンドウを所有し続けるため、ポインタは生存している。
    unsafe { raise_ns_window(raw) };
}

/// macOS 以外では前面化は行わない（`show()` の挙動のまま）。
#[cfg(not(target_os = "macos"))]
fn bring_to_front(_window: &slint::Window) {}

/// `bring_to_front` の本体。raw ハンドルを引数で受けるのは、ウィンドウ実体なしで
/// 縮退経路（取得失敗・非 AppKit）を決定的にテストできるようにするため。
///
/// # Safety
///
/// `raw` が `AppKit` バリアントの場合、その `ns_view` はこの関数の呼び出し中に解放されない
/// 有効な NSView を指していること。`Err` や AppKit 以外のバリアントはポインタを参照せず
/// 縮退するため、この前提を自明に満たす。メインスレッドか否かは関数内部で確認し、
/// 外れる場合はポインタに触れず縮退する（呼び出し側の前提ではない）。
#[cfg(target_os = "macos")]
unsafe fn raise_ns_window(
    raw: Result<raw_window_handle::RawWindowHandle, raw_window_handle::HandleError>,
) {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSView};

    let handle = match raw {
        Ok(raw_window_handle::RawWindowHandle::AppKit(handle)) => handle,
        Ok(_) => {
            eprintln!("Skipping bring-to-front because the window is not backed by AppKit");
            return;
        }
        Err(err) => {
            eprintln!("Skipping bring-to-front because the window handle is unavailable: {err}");
            return;
        }
    };
    // 実運用ではイベントループのコールバック（メインスレッド）から呼ばれる。テスト等の
    // メインスレッド外では AppKit に触らず縮退する（expect でパニックさせない）。
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("Skipping bring-to-front because it is not called on the main thread");
        return;
    };
    // SAFETY: この関数の # Safety（ns_view は生存中の NSView を指す）を呼び出し側が保証する。
    // メインスレッド上であることは直前の mtm 確認で保証済みで、参照はこの関数内で完結し
    // ポインタを保持して持ち越さない。
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    let Some(ns_window) = view.window() else {
        eprintln!("Skipping bring-to-front because the view has no window");
        return;
    };
    // Cmd+M で最小化されている場合は復元してから前面化する。
    if ns_window.isMiniaturized() {
        ns_window.deminiaturize(None);
    }
    ns_window.makeKeyAndOrderFront(None);
    // Accessory アプリはウィンドウを前面化してもアプリ自体が非アクティブのままなので、
    // 明示的にアクティブ化する。他アプリからフォーカスを奪う操作だが、ユーザーのメニュー
    // クリックへの応答としてのみ呼ばれるため許容する（バックグラウンドから呼ぶ経路は作らない）。
    // 後継の activate() は macOS 14+ のため、最低対応 macOS 13 では非推奨版を使う
    // （expect にしておけば、非推奨でなくなったときに警告で気づける）。
    #[expect(deprecated)]
    NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);
}

/// 録音中アイコンの明滅レベルを、録音経過時間からサイン波で算出する純粋関数。
///
/// `0.0`（最も暗い赤）〜`1.0`（最も明るい赤）を返す。位相はティック数ではなく経過時間
/// （`Recorder::elapsed()`）基準なので、ポーリング tick の揺れに依存せず一定周期で明滅する。
/// `cycle_secs` は 1 サイクル（明→暗→明）の秒数。位相 0 は中間（0.5）から始まる。
fn breathing_level(elapsed: std::time::Duration, cycle_secs: f32) -> f32 {
    use std::f32::consts::PI;
    let t = elapsed.as_secs_f32();
    ((2.0 * PI * t / cycle_secs).sin() + 1.0) / 2.0
}

/// 文字起こしの表示状態（`ui/recordings-window.slint` の `TranscriptStatus`）を合成する。
/// ワーカーの進行状況（メモリ）があればそれを優先し、無ければ JSON の有無で解決する。
fn transcript_display_status(
    worker_status: Option<transcribe::TranscribeStatus>,
    has_transcript: bool,
) -> TranscriptStatus {
    match worker_status {
        Some(transcribe::TranscribeStatus::Transcribing) => TranscriptStatus::Transcribing,
        Some(transcribe::TranscribeStatus::Done) => TranscriptStatus::Done,
        Some(transcribe::TranscribeStatus::Failed) => TranscriptStatus::Failed,
        None if has_transcript => TranscriptStatus::Done,
        None => TranscriptStatus::NotTranscribed,
    }
}

/// 「文字起こし中」の表示ラベル。状態テキストと Transcript の縮退表示の両方で同じ文言を
/// 使うため、1 箇所で管理する（片方だけ変えて食い違うのを防ぐ）。
const TRANSCRIBING_LABEL: &str = "Transcribing…";

/// 文字起こしの表示状態 → 詳細ペインの状態テキスト。
fn transcript_status_text(display_status: TranscriptStatus) -> &'static str {
    match display_status {
        TranscriptStatus::NotTranscribed => "Not transcribed",
        TranscriptStatus::Transcribing => TRANSCRIBING_LABEL,
        TranscriptStatus::Done => "Transcribed",
        TranscriptStatus::Failed => "Transcription failed",
    }
}

/// 文字起こしの表示状態 → Transcript セクションの縮退表示（セグメントが無いとき）のラベル。
///
/// 状態テキスト（`transcript_status_text`）が**いまどうなっているか**を言うのに対し、こちらは
/// **何が無いか**を言う（`Not transcribed yet`）。どちらも sentence case（#128 で揃えた）。
/// `Done` でセグメントが空になるのは JSON の欠落・破損時で、従来どおり未実施と同じ表示に落とす。
fn transcript_placeholder_text(display_status: TranscriptStatus) -> &'static str {
    match display_status {
        TranscriptStatus::Transcribing => TRANSCRIBING_LABEL,
        TranscriptStatus::Failed => "Transcription failed",
        TranscriptStatus::NotTranscribed | TranscriptStatus::Done => "Not transcribed yet",
    }
}

/// 詳細ペインの文字起こし表示（状態テキスト・状態依存の配色・縮退ラベル）を反映する。
/// 選択時・手動投入直後・tick 追従の全経路でここを通し、表示ロジックを 1 箇所にする。
///
/// **ボタンの活性は Rust から set しない**。Slint 側が状態 enum から導出する 2 つのゲートで
/// 決める（bool を別途渡して enum と食い違う余地を作らないため。`docs/rules/slint.md`）:
/// `detail-files-in-use`（文字起こし中・要約生成中＝ワーカーがファイルを読み書き中）が Delete を、
/// `detail-jobs-pending`（それ＋要約のキュー待ち）が Transcribe / Summarize を止める。
fn apply_detail_transcript_status(rec: &RecordingsWindow, status: TranscriptStatus) {
    rec.set_detail_transcript_text(transcript_status_text(status).into());
    rec.set_detail_transcript_placeholder(transcript_placeholder_text(status).into());
    rec.set_detail_transcript_status(status);
}

/// セッションの現在の文字起こし状態を合成して詳細ペインへ反映する（選択時・手動投入直後用。
/// tick は行の差分更新で status を計算済みのため `apply_detail_transcript_status` を直接使う）。
fn refresh_detail_transcript_status(
    rec: &RecordingsWindow,
    transcriber: &transcribe::TranscribeWorker,
    session: &recordings::RecordingSession,
) {
    apply_detail_transcript_status(
        rec,
        transcript_display_status(transcriber.status_of(&session.dir), session.has_transcript),
    );
}

/// 議事録生成の表示状態（`ui/recordings-window.slint` の `SummaryStatus`）を合成する。
/// ワーカーの進行状況（メモリ）があればそれを優先し、無ければ `summary.md` の有無で解決する
/// （`transcript_display_status` と同じ流儀）。
fn summary_display_status(
    worker_status: Option<summarize::SummarizeStatus>,
    has_summary: bool,
) -> SummaryStatus {
    match worker_status {
        Some(summarize::SummarizeStatus::Queued) => SummaryStatus::Queued,
        Some(summarize::SummarizeStatus::Summarizing) => SummaryStatus::Summarizing,
        Some(summarize::SummarizeStatus::Done) => SummaryStatus::Done,
        Some(summarize::SummarizeStatus::Failed) => SummaryStatus::Failed,
        None if has_summary => SummaryStatus::Done,
        None => SummaryStatus::NotSummarized,
    }
}

/// 「要約生成中」の表示ラベル。状態テキストと Summary の縮退表示で同じ文言を使うため 1 箇所で
/// 管理する（`TRANSCRIBING_LABEL` と同じ理由）。
const SUMMARIZING_LABEL: &str = "Writing notes…";

/// 「キュー待ち」の表示ラベル。生成中と区別できる語にする: この間はまだ CPU を使っておらず、
/// 取り消せる（`SummarizeWorker::cancel`）。
///
/// 状態行と縮退表示で**同じ文言**を使う（#128 で語を `notes` に揃えたら一致した）。以前は
/// 大小の流儀が違って 2 つに分けていた。
const SUMMARY_QUEUED_LABEL: &str = "Waiting to write notes…";

/// 議事録生成の表示状態 → 詳細ペインの状態テキスト。
fn summary_status_text(display_status: SummaryStatus) -> &'static str {
    match display_status {
        SummaryStatus::NotSummarized => "No notes",
        SummaryStatus::Queued => SUMMARY_QUEUED_LABEL,
        SummaryStatus::Summarizing => SUMMARIZING_LABEL,
        SummaryStatus::Done => "Notes ready",
        SummaryStatus::Failed => "Notes failed",
    }
}

/// 議事録生成の表示状態 → Notes タブの縮退表示（行が無いとき）のラベル。状態テキストが
/// **いまどうなっているか**を言うのに対し、こちらは**何が無いか**を言う
/// （`transcript_placeholder_text` と対称）。`Done` で行が空になるのは `summary.md` の欠落・
/// 破損・空のときで、未生成と同じ表示に落とす。
fn summary_placeholder_text(display_status: SummaryStatus) -> &'static str {
    match display_status {
        SummaryStatus::Queued => SUMMARY_QUEUED_LABEL,
        SummaryStatus::Summarizing => SUMMARIZING_LABEL,
        SummaryStatus::Failed => "Notes could not be written",
        SummaryStatus::NotSummarized | SummaryStatus::Done => "No notes yet",
    }
}

/// `summary.md` を Summary タブの表示行へ分ける（**Markdown をどこまで解釈するかの正はここ**。
/// `ui/recordings-window.slint` の `SummaryRow` はこの doc を参照する）。
///
/// 本格的なレンダリングはしない（#81 のスコープ外）。行単位に切って、**見出し（`#` の連なりの
/// 後ろに空白か行末が続く行）だけ**記号を落として `is_heading` を立てる。`##` 以降も同じ強調で、
/// 階層は付けない（この幅のペインで 3 段の見出しを描き分けても読み取れない）。ほかの記法
/// （`- ` の箇条書き等）は記号ごとそのまま出す（消すと構造が読めなくなる）。
///
/// 空行は段落の切れ目として残すが、末尾の空行は落とす（生成物は末尾に改行を持つ）。**見出し記号
/// だけの行（`#` 単独）は行の途中でも落とす**（強調だけの空行を描かない）。中身が空になる行だけの
/// 入力は**行なし**になり、呼び出し側で状態依存の縮退表示へ落ちる。
fn summary_rows(text: &str) -> Vec<SummaryRow> {
    let mut rows: Vec<SummaryRow> = text
        .lines()
        .map(|line| {
            let trimmed = line.trim_end();
            match heading_text(trimmed.trim_start()) {
                Some(heading) => SummaryRow {
                    text: heading.into(),
                    is_heading: true,
                },
                None => SummaryRow {
                    text: trimmed.into(),
                    is_heading: false,
                },
            }
        })
        .collect();
    // 見出し記号だけの行（`#` 単独）は本文が空なので落とす（強調だけの空行を描かない。
    // 行の途中でも末尾でも同じ扱い）。本文側の空行は段落の切れ目として残す。
    rows.retain(|row| !(row.is_heading && row.text.is_empty()));
    // 末尾の空行は落とす（生成物は末尾に改行を持つ）。中身が全部空なら行なしになり、
    // 呼び出し側で状態依存の縮退表示へ落ちる。
    while rows.last().is_some_and(|row| row.text.is_empty()) {
        rows.pop();
    }
    rows
}

/// 行が Markdown の見出しなら、記号と後続の空白を落とした本文を返す。
///
/// `#` の連なりの直後が**空白か行末**であることを条件にする（`#81 の件` のような行頭ハッシュを
/// 見出しと誤認して `81 の件` と表示しないため）。
fn heading_text(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('#')?.trim_start_matches('#');
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(rest.trim_start())
    } else {
        None
    }
}

/// 詳細ペインの議事録生成の表示（状態テキスト・状態依存の配色・縮退ラベル）を反映する
/// （`apply_detail_transcript_status` と対称。ボタンの活性の扱いもそちらの doc 参照）。
fn apply_detail_summary_status(rec: &RecordingsWindow, status: SummaryStatus) {
    rec.set_detail_summary_status_text(summary_status_text(status).into());
    rec.set_detail_summary_placeholder(summary_placeholder_text(status).into());
    rec.set_detail_summary_status(status);
}

/// セッションの現在の要約状態を合成して詳細ペインへ反映する（選択時・手動投入直後用。
/// tick は状態を計算済みなので `apply_detail_summary_status` を直接使う）。
fn refresh_detail_summary_status(
    rec: &RecordingsWindow,
    summarizer: &summarize::SummarizeWorker,
    session: &recordings::RecordingSession,
) {
    apply_detail_summary_status(
        rec,
        summary_display_status(summarizer.status_of(&session.dir), session.has_summary),
    );
}

/// 選択中セッションの `summary.md` を読み直して Summary タブへ反映する（選択時・生成完了時）。
/// 欠落・破損・空はいずれも行なしになり、Slint 側が状態依存のラベルへ縮退させる。
fn refresh_detail_summary_rows(rec: &RecordingsWindow, session_dir: &std::path::Path) {
    let rows = summarize::load_summary(session_dir)
        .map(|text| summary_rows(&text))
        .unwrap_or_default();
    rec.set_summary_rows(Rc::new(slint::VecModel::from(rows)).into());
}

/// 保存先パスを画面表示用の文字列に変換する。
fn recording_dir_text(dir: &std::path::Path) -> slint::SharedString {
    dir.display().to_string().into()
}

/// 設定画面に出すバージョン表記（例: `shoki v0.1.0`）。
///
/// バージョンの正は `Cargo.toml` の `version` 一本で、ここは `env!("CARGO_PKG_VERSION")`
/// （コンパイル時定数）から組み立てる。実行時にファイルを読まないので、表示と動いている
/// バイナリがずれることがない。
fn app_version_text() -> slint::SharedString {
    format!("shoki v{}", env!("CARGO_PKG_VERSION")).into()
}

/// 登録アプリ 1 件を設定画面の行にする。検知できないアプリには
/// `app_audio_monitor::auto_record_limitation` の注記を添える。
fn trigger_app_row(trigger: &config::AppTrigger) -> TriggerApp {
    #[cfg(target_os = "macos")]
    let note = app_audio_monitor::auto_record_limitation(&trigger.bundle_id).unwrap_or("");
    // 自動録音は macOS 限定の機能なので、他 OS では注記も出さない。
    #[cfg(not(target_os = "macos"))]
    let note = "";

    TriggerApp {
        name: trigger.name.as_str().into(),
        limitation_note: note.into(),
    }
}

/// その種別のモデルファイルを `config.toml` で上書きしているか（上書き先のパス）。上級者向けの
/// 手編集のみで、UI からは設定できない。
///
/// **網羅 match** にしてあるので、種別を足したら上書きの扱いを書くまでコンパイルが通らない。
/// 取得の契機（`model_downloads_on_select`）と状態行（`*_model_status_line`）の**両方**がここを
/// 通るので、種別を足した人が片方だけ更新して静かに食い違うことがない。
fn model_path_override(
    kind: model_download::ModelKind,
    config: &Config,
) -> Option<&std::path::Path> {
    match kind {
        model_download::ModelKind::Speech => config.whisper_model_path.as_deref(),
        model_download::ModelKind::Summary => config.summary_model_path.as_deref(),
    }
}

/// モデルを選び直した時点で、そのモデルの取得を始めるか。**取得の契機の正**で、状態行の文言
/// （`whisper_model_status_line` / `summary_model_status_line`）もこれに合わせる。
///
/// 使われないモデルを数 GB 落とさないための抑止（`docs/rules/security.md` の「通信はユーザーが
/// 機能を有効化したときだけ」）。抑止するケースは、その後の取得の仕方も違う:
///
/// - **モデルパスを上書き中**（両種別）: そのファイルが優先されるので、カタログのモデルは以後も
///   取得しない（`TranscribeJob::model_override` / `summarize::resolve_model`）。
/// - **要約 OFF**（要約のみ）: 選択だけ保存する。取得は次に要約が走るとき（設定を ON にした後の
///   初回要約、または Recordings ウィンドウの「Summarize」による手動生成）に `ensure_model` が行う。
///
/// 文字起こし側に「自動文字起こし OFF なら取得しない」というゲートは**置かない**。一覧の「Use」を
/// 押すのは**先行取得の意思表示**だから（機能を OFF にしたまま準備しておく、という使い方をする）。
/// 要約側に `auto_summarize` のゲートがあるのは、要約 LLM が whisper より大きく（最大 4.4 GB）、
/// 生成時に `ensure_model` が取得する経路が別にあるから。
///
/// 上書きの判定は `model_path_override` に任せ、ここは**上書き以外の契機**だけを種別ごとに
/// 書く（**網羅 match** なので、種別を足したら契機を書くまでコンパイルが通らない）。
///
/// テストでピン留めしてあるのはこの述語まで。呼び出し側のガード（`select_model` の
/// `if downloads_now`）は、実際に取得を始める副作用を持つためテストから叩けない。
fn model_downloads_on_select(kind: model_download::ModelKind, config: &Config) -> bool {
    if model_path_override(kind, config).is_some() {
        return false;
    }
    match kind {
        model_download::ModelKind::Speech => true,
        model_download::ModelKind::Summary => config.auto_summarize,
    }
}

/// モデルパスを `config.toml` で上書きしているときの、設定画面の状態行。whisper・要約 LLM で
/// 共用する（同じ状態なので、片方だけ書き換えて種別で違う説明が出るのを防ぐ）。
const MODEL_OVERRIDDEN_STATUS: &str = "Using the model file set in config.toml";

/// 設定画面の状態行を、**変わったときだけ**流し込むためのキャッシュ（種別ごとに直前の行を持つ）。
///
/// 文字起こしに使う whisper モデルの取得状況を、設定画面の状態行（文言・意味・進捗）にする。
///
/// どのモデルかは Select が示すので、ここは状態だけを出す。ただし上書き中は選んでも取得せず
/// そのファイルが使われるので、共用の「downloads automatically」だと表示と挙動が食い違う。
/// 取得の契機の正は `model_downloads_on_select`。
fn whisper_model_status_line(
    config: &Config,
    downloader: &model_download::ModelDownloader,
) -> ModelStatusLine {
    let kind = model_download::ModelKind::Speech;
    if model_path_override(kind, config).is_some() {
        // 壊れてはいないが、選択が使われない状態なので caution（失敗ではない）。
        return ModelStatusLine {
            overridden: true,
            ..ModelStatusLine::plain(
                kind,
                MODEL_OVERRIDDEN_STATUS.to_owned(),
                StatusTone::Caution,
            )
        };
    }
    model_status_line(
        kind,
        whisper_model::spec_or_default(&config.whisper_model),
        downloader,
    )
}

/// 議事録生成に使う LLM の取得状況を、設定画面の状態行（文言・意味・進捗）にする。
///
/// どのモデルかは Select が示すので、ここは状態だけを出す。ただし取得の契機は whisper より
/// 条件が多い（`model_downloads_on_select`）ので、共用の「downloads automatically」では表示と
/// 挙動が食い違う場合がある。その場合は契機を明示する:
///
/// - モデルパスを上書きしている: そのファイルが使われ、カタログのモデルは取得しない
///   （whisper と同じ状態なので `MODEL_OVERRIDDEN_STATUS` を共用する）。
/// - 要約 OFF: 選んでも取得は始まらない（次に要約が走るときに取得する。設定を ON にした後の
///   初回要約か、Recordings ウィンドウからの手動生成）。
fn summary_model_status_line(
    config: &Config,
    downloader: &model_download::ModelDownloader,
) -> ModelStatusLine {
    let kind = model_download::ModelKind::Summary;
    if model_path_override(kind, config).is_some() {
        return ModelStatusLine {
            overridden: true,
            ..ModelStatusLine::plain(
                kind,
                MODEL_OVERRIDDEN_STATUS.to_owned(),
                StatusTone::Caution,
            )
        };
    }
    let spec = summary_model::spec_or_default(&config.summary_model);
    if !model_downloads_on_select(kind, config)
        && downloader.status_of(spec) == model_download::DownloadStatus::NotDownloaded
    {
        return ModelStatusLine::plain(
            kind,
            format!(
                "Not downloaded — downloads when notes are generated ({})",
                model_download::format_size(spec.size_bytes)
            ),
            StatusTone::Neutral,
        );
    }
    model_status_line(kind, spec, downloader)
}

/// 受信済みバイトから進捗のパーセントを出す（設定画面の状態行と一覧の行で共用）。
///
/// `total` は Content-Length または既知サイズで常に正だが、防御的にゼロ除算を避ける。
/// Content-Length が実サイズより小さい異常時も 100% を超えて表示しない。
fn download_percent(received: u64, total: u64) -> u64 {
    (received.saturating_mul(100) / total.max(1)).min(100)
}

/// 設定画面の状態行 1 本ぶん。文言だけでなく**意味（`tone`）と進捗**も一緒に運ぶ。
///
/// 色の対応表は Slint 側（`Style.tone-ink` / `Style.tone-mark`）に 1 つだけ置き、こちらは
/// 「どの意味か」を決める（`docs/rules/slint.md` の「状態→UI の対応表を三項連鎖にしない」）。
///
/// `kind` を持つのは、**流し込む先を呼び出し側の規律に任せない**ため（`apply` の網羅 match が
/// 決める。種別を足したら流し込み先を書くまでコンパイルが通らない）。
#[derive(Debug, Clone, PartialEq)]
struct ModelStatusLine {
    kind: model_download::ModelKind,
    text: String,
    tone: StatusTone,
    /// 取得中の進捗（0.0〜1.0）。取得中でなければ `None`。Slint の `float` へは
    /// `apply` が `-1.0` へ落とす（**センチネルを知るのはその 1 箇所だけ**にする）。
    progress: Option<f32>,
    /// `config.toml` でモデルファイルのパスを上書きしているか。カタログの選択が使われないので、
    /// 設定画面は選択肢の説明行を出さない。
    overridden: bool,
}

impl ModelStatusLine {
    /// 進捗を持たない状態行。
    fn plain(kind: model_download::ModelKind, text: String, tone: StatusTone) -> Self {
        Self {
            kind,
            text,
            tone,
            progress: None,
            overridden: false,
        }
    }
}

/// モデルの取得状況を、設定画面の状態行にする（whisper / 要約 LLM で共用）。
///
/// **網羅 match** なので、`DownloadStatus` にバリアントを足したら文言と意味を決めるまで
/// コンパイルが通らない。
fn model_status_line(
    kind: model_download::ModelKind,
    spec: &'static model_download::ModelSpec,
    downloader: &model_download::ModelDownloader,
) -> ModelStatusLine {
    match downloader.status_of(spec) {
        model_download::DownloadStatus::NotDownloaded => ModelStatusLine::plain(
            kind,
            format!(
                // 自動取得の契機は複数ある（設定画面で選択した時点、または次の文字起こし・要約時）。
                // 共用の文言なので、どれかに限定した書き方にしない。
                "Not downloaded — downloads automatically ({})",
                model_download::format_size(spec.size_bytes)
            ),
            StatusTone::Neutral,
        ),
        model_download::DownloadStatus::Downloading { received, total } => ModelStatusLine {
            kind,
            text: format!("Downloading… {}%", download_percent(received, total)),
            tone: StatusTone::Active,
            // 0.0〜1.0 に丸めるのは**ここ**（doc の契約を生成側で守る）。分母は Content-Length か
            // 既知サイズで常に正だが防御的にゼロ除算を避け、Content-Length が実サイズより小さい
            // 異常時も 1.0 を超えない（文言側の `download_percent` が 100% で頭打ちにするのと同じ）。
            progress: Some((received as f32 / total.max(1) as f32).clamp(0.0, 1.0)),
            overridden: false,
        },
        model_download::DownloadStatus::Downloaded => {
            ModelStatusLine::plain(kind, "Downloaded".to_owned(), StatusTone::Done)
        }
        model_download::DownloadStatus::Failed(reason) => ModelStatusLine::plain(
            kind,
            format!("Download failed: {reason}"),
            StatusTone::Danger,
        ),
    }
}

/// macOS で Dock アイコンを隠し、メニューバー常駐アプリとして振る舞わせる。
///
/// activation policy を Accessory にすることで Dock とアプリスイッチャーに出なくなる。
/// **イベントループ開始後に呼ぶこと**。winit は未バンドル起動時に起動処理
/// （`applicationDidFinishLaunching:`）で policy を Regular へ強制するため、ループ開始前に
/// 設定しても上書きされる。呼び出しは `main` の `invoke_from_event_loop` に集約している。
/// 配布パッケージでは `Info.plist` の `LSUIElement` 指定が確実だが、それはパッケージング時に扱う。
#[cfg(target_os = "macos")]
fn hide_dock_icon() {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};

    let mtm = MainThreadMarker::new()
        .expect("the Slint event loop runs on the main thread, so this succeeds");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

/// Slint のテストバックエンドを初期化する（**スレッドごとに 1 回だけ**呼べる仕様）。
///
/// `tests/ui_support::init_backend` と同じ趣旨だが、あちらは統合テスト用のモジュールで bin
/// クレートからは使えないため、ここに持つ。
#[cfg(test)]
pub(crate) fn init_test_backend() {
    use std::cell::Cell;
    thread_local! {
        static INITIALIZED: Cell<bool> = const { Cell::new(false) };
    }
    INITIALIZED.with(|initialized| {
        if !initialized.replace(true) {
            i_slint_backend_testing::init_no_event_loop();
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        StatusTone, SummaryStatus, TranscriptStatus, app_version_text, breathing_level,
        model_downloads_on_select, model_status_line, playback_progress, seek_position_from_ratio,
        summary_display_status, summary_model_status_line, summary_placeholder_text, summary_rows,
        summary_status_text, transcript_display_status, transcript_placeholder_text,
        transcript_status_text, whisper_model_status_line,
    };
    use chrono::{Datelike as _, Timelike as _};

    use crate::transcribe::TranscribeStatus;
    use std::time::Duration;

    /// 一覧の見出しは**その日の最初の行だけ**が持つ。
    ///
    /// 全行に出すと同じ語が並んで、どこで日が変わったのか分からない。逆に別の配列へ分けると、
    /// 行の添字とセッションの添字がずれて**別の録音を選ぶ**（`SessionRow` の doc）。
    #[test]
    fn a_group_heading_marks_only_the_first_row_of_its_day() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
            .expect("a valid date")
            .and_hms_opt(18, 0, 0)
            .expect("a valid time");
        let sessions = [
            crate::recordings::RecordingSession::for_test(now.with_hour(14).expect("a valid hour")),
            crate::recordings::RecordingSession::for_test(now.with_hour(9).expect("a valid hour")),
            crate::recordings::RecordingSession::for_test(
                now.with_day(9)
                    .expect("a valid day")
                    .with_hour(16)
                    .expect("a valid hour"),
            ),
        ];

        assert_eq!(super::session_group_heading(&sessions, 0, now), "Today");
        assert_eq!(
            super::session_group_heading(&sessions, 1, now),
            "",
            "the second recording of the same day repeats no heading"
        );
        assert_eq!(super::session_group_heading(&sessions, 2, now), "Yesterday");
    }

    /// 行の 3 行目は「音源 · 文字起こしの状態」（**網羅 match**。状態を足したら語を決めるまで
    /// コンパイルが通らない）。
    #[test]
    fn a_row_says_its_sources_and_transcript_state() {
        let now = chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
            .expect("a valid date")
            .and_hms_opt(14, 0, 0)
            .expect("a valid time");
        let mut session = crate::recordings::RecordingSession::for_test(now);
        session.has_mic = true;
        session.has_system = true;
        assert_eq!(
            super::session_detail_text(&session, TranscriptStatus::Transcribing),
            "Mic + system · transcribing"
        );
        session.has_system = false;
        assert_eq!(
            super::session_detail_text(&session, TranscriptStatus::Done),
            "Mic only · transcribed"
        );
        session.has_mic = false;
        assert_eq!(
            super::session_detail_text(&session, TranscriptStatus::Failed),
            "No audio · transcription failed",
            "a session without audio still says what it is"
        );
    }

    /// 一覧の合計は**件数だけ**（単数形も出す）。容量は全ファイルを開かないと分からない。
    #[test]
    fn the_library_summary_counts_recordings() {
        assert_eq!(super::library_summary(0), "0 recordings");
        assert_eq!(super::library_summary(1), "1 recording");
        assert_eq!(super::library_summary(148), "148 recordings");
    }

    /// バージョン表記が `Cargo.toml` の `version` と一致することを確かめる。
    ///
    /// 期待値を `env!("CARGO_PKG_VERSION")` で組むと実装と同じ式になるので、**別の出所**として
    /// `Cargo.toml` を直接読む。これで「バンプしたのに表示が追随しない」形の崩れは捕まる。
    ///
    /// ただし「実装が `env!` を使っている」ことまでは検証できない。実装を現在の値のまま
    /// ハードコードしても両辺が一致して通る（ミューテーションで確認済み）。`Cargo.toml` と
    /// `env!("CARGO_PKG_VERSION")` の紐づきは cargo のコンパイル時保証で、実行時テストの
    /// 守備範囲外。

    #[test]
    fn app_version_text_shows_the_version_from_cargo_toml() {
        let version = include_str!("../Cargo.toml")
            .lines()
            .find_map(|line| line.strip_prefix("version = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .expect("Cargo.toml should declare a package version");

        assert_eq!(app_version_text(), format!("shoki v{version}").as_str());
    }

    /// 設定画面の行は、検知できないアプリにだけ注記を持つ（判定は `auto_record_limitation`）。
    /// バンドル ID → 注記の写像を渡し忘れる回帰を、ここで止める。
    ///
    /// 自動録音は macOS 限定なので、他 OS では注記が常に空になる（`trigger_app_row` の `cfg`）。
    #[test]
    #[cfg(target_os = "macos")]
    fn trigger_app_row_notes_undetectable_apps() {
        // 非 macOS では item ごと落ちるので、import も関数の中に置く（外に出すと未使用になる）。
        use super::trigger_app_row;
        use crate::config::AppTrigger;

        let row = trigger_app_row(&AppTrigger {
            bundle_id: "com.apple.Safari".to_owned(),
            name: "Safari".to_owned(),
        });
        assert_eq!(row.name, "Safari");
        // 文言そのものは `auto_record_limitation` が持つので、ここでは有無だけを見る。
        assert!(
            !row.limitation_note.is_empty(),
            "Safari should carry a note about not being detected"
        );

        let row = trigger_app_row(&AppTrigger {
            bundle_id: "com.google.Chrome".to_owned(),
            name: "Google Chrome".to_owned(),
        });
        assert_eq!(row.name, "Google Chrome");
        assert!(row.limitation_note.is_empty());
    }

    /// ワーカーの進行状況（メモリ）があればそれを優先し、無ければ JSON の有無で解決する。
    #[test]
    fn transcript_display_status_prefers_worker_status_over_json() {
        // ワーカーの状態が最優先（再実行中は完了済み JSON があっても「文字起こし中」）。
        assert_eq!(
            transcript_display_status(Some(TranscribeStatus::Transcribing), true),
            TranscriptStatus::Transcribing
        );
        assert_eq!(
            transcript_display_status(Some(TranscribeStatus::Done), false),
            TranscriptStatus::Done
        );
        // 失敗の記録は JSON があっても優先する（古い JSON で失敗を隠さない）。
        assert_eq!(
            transcript_display_status(Some(TranscribeStatus::Failed), true),
            TranscriptStatus::Failed
        );
        // ワーカーの記録が無ければ JSON の有無で解決する（起動前の録音など）。
        assert_eq!(
            transcript_display_status(None, true),
            TranscriptStatus::Done
        );
        assert_eq!(
            transcript_display_status(None, false),
            TranscriptStatus::NotTranscribed
        );
    }

    #[test]
    fn transcript_status_text_covers_all_states() {
        assert_eq!(
            transcript_status_text(TranscriptStatus::NotTranscribed),
            "Not transcribed"
        );
        assert_eq!(
            transcript_status_text(TranscriptStatus::Transcribing),
            "Transcribing…"
        );
        assert_eq!(
            transcript_status_text(TranscriptStatus::Done),
            "Transcribed"
        );
        assert_eq!(
            transcript_status_text(TranscriptStatus::Failed),
            "Transcription failed"
        );
    }

    /// 縮退表示ラベル。Done は「セグメントが空＝JSON の欠落・破損」の経路でのみ表示され、
    /// 未実施と同じラベルに落とす。
    #[test]
    fn transcript_placeholder_text_covers_all_states() {
        assert_eq!(
            transcript_placeholder_text(TranscriptStatus::NotTranscribed),
            "Not transcribed yet"
        );
        assert_eq!(
            transcript_placeholder_text(TranscriptStatus::Transcribing),
            "Transcribing…"
        );
        assert_eq!(
            transcript_placeholder_text(TranscriptStatus::Done),
            "Not transcribed yet"
        );
        assert_eq!(
            transcript_placeholder_text(TranscriptStatus::Failed),
            "Transcription failed"
        );
    }

    /// 要約もワーカーの進行状況を優先し、無ければ `summary.md` の有無で解決する
    /// （文字起こし側と同じ契約）。
    #[test]
    fn summary_display_status_prefers_worker_status_over_the_file() {
        use crate::summarize::SummarizeStatus;

        // 投入直後（キュー待ち）は生成中と区別する。取り消せるのはこの間だけ。
        assert_eq!(
            summary_display_status(Some(SummarizeStatus::Queued), false),
            SummaryStatus::Queued
        );
        // 再生成中は `summary.md` が残っていても「生成中」。
        assert_eq!(
            summary_display_status(Some(SummarizeStatus::Summarizing), true),
            SummaryStatus::Summarizing
        );
        assert_eq!(
            summary_display_status(Some(SummarizeStatus::Done), false),
            SummaryStatus::Done
        );
        // 失敗の記録は古い `summary.md` があっても優先する（失敗を隠さない）。
        assert_eq!(
            summary_display_status(Some(SummarizeStatus::Failed), true),
            SummaryStatus::Failed
        );
        // ワーカーの記録が無ければファイルの有無で解決する（起動前に生成した分など）。
        assert_eq!(summary_display_status(None, true), SummaryStatus::Done);
        assert_eq!(
            summary_display_status(None, false),
            SummaryStatus::NotSummarized
        );
    }

    #[test]
    fn summary_status_text_covers_all_states() {
        assert_eq!(
            summary_status_text(SummaryStatus::NotSummarized),
            "No notes"
        );
        assert_eq!(
            summary_status_text(SummaryStatus::Queued),
            "Waiting to write notes…"
        );
        assert_eq!(
            summary_status_text(SummaryStatus::Summarizing),
            "Writing notes…"
        );
        assert_eq!(summary_status_text(SummaryStatus::Done), "Notes ready");
        assert_eq!(summary_status_text(SummaryStatus::Failed), "Notes failed");
    }

    /// 縮退表示ラベル。Done で行が空になるのは `summary.md` の欠落・破損・空の経路で、
    /// 未生成と同じラベルに落とす。
    #[test]
    fn summary_placeholder_text_covers_all_states() {
        assert_eq!(
            summary_placeholder_text(SummaryStatus::NotSummarized),
            "No notes yet"
        );
        assert_eq!(
            summary_placeholder_text(SummaryStatus::Queued),
            "Waiting to write notes…"
        );
        assert_eq!(
            summary_placeholder_text(SummaryStatus::Summarizing),
            "Writing notes…"
        );
        assert_eq!(
            summary_placeholder_text(SummaryStatus::Done),
            "No notes yet"
        );
        assert_eq!(
            summary_placeholder_text(SummaryStatus::Failed),
            "Notes could not be written"
        );
    }

    /// `summary.md` の行分け: 見出しは記号を落として heading を立て、他はそのまま。
    /// 途中の空行は段落の切れ目として残し、末尾の空行だけ落とす。
    ///
    /// 実際の議事録は日本語にもなるが、この関数は行頭の `#` だけを見て言語に依存しないので、
    /// 期待値の読みやすさを優先して英語で書く（日本語の見え方は確認用バイナリで見る）。
    #[test]
    fn summary_rows_marks_headings_and_keeps_body_lines() {
        let rows =
            summary_rows("# Overview\n\n- Decision: ship next week\n## Follow-ups\nbody\n\n\n");

        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(
            texts,
            vec![
                "Overview",
                "",
                "- Decision: ship next week",
                "Follow-ups",
                "body",
            ],
            "trailing blank lines are dropped, inner ones are kept"
        );
        let headings: Vec<bool> = rows.iter().map(|row| row.is_heading).collect();
        assert_eq!(headings, vec![true, false, false, true, false]);
    }

    /// 見出しは「`#` の連なり＋空白（または行末）」だけ。本文中の `#`・行頭の `#123` は
    /// 見出しにしない（記号を落として意味を変えてしまわないため）。
    #[test]
    fn summary_rows_only_treats_real_headings_as_headings() {
        let rows = summary_rows("issue #81 follow-up\n#81 follow-up\n#\tTabbed\n");
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        let headings: Vec<bool> = rows.iter().map(|row| row.is_heading).collect();
        assert_eq!(
            texts,
            vec!["issue #81 follow-up", "#81 follow-up", "Tabbed"]
        );
        assert_eq!(headings, vec![false, false, true]);

        // 行末の空白は落とす（LLM の出力に混じる）。
        let rows = summary_rows("body   \n");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].text, "body");

        // 記号だけの見出し行は本文が空なので、**行の途中でも**残さない（強調だけの空行を
        // 描かない）。前後の空行は段落の切れ目として残る。
        let rows = summary_rows("# A\nbody\n\n#\n\nmore\n");
        let texts: Vec<&str> = rows.iter().map(|row| row.text.as_str()).collect();
        assert_eq!(texts, vec!["A", "body", "", "", "more"]);

        // 中身が無い入力は行なし（呼び出し側が状態依存の縮退表示へ落とす）。
        assert!(summary_rows("").is_empty());
        assert!(summary_rows("\n\n").is_empty());
        assert!(summary_rows("###\n").is_empty());
    }

    /// シークバーの比率→再生位置。全体長に対する按分で、両端は境界そのものになる。
    #[test]
    fn seek_position_from_ratio_scales_within_the_duration() {
        let total = Duration::from_secs(120);
        assert_eq!(
            seek_position_from_ratio(0.0, Some(total)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            seek_position_from_ratio(0.25, Some(total)),
            Some(Duration::from_secs(30))
        );
        assert_eq!(seek_position_from_ratio(1.0, Some(total)), Some(total));
        // 全体長が不明ならシークしない（バーは表示専用に縮退する）。
        assert_eq!(seek_position_from_ratio(0.5, None), None);
        // 長さ 0 のファイルでも 0 秒に落ちるだけでパニックしない。
        assert_eq!(
            seek_position_from_ratio(0.5, Some(Duration::ZERO)),
            Some(Duration::ZERO)
        );
    }

    /// 不正な比率（範囲外・NaN・無限・負値）でもパニックせず 0.0〜1.0 相当へ丸める。
    #[test]
    fn seek_position_from_ratio_clamps_invalid_ratios() {
        let total = Duration::from_secs(60);
        assert_eq!(
            seek_position_from_ratio(-0.5, Some(total)),
            Some(Duration::ZERO)
        );
        assert_eq!(seek_position_from_ratio(1.5, Some(total)), Some(total));
        assert_eq!(
            seek_position_from_ratio(f32::NEG_INFINITY, Some(total)),
            Some(Duration::ZERO)
        );
        assert_eq!(
            seek_position_from_ratio(f32::INFINITY, Some(total)),
            Some(total)
        );
        assert_eq!(
            seek_position_from_ratio(f32::NAN, Some(total)),
            Some(Duration::ZERO)
        );
    }

    /// 再生位置→進捗比率。全体長が不明・0 のときは 0.0、行き過ぎても 1.0 を超えない。
    #[test]
    fn playback_progress_maps_position_to_ratio() {
        let total = Duration::from_secs(200);
        assert_eq!(playback_progress(Duration::ZERO, Some(total)), 0.0);
        assert_eq!(
            playback_progress(Duration::from_secs(50), Some(total)),
            0.25
        );
        assert_eq!(playback_progress(total, Some(total)), 1.0);
        // 全体長を超える位置（デコーダの報告位置が終端を跨ぐ等）でも 1.0 で止まる。
        assert_eq!(
            playback_progress(Duration::from_secs(500), Some(total)),
            1.0
        );
        assert_eq!(playback_progress(Duration::from_secs(10), None), 0.0);
        assert_eq!(
            playback_progress(Duration::from_secs(10), Some(Duration::ZERO)),
            0.0
        );
    }

    /// 取得状況テキストが 4 状態すべてを言い分けること（進捗の丸め・上限も含む）。
    #[test]
    fn model_status_text_covers_all_states() {
        let downloader = crate::model_download::ModelDownloader::new();
        let spec = crate::whisper_model::spec_for("large-v3").expect("large-v3 is in the catalog");

        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Downloading {
                received: 25,
                total: 100,
            },
        );
        assert_eq!(
            model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader).text,
            "Downloading… 25%"
        );

        // Content-Length が実サイズより小さい異常時も 100% を超えない。
        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Downloading {
                received: 300,
                total: 100,
            },
        );
        assert_eq!(
            model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader).text,
            "Downloading… 100%"
        );

        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::Downloaded);
        assert_eq!(
            model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader).text,
            "Downloaded"
        );

        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Failed("boom".into()),
        );
        assert_eq!(
            model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader).text,
            "Download failed: boom"
        );

        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::NotDownloaded);
        assert_eq!(
            model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader).text,
            "Not downloaded — downloads automatically (2.9 GB)"
        );
    }

    /// 要約 LLM を選んで取得を始めるのは「要約 ON かつモデルパス未上書き」のときだけ
    /// （4 通りを固定する）。
    #[test]
    fn model_downloads_on_select_for_summary_only_when_the_summary_runs() {
        let base = crate::config::Config::default();
        let with = |auto_summarize, path: Option<&str>| crate::config::Config {
            auto_summarize,
            summary_model_path: path.map(std::path::PathBuf::from),
            ..base.clone()
        };
        let downloads = |config: &crate::config::Config| {
            model_downloads_on_select(crate::model_download::ModelKind::Summary, config)
        };

        assert!(downloads(&with(true, None)));
        // 要約 OFF では使われないモデルを落とさない（既定は OFF なので既定でも落とさない）。
        assert!(!downloads(&with(false, None)));
        assert!(!downloads(&base));
        // 上書きしたファイルが優先されるので、カタログのモデルは落としても使われない。
        assert!(!downloads(&with(true, Some("/tmp/model.gguf"))));
        assert!(!downloads(&with(false, Some("/tmp/model.gguf"))));
    }

    /// 状態行の**意味（tone）と進捗**が状態ごとに決まること。色は Slint 側の対応表が引くので、
    /// ここが崩れると「失敗が普通の色で出る」「進捗バーが出ない／出っぱなし」になる。
    #[test]
    fn model_status_line_carries_tone_and_progress() {
        let downloader = crate::model_download::ModelDownloader::new();
        let spec = crate::whisper_model::default_spec();

        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::NotDownloaded);
        let line = model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader);
        assert_eq!(line.tone, StatusTone::Neutral);
        assert_eq!(line.progress, None, "no bar before the download starts");

        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Downloading {
                received: 25,
                total: 100,
            },
        );
        let line = model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader);
        assert_eq!(line.tone, StatusTone::Active);
        assert_eq!(line.progress, Some(0.25));

        // Content-Length が実サイズより小さい異常時も 1.0 を超えない（文言側の 100% 頭打ちと対）。
        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Downloading {
                received: 300,
                total: 100,
            },
        );
        assert_eq!(
            model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader).progress,
            Some(1.0)
        );
        // 分母 0 でもゼロ除算せず、範囲内に収まる。
        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Downloading {
                received: 0,
                total: 0,
            },
        );
        assert_eq!(
            model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader).progress,
            Some(0.0)
        );

        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::Downloaded);
        let line = model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader);
        assert_eq!(line.tone, StatusTone::Done);
        assert_eq!(line.progress, None);

        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Failed("boom".into()),
        );
        let failed = model_status_line(crate::model_download::ModelKind::Speech, spec, &downloader);
        assert_eq!(failed.tone, StatusTone::Danger);
        assert_eq!(failed.progress, None, "no bar once it failed");

        // 上書き中は「失敗」ではなく「そのままでは選択が使われない」＝ caution。
        let overridden = crate::config::Config {
            whisper_model_path: Some(std::path::PathBuf::from("/tmp/ggml-small.bin")),
            ..crate::config::Config::default()
        };
        assert_eq!(
            whisper_model_status_line(&overridden, &downloader).tone,
            StatusTone::Caution
        );
    }

    /// whisper モデルを選んで取得を始めるのは「モデルパス未上書き」のときだけ。上書き中は
    /// `transcribe` がそのファイルを使うので、カタログのモデルは落としても使われない（#123）。
    /// 自動文字起こしの ON/OFF では変わらない（要約と違ってゲートを置いていない）ことも固定する。
    #[test]
    fn model_downloads_on_select_for_speech_unless_the_path_is_overridden() {
        let base = crate::config::Config::default();
        let with = |auto_transcribe, path: Option<&str>| crate::config::Config {
            auto_transcribe,
            whisper_model_path: path.map(std::path::PathBuf::from),
            ..base.clone()
        };
        let downloads = |config: &crate::config::Config| {
            model_downloads_on_select(crate::model_download::ModelKind::Speech, config)
        };

        assert!(downloads(&with(true, None)));
        assert!(downloads(&with(false, None)));
        assert!(downloads(&base));
        assert!(!downloads(&with(true, Some("/tmp/ggml-small.bin"))));
        assert!(!downloads(&with(false, Some("/tmp/ggml-small.bin"))));
    }

    /// whisper の状態行は、上書き中だけ取得状況ではなく「上書きを使っている」ことを出す（#123）。
    /// 上書きが無いときは取得状況をそのまま出す（要約側のような契機の説明は要らない
    /// ＝選べば必ず取得が始まるため）。
    ///
    /// **選択中のモデルを解決していること**も固定する: 既定（Small）だけで見ると
    /// `spec_or_default` を `default_spec` に壊してもテストが緑のままになるため、既定ではない
    /// カタログモデル（Tiny）でサイズ入りの文言をリテラルで留める。
    #[test]
    fn whisper_model_status_line_shows_the_override() {
        let downloader = crate::model_download::ModelDownloader::new();
        let default_spec = crate::whisper_model::default_spec();
        let tiny = crate::whisper_model::spec_for("tiny").expect("tiny is in the catalog");
        downloader.set_status_for_test(
            default_spec,
            crate::model_download::DownloadStatus::Downloaded,
        );
        downloader.set_status_for_test(tiny, crate::model_download::DownloadStatus::NotDownloaded);

        // 既定以外を選んでいれば、その spec の状況とサイズが出る（選択が効いていることの担保）。
        let base = crate::config::Config::default();
        let choosing_tiny = crate::config::Config {
            whisper_model: "tiny".to_owned(),
            ..base.clone()
        };
        assert_eq!(
            whisper_model_status_line(&choosing_tiny, &downloader).text,
            "Not downloaded — downloads automatically (74 MB)"
        );
        assert_eq!(
            whisper_model_status_line(&base, &downloader).text,
            "Downloaded"
        );

        // カタログ外の手編集値は既定モデルの状況を出す（使用時のフォールバックと整合）。
        let unknown = crate::config::Config {
            whisper_model: "no-such-model".to_owned(),
            ..base.clone()
        };
        assert_eq!(
            whisper_model_status_line(&unknown, &downloader).text,
            "Downloaded"
        );

        // 上書きが無い間は説明行を出す（`overridden` の逆向きの取り違えもここで落ちる）。
        assert!(!whisper_model_status_line(&base, &downloader).overridden);

        // 上書き中は取得状況によらず同じ文言（要約側と同じ表現にする）。
        let overridden = crate::config::Config {
            whisper_model_path: Some(std::path::PathBuf::from("/tmp/ggml-small.bin")),
            ..choosing_tiny
        };
        let overridden_line = whisper_model_status_line(&overridden, &downloader);
        assert_eq!(
            overridden_line.text,
            "Using the model file set in config.toml"
        );
        // 「失敗」ではなく「選択が使われない」＝ caution。説明行も出さない。
        assert_eq!(overridden_line.tone, StatusTone::Caution);
        assert!(overridden_line.overridden);
        downloader.set_status_for_test(tiny, crate::model_download::DownloadStatus::Downloaded);
        assert_eq!(
            whisper_model_status_line(&overridden, &downloader).text,
            "Using the model file set in config.toml"
        );
    }

    /// 要約 LLM の状態行は取得状況を示す（どのモデルかは Select が示す）。取得の契機が設定で
    /// 変わるので、選んでも取得が始まらない設定では「自動で落ちる」と読める文言を出さない。
    #[test]
    fn summary_model_status_line_shows_when_the_download_happens() {
        let downloader = crate::model_download::ModelDownloader::new();
        let spec = crate::summary_model::default_spec();
        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::NotDownloaded);

        let running = crate::config::Config {
            auto_summarize: true,
            ..crate::config::Config::default()
        };
        assert_eq!(
            summary_model_status_line(&running, &downloader).text,
            "Not downloaded — downloads automatically (4.4 GB)"
        );

        // 要約 OFF（既定）では選んでも取得しないので、取得の契機を明示する。
        let idle = crate::config::Config::default();
        let idle_line = summary_model_status_line(&idle, &downloader);
        assert_eq!(
            idle_line.text,
            "Not downloaded — downloads when notes are generated (4.4 GB)"
        );
        // 取得の契機を説明するだけなので neutral（注意でも失敗でもない）。
        assert_eq!(idle_line.tone, StatusTone::Neutral);
        assert!(!idle_line.overridden);
        // 取得済みなら契機の説明は不要（状態そのものを出す）。
        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::Downloaded);
        assert_eq!(
            summary_model_status_line(&idle, &downloader).text,
            "Downloaded"
        );
        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::NotDownloaded);

        // カタログ外の手編集値は既定モデルの状況を出す（使用時のフォールバックと整合）。
        let unknown = crate::config::Config {
            summary_model: "no-such-model".to_owned(),
            ..running.clone()
        };
        assert_eq!(
            summary_model_status_line(&unknown, &downloader).text,
            summary_model_status_line(&running, &downloader).text
        );

        let overridden = crate::config::Config {
            summary_model_path: Some(std::path::PathBuf::from("/tmp/model.gguf")),
            ..running
        };
        let overridden_line = summary_model_status_line(&overridden, &downloader);
        assert_eq!(
            overridden_line.text,
            "Using the model file set in config.toml"
        );
        // 上書きは「失敗」ではなく「選択が使われない」＝ caution。説明行も出さない。
        assert_eq!(overridden_line.tone, StatusTone::Caution);
        assert!(overridden_line.overridden);
    }

    /// サイン波の代表的な位相で、期待どおりの明度レベルになることを確認する。
    /// 2 秒周期なら 0s→0.5, 0.5s(1/4)→1.0, 1.0s(1/2)→0.5, 1.5s(3/4)→0.0, 2.0s(1周)→0.5。
    #[test]
    fn breathing_level_matches_sine_phases() {
        const CYCLE: f32 = 2.0;
        let approx = |a: f32, b: f32| (a - b).abs() < 1e-4;

        assert!(approx(
            breathing_level(Duration::from_secs_f32(0.0), CYCLE),
            0.5
        ));
        assert!(approx(
            breathing_level(Duration::from_secs_f32(0.5), CYCLE),
            1.0
        ));
        assert!(approx(
            breathing_level(Duration::from_secs_f32(1.0), CYCLE),
            0.5
        ));
        assert!(approx(
            breathing_level(Duration::from_secs_f32(1.5), CYCLE),
            0.0
        ));
        // 1 周期後は位相が戻り、開始と同じ 0.5。
        assert!(approx(
            breathing_level(Duration::from_secs_f32(2.0), CYCLE),
            0.5
        ));
    }

    /// 返り値は常に 0.0〜1.0 の範囲に収まる（アルファ 0 に落ちる＝消えたようには見せない前提）。
    #[test]
    fn breathing_level_stays_within_unit_range() {
        const CYCLE: f32 = 2.0;
        for i in 0..=40 {
            let t = i as f32 * 0.05; // 0.00〜2.00 秒を 0.05 刻みで
            let level = breathing_level(Duration::from_secs_f32(t), CYCLE);
            assert!(
                (0.0..=1.0).contains(&level),
                "level {level} out of range (t={t})"
            );
        }
    }

    /// 前面化の縮退経路（ハンドル取得失敗・非 AppKit バックエンド）はパニックせず戻る。
    /// 前面化の成否（ウィンドウの前後関係）は自動判定が困難なため実機の手動確認とし、
    /// ここでは FFI 手前の分岐が安全に縮退することだけを検証する（`docs/rules/ffi.md`）。
    #[cfg(target_os = "macos")]
    #[test]
    fn raise_ns_window_returns_without_panic_on_degraded_paths() {
        use raw_window_handle::{HandleError, RawWindowHandle, WebWindowHandle};

        // SAFETY: Err と非 AppKit バリアントはポインタを参照せず縮退するため、
        // # Safety の前提（AppKit の ns_view が有効）を自明に満たす。
        unsafe {
            super::raise_ns_window(Err(HandleError::Unavailable));
            super::raise_ns_window(Ok(RawWindowHandle::Web(WebWindowHandle::new(1))));
        }
    }
}
