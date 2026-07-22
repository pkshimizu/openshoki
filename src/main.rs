//! openshoki — メニューバー／タスクバーに常駐する録音アプリ（基盤）。
//!
//! 起動時はウィンドウを表示せずトレイに常駐し、トレイメニューから Slint ウィンドウの
//! 表示/非表示とアプリ終了を行う。録音機能は後続の issue で実装する。

#[cfg(target_os = "macos")]
mod app_audio_monitor;
mod config;
mod mixdown;
mod player;
mod recorder;
mod recordings;
mod single_instance;
#[cfg(target_os = "macos")]
mod system_audio;
mod transcribe;
mod transcript;
mod tray;
mod whisper_model;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use tray_icon::menu::{IconMenuItem, MenuEvent};

use crate::config::Config;
use crate::recorder::Recorder;
use crate::tray::Tray;

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
const WINDOW_WIDTH: f32 = 420.0;
const WINDOW_HEIGHT: f32 = 790.0;
/// 初回表示位置（画面左上からの暫定値）。中央寄せ等の調整は後続に回す。
const WINDOW_X: f32 = 240.0;
const WINDOW_Y: f32 = 160.0;

/// Recordings ウィンドウの初期ジオメトリ。幅・高さは `ui/recordings-window.slint` の
/// min/preferred と一致させること（片方だけ変えない）。設定ウィンドウと重ならない位置に出す。
const RECORDINGS_WIDTH: f32 = 720.0;
const RECORDINGS_HEIGHT: f32 = 540.0;
const RECORDINGS_X: f32 = 200.0;
const RECORDINGS_Y: f32 = 120.0;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 多重起動ガード。取得したロックは _instance_lock でプロセス終了まで保持し続ける
    // （背景・各分岐の意味・保持理由は `single_instance` モジュール doc / `Acquire` 参照）。
    let _instance_lock = match single_instance::acquire() {
        single_instance::Acquire::Acquired(lock) => Some(lock),
        single_instance::Acquire::AlreadyRunning => {
            eprintln!("Exiting because another instance of openshoki is already running.");
            return Ok(());
        }
        single_instance::Acquire::Unavailable => None,
    };

    // ウィンドウは生成するが表示はしない（起動時はトレイのみ）。
    let ui = AppWindow::new()?;

    // 設定を読み込み、現在の保存先・自動録音トグル・登録アプリ一覧を画面へ反映する。
    // 失敗時は load() がデフォルトを返す。
    let config = Rc::new(RefCell::new(Config::load()));

    // 内蔵 whisper モデルのダウンロード・状態管理。設定画面（モデル選択・DL 状況表示）と
    // 文字起こしワーカーで同じ状態を共有し、同一モデルの二重ダウンロードを防ぐ。
    let model_downloader = whisper_model::ModelDownloader::new();

    ui.set_recording_dir(recording_dir_text(&config.borrow().recording_dir));
    ui.set_auto_record_app(config.borrow().auto_record_on_app_mic);
    // 保存値は load 時に範囲へ正規化済みなので、そのまま表示へ渡す。
    ui.set_auto_stop_debounce_secs(config.borrow().auto_stop_debounce_secs as i32);
    ui.set_auto_transcribe(config.borrow().auto_transcribe);
    // 文字起こし言語: 表示名一覧はカタログ（TRANSCRIBE_LANGUAGES）から組み立てる。選択位置は
    // 設定の言語コードから解決し、カタログ外の手編集値は既定（English）位置に表示される
    // （値は書き換えず、ユーザーが ComboBox を操作した時点で上書き保存される）。
    ui.set_transcribe_languages(
        Rc::new(slint::VecModel::<slint::SharedString>::from(
            config::TRANSCRIBE_LANGUAGES
                .iter()
                .map(|(_, display)| slint::SharedString::from(*display))
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    ui.set_transcribe_language_index(config::transcribe_language_index(
        &config.borrow().transcribe_language,
    ) as i32);
    // 内蔵 whisper モデル: 表示名一覧はカタログから「名前 — サイズ — 説明」を組み立てる。
    // 選択位置は設定のモデル ID から解決し、カタログ外の手編集値は既定（Small）位置に表示される。
    ui.set_whisper_models(
        Rc::new(slint::VecModel::<slint::SharedString>::from(
            whisper_model::CATALOG
                .iter()
                .map(|spec| {
                    slint::SharedString::from(format!(
                        "{} — {} — {}",
                        spec.display_name,
                        whisper_model::format_size(spec.size_bytes),
                        spec.description
                    ))
                })
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    ui.set_whisper_model_index(whisper_model::model_index(&config.borrow().whisper_model) as i32);
    ui.set_whisper_model_status(
        selected_model_status_text(&config.borrow().whisper_model, &model_downloader).into(),
    );
    // 登録アプリの表示名一覧を Slint のモデルで持ち、追加/削除で更新する。
    let app_list_model = Rc::new(slint::VecModel::<slint::SharedString>::from(
        config
            .borrow()
            .app_mic_triggers
            .iter()
            .map(|trigger| slint::SharedString::from(trigger.name.as_str()))
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

    // 自動停止デバウンス秒数の変更: SpinBox の値を範囲へ丸めて永続化し、成功後にメモリへ反映する。
    // SpinBox 側でも minimum/maximum を持つが、手編集された設定値との整合のため保存側でも丸める。
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
        // 丸めた値を SpinBox へ反映し、表示・メモリ・ディスクを一致させる。
        ui.set_auto_stop_debounce_secs(secs as i32);
        *config_for_debounce.borrow_mut() = candidate;
    });

    // 「録音停止時に自動文字起こし」トグル: 永続化に成功してから反映する。Slint 側は先に
    // チェック状態を新値へ更新するため、保存失敗時は表示を保存済みの値へ戻す
    // （docs/rules/slint.md。自動録音トグルと対称）。モデルは内蔵（初回に自動ダウンロード）
    // なので、ここではモデルの選択・検証は行わない。
    let config_for_transcribe = Rc::clone(&config);
    let ui_for_transcribe = ui.as_weak();
    ui.on_toggle_auto_transcribe(move |enabled| {
        let Some(ui) = ui_for_transcribe.upgrade() else {
            return;
        };
        let mut candidate = config_for_transcribe.borrow().clone();
        candidate.auto_transcribe = enabled;
        if let Err(err) = candidate.save() {
            eprintln!(
                "Not changing the auto-transcribe setting because saving the settings failed: {err}"
            );
            ui.set_auto_transcribe(config_for_transcribe.borrow().auto_transcribe);
            return;
        }
        *config_for_transcribe.borrow_mut() = candidate;
    });

    // 文字起こし言語の変更: ComboBox のインデックスをカタログの言語コードへ変換して永続化する。
    // Slint 側は先に選択位置を新値へ更新するため、保存失敗時は表示を保存済みの値へ戻す
    // （docs/rules/slint.md）。
    let config_for_language = Rc::clone(&config);
    let ui_for_language = ui.as_weak();
    ui.on_change_transcribe_language(move |index| {
        let Some(ui) = ui_for_language.upgrade() else {
            return;
        };
        // ComboBox は Rust が渡したカタログの範囲しか返さないが、防御的に既定（先頭）へ丸める。
        let code = usize::try_from(index)
            .ok()
            .and_then(|i| config::TRANSCRIBE_LANGUAGES.get(i))
            .unwrap_or(&config::TRANSCRIBE_LANGUAGES[0])
            .0;
        let mut candidate = config_for_language.borrow().clone();
        candidate.transcribe_language = code.to_owned();
        if let Err(err) = candidate.save() {
            eprintln!(
                "Not changing the transcription language because saving the settings failed: {err}"
            );
            ui.set_transcribe_language_index(config::transcribe_language_index(
                &config_for_language.borrow().transcribe_language,
            ) as i32);
            return;
        }
        *config_for_language.borrow_mut() = candidate;
    });

    // 内蔵 whisper モデルの変更: ComboBox のインデックスをカタログの ID へ変換して永続化し、
    // 未取得なら即バックグラウンドでダウンロードを開始する（進捗はタイマーが状態行へ反映する）。
    // Slint 側は先に選択位置を新値へ更新するため、保存失敗時は表示を保存済みの値へ戻す
    // （docs/rules/slint.md）。
    let config_for_model = Rc::clone(&config);
    let ui_for_model = ui.as_weak();
    let downloader_for_model = model_downloader.clone();
    ui.on_change_whisper_model(move |index| {
        let Some(ui) = ui_for_model.upgrade() else {
            return;
        };
        // ComboBox は Rust が渡したカタログの範囲しか返さないが、防御的に既定へ丸める。
        let spec = usize::try_from(index)
            .ok()
            .and_then(|i| whisper_model::CATALOG.get(i))
            .unwrap_or_else(|| whisper_model::default_spec());
        let mut candidate = config_for_model.borrow().clone();
        candidate.whisper_model = spec.id.to_owned();
        if let Err(err) = candidate.save() {
            eprintln!("Not changing the Whisper model because saving the settings failed: {err}");
            ui.set_whisper_model_index(whisper_model::model_index(
                &config_for_model.borrow().whisper_model,
            ) as i32);
            return;
        }
        *config_for_model.borrow_mut() = candidate;
        // 選択したモデルが未取得（または直近失敗）なら取得を開始する（取得済み・DL 中は
        // request_download 側が早期 return する）。
        downloader_for_model.request_download(spec);
        ui.set_whisper_model_status(
            selected_model_status_text(spec.id, &downloader_for_model).into(),
        );
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
            let name = slint::SharedString::from(trigger.name.as_str());
            candidate.app_mic_triggers.push(trigger);
            if let Err(err) = candidate.save() {
                eprintln!("Not adding the app because saving the settings failed: {err}");
                return;
            }
            model_for_add.push(name);
            *config_for_add.borrow_mut() = candidate;
        });
    }

    // Slint バックエンドの初期化後にトレイを常駐させる（macOS の NSApplication 初期化後）。
    let tray = Tray::new()?;

    // 登録アプリのマイク使用を監視するモニタ（macOS 14.4+）。照会は失敗しても落ちない設計のため、
    // 生成は常に成功する。実際に照会できるかはポーリング時に判定する。
    #[cfg(target_os = "macos")]
    let app_monitor = app_audio_monitor::AppAudioMonitor::new();

    // 文字起こしのバックグラウンドワーカー。設定 OFF の間はジョブが来ないだけで、常駐コストは
    // アイドルなスレッド 1 本のみ。起動失敗時は文字起こしだけが無効化される（録音は無関係）。
    let transcriber = transcribe::TranscribeWorker::start(model_downloader.clone());

    // 録音停止後の後処理（極小音量の正規化→ミックス生成→文字起こし投入）を直列に行う
    // バックグラウンドワーカー。文字起こしは後処理ワーカーが完了後に投入するため、
    // transcriber はここで所有を渡す（正規化後の音声で文字起こしさせる）。
    let postprocessor = mixdown::PostProcessWorker::start(transcriber);

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
    // 選択中セッションのトランスクリプト（セグメントクリック→開始秒の解決、tick→現在セグメントの
    // 算出に使う）。選択のたびに読み直す。
    let transcript_segments: Rc<RefCell<Vec<transcript::TranscriptSegment>>> =
        Rc::new(RefCell::new(Vec::new()));

    // セッション選択: 詳細を更新し、その音源を再生準備（停止状態でロード。Play で再生開始）。
    {
        let player = Rc::clone(&player);
        let sessions = Rc::clone(&sessions);
        let transcript_segments = Rc::clone(&transcript_segments);
        let rec_weak = recordings_ui.as_weak();
        recordings_ui.on_select_session(move |index| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            let sessions = sessions.borrow();
            let Some(session) = usize::try_from(index).ok().and_then(|i| sessions.get(i)) else {
                return;
            };
            rec.set_has_selection(true);
            rec.set_detail_datetime(session.display_datetime.clone().into());
            rec.set_detail_summary(session.source_summary().into());
            // 文字起こしを読み込み、話者ラベル＋開始時刻付きのセグメント一覧を更新する
            // （空＝欠落・破損・未生成なら Slint 側が縮退表示する）。
            let segments = transcript::load_transcript(&session.dir);
            rec.set_segments(Rc::new(slint::VecModel::from(transcript_rows(&segments))).into());
            rec.set_current_segment(-1);
            *transcript_segments.borrow_mut() = segments;
            rec.set_playing(false);
            rec.set_progress(0.0);
            // 再生対象は事前生成の mix.mp3（両音源）か単一音源ファイル。両音源で mix.mp3 が
            // まだ無ければ再生不可（選択時にその場でミックスして UI を固めない）。
            rec.set_playable(session.is_playable());
            let duration = match session.playback_path() {
                Some(path) => match player.borrow_mut().as_mut() {
                    Some(p) => match p.load(&path) {
                        Ok(()) => p.duration(),
                        Err(err) => {
                            eprintln!("Failed to load the recording for playback: {err}");
                            None
                        }
                    },
                    None => None,
                },
                None => None,
            };
            rec.set_time_text(format_playback_time(Duration::ZERO, duration).into());
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
                rec.set_progress(0.0);
                rec.set_time_text(format_playback_time(Duration::ZERO, p.duration()).into());
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
            if let Some(p) = player.borrow().as_ref() {
                p.seek(segment.start_duration());
            }
            // クリックしたセグメントを即ハイライトする（次の tick で位置に追従する）。
            rec.set_current_segment(index);
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
                sessions: Rc::clone(&sessions),
                transcript_segments: Rc::clone(&transcript_segments),
            },
            &tray,
            Rc::clone(&config),
            postprocessor,
            model_downloader.clone(),
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
    sessions: Rc<RefCell<Vec<recordings::RecordingSession>>>,
    transcript_segments: Rc<RefCell<Vec<transcript::TranscriptSegment>>>,
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
    tray: &Tray,
    config: Rc<RefCell<Config>>,
    postprocessor: mixdown::PostProcessWorker,
    model_downloader: whisper_model::ModelDownloader,
    #[cfg(target_os = "macos")] app_monitor: app_audio_monitor::AppAudioMonitor,
) -> impl FnMut() + 'static {
    // Recordings ウィンドウ・再生・一覧のハンドルは 1 つにまとめて受け取り、ここで分解する。
    let RecordingsHandles {
        ui: rec_ui,
        player,
        sessions,
        transcript_segments,
    } = recordings;
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
                let Some(rec) = rec_ui.upgrade() else {
                    continue;
                };
                open_recordings_window(
                    &rec,
                    &config,
                    &player,
                    &sessions,
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
        // （閉じているときは更新しない＝アイドル時の無駄な描画をしない）。
        if let Some(rec) = rec_ui.upgrade()
            && rec.window().is_visible()
            && let Some(player) = player.borrow().as_ref()
        {
            let position = player.position();
            let duration = player.duration();
            let secs = position.as_secs();
            if last_play_secs != Some(secs) {
                rec.set_time_text(format_playback_time(position, duration).into());
                last_play_secs = Some(secs);
            }
            let progress = match duration {
                Some(total) if total > Duration::ZERO => {
                    (position.as_secs_f32() / total.as_secs_f32()).clamp(0.0, 1.0)
                }
                _ => 0.0,
            };
            rec.set_progress(progress);
            rec.set_playing(player.is_playing());
            // 再生位置に対応するトランスクリプトのセグメントをハイライトする（該当なしは -1）。
            let current =
                transcript::current_index(&transcript_segments.borrow(), position.as_secs_f64())
                    .and_then(|index| i32::try_from(index).ok())
                    .unwrap_or(-1);
            rec.set_current_segment(current);
        }

        // 設定ウィンドウが開いている間だけ、選択中モデルの取得状況（ダウンロード進捗等）を
        // 状態行へ反映する（閉じているときは更新しない。変化したときだけ set して無駄な
        // 再描画を避ける）。
        if let Some(ui) = ui.upgrade()
            && ui.window().is_visible()
        {
            let status =
                selected_model_status_text(&config.borrow().whisper_model, &model_downloader);
            if ui.get_whisper_model_status() != status.as_str() {
                ui.set_whisper_model_status(status.into());
            }
        }
    }
}

/// トレイの「Recordings…」で Recordings ウィンドウを開く。保存先を走査して一覧を更新し、
/// 選択・再生状態を初期化してから表示する（初回表示はジオメトリを明示する。`docs/rules/slint.md`）。
fn open_recordings_window(
    rec: &RecordingsWindow,
    config: &Rc<RefCell<Config>>,
    player: &Rc<RefCell<Option<player::AudioPlayer>>>,
    sessions: &Rc<RefCell<Vec<recordings::RecordingSession>>>,
    geometry_committed: &mut bool,
    last_play_secs: &mut Option<u64>,
) {
    let list = recordings::list_sessions(&config.borrow().recording_dir);
    let rows: Vec<SessionRow> = list
        .iter()
        .map(|session| SessionRow {
            datetime: session.display_datetime.clone().into(),
            has_mic: session.has_mic,
            has_system: session.has_system,
            has_transcript: session.has_transcript,
        })
        .collect();
    rec.set_sessions(Rc::new(slint::VecModel::from(rows)).into());
    // 開くたびに未選択・停止表示へ初期化する。
    rec.set_selected_index(-1);
    rec.set_has_selection(false);
    rec.set_playing(false);
    rec.set_progress(0.0);
    rec.set_time_text(format_playback_time(Duration::ZERO, None).into());
    *sessions.borrow_mut() = list;
    *last_play_secs = None;
    // 前回の再生が残っていれば止める。
    if let Some(p) = player.borrow().as_ref() {
        p.stop();
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

/// 保存済みセッションの後処理（正規化→ミックス→文字起こし）を組み立てて投入する。
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
            audio_paths: saved.to_vec(),
            model_id: config_ref.whisper_model.clone(),
            model_override: config_ref.whisper_model_path.clone(),
            language: config_ref.transcribe_language.clone(),
        });
    postprocessor.submit(mixdown::PostProcessJob {
        session_dir: session_dir.to_path_buf(),
        saved: saved.to_vec(),
        transcribe,
    });
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
        eprintln!("Failed to show the window: {err}");
    }
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

/// 保存先パスを画面表示用の文字列に変換する。
fn recording_dir_text(dir: &std::path::Path) -> slint::SharedString {
    dir.display().to_string().into()
}

/// 設定で選択中の whisper モデルの取得状況を、設定画面の状態行テキストにする。
/// カタログ外の手編集値は既定モデルの状況を表示する（表示位置のフォールバックと整合）。
fn selected_model_status_text(
    model_id: &str,
    downloader: &whisper_model::ModelDownloader,
) -> String {
    let spec = whisper_model::spec_for(model_id).unwrap_or_else(|| whisper_model::default_spec());
    match downloader.status_of(spec) {
        whisper_model::DownloadStatus::NotDownloaded => format!(
            // 未取得モデルは「選択した時点」または「次の文字起こし時」に自動取得される。
            // どちらかに限定した文言にしない（両方の経路がある）。
            "Not downloaded — downloads automatically ({})",
            whisper_model::format_size(spec.size_bytes)
        ),
        whisper_model::DownloadStatus::Downloading { received, total } => {
            // total は Content-Length または既知サイズで常に正だが、防御的にゼロ除算を避ける。
            // Content-Length が実サイズより小さい異常時も 100% を超えて表示しない。
            let percent = (received.saturating_mul(100) / total.max(1)).min(100);
            format!("Downloading… {percent}%")
        }
        whisper_model::DownloadStatus::Downloaded => "Downloaded".to_owned(),
        whisper_model::DownloadStatus::Failed(reason) => format!("Download failed: {reason}"),
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

#[cfg(test)]
mod tests {
    use super::{breathing_level, selected_model_status_text};
    use std::time::Duration;

    /// サイン波の代表的な位相で、期待どおりの明度レベルになることを確認する。
    /// 2 秒周期なら 0s→0.5, 0.5s(1/4)→1.0, 1.0s(1/2)→0.5, 1.5s(3/4)→0.0, 2.0s(1周)→0.5。
    #[test]
    fn model_status_text_covers_all_states() {
        let downloader = crate::whisper_model::ModelDownloader::new();
        let spec = crate::whisper_model::spec_for("large-v3").expect("large-v3 is in the catalog");

        downloader.set_status_for_test(
            spec,
            crate::whisper_model::DownloadStatus::Downloading {
                received: 25,
                total: 100,
            },
        );
        assert_eq!(
            selected_model_status_text("large-v3", &downloader),
            "Downloading… 25%"
        );

        // Content-Length が実サイズより小さい異常時も 100% を超えない。
        downloader.set_status_for_test(
            spec,
            crate::whisper_model::DownloadStatus::Downloading {
                received: 300,
                total: 100,
            },
        );
        assert_eq!(
            selected_model_status_text("large-v3", &downloader),
            "Downloading… 100%"
        );

        downloader.set_status_for_test(spec, crate::whisper_model::DownloadStatus::Downloaded);
        assert_eq!(
            selected_model_status_text("large-v3", &downloader),
            "Downloaded"
        );

        downloader.set_status_for_test(
            spec,
            crate::whisper_model::DownloadStatus::Failed("boom".into()),
        );
        assert_eq!(
            selected_model_status_text("large-v3", &downloader),
            "Download failed: boom"
        );

        downloader.set_status_for_test(spec, crate::whisper_model::DownloadStatus::NotDownloaded);
        assert_eq!(
            selected_model_status_text("large-v3", &downloader),
            "Not downloaded — downloads automatically (2.9 GB)"
        );
    }

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
}
