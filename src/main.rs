//! shoki — メニューバー／タスクバーに常駐する録音アプリのエントリポイント。
//!
//! 起動時はウィンドウを表示せずトレイに常駐し、トレイメニューから設定ウィンドウ・Library
//! ウィンドウの表示/非表示とアプリ終了を行う。録音・文字起こし・議事録生成は各モジュール
//! （`recorder` / `transcribe` / `summarize`）が持ち、ここは UI との配線とタイマー駆動の
//! 状態追従（メニューバー表示・再生位置・進行状況）を担う。

#[cfg(target_os = "macos")]
mod app_audio_monitor;
mod atomic_replace;
mod config;
mod dataless;
mod inference_slot;
mod library_text;
mod mixdown;
mod model_download;
mod player;
mod private_file;
mod recorder;
mod recordings;
mod single_instance;
mod slint_map;
mod summarize;
mod summary_model;
#[cfg(target_os = "macos")]
mod system_audio;
mod transcribe;
mod transcript;
mod tray;
mod whisper_model;
mod windows;

use shoki_core::{
    SummaryPane, TranscriptInput, TranscriptPane, actions_allowed_while_busy, elapsed_text,
    summary_status_text,
};
// **core と同名の型（`TranscriptStatus` / `SummaryStatus` / `PaneAction` / `PaneActionKind`）は
// 裸で書かない**（#188）。`slint::include_modules!()` が生成型をクレート直下に置くので、裸の
// 名前は Slint 型を指してしまい、core の同名の型と読み分けられない。Slint 側は `Ui` 付きの
// 別名、core 側は `shoki_core::` で修飾する。
//
// **衝突しない生成型（`LibraryWindow` / `SessionRow` / `StatusTone` など）はそのままでよい。**
use slint_map::{UiPaneAction, UiPaneActionKind};

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

// VecModel の row_data / set_row_data（tick の行単位更新）に必要。

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

/// Library ウィンドウの初期ジオメトリ。幅・高さは `ui/library-window.slint` の
/// min/preferred と一致させること（片方だけ変えない）。設定ウィンドウと重ならない位置に出す。
const LIBRARY_WIDTH: f32 = 1100.0;
const LIBRARY_HEIGHT: f32 = 720.0;
const LIBRARY_X: f32 = 200.0;
const LIBRARY_Y: f32 = 120.0;

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
    // （正規化後の音声で文字起こしさせる）。transcriber は Clone 共有で、Library ウィンドウの
    // 手動再実行・状態表示も同じワーカー・同じ状態マップを使う。
    let postprocessor = mixdown::PostProcessWorker::start(transcriber.clone());

    // ウィンドウを閉じても終了させず、非表示にして常駐を保つ。メニューからは開くだけで、
    // 閉じるのはウィンドウ自身の閉じるボタンに任せる。
    ui.window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);

    // Library ウィンドウ（録音一覧＋再生）。設定ウィンドウと同じく起動時に生成して隠しておき、
    // トレイの「Library…」で表示する。閉じても常駐を保つ。
    let library_ui = LibraryWindow::new()?;
    // 選んだ録音の読み込み結果を UI スレッドへ返す道（#152）。**tick が受け取る**——Slint の
    // プロパティも `Rc` の共有状態も UI スレッド専有なので、読み込みスレッドからは触れない。
    let (load_sender, load_receiver) = std::sync::mpsc::channel::<LoadedSession>();
    // 選択の世代。**遅れて届いた結果で新しい選択を上書きしない**ための番号で、選ぶたびに増やす
    // （速く切り替えると、前の読み込みがあとから返ってくる）。
    let load_generation = Rc::new(Cell::new(0u64));
    // 一覧の走査結果を UI スレッドへ返す道（#181。`load_receiver` と同じ理由）。
    let (scan_sender, scan_receiver) = std::sync::mpsc::channel::<ScannedSessions>();
    // 走査の世代。**閉じて開き直したときに、古い走査の結果で一覧を書き換えない**ための番号。
    // 閉じるときにも降ろすので、閉じるハンドラより前に用意する。
    let scan_generation = Rc::new(Cell::new(0u64));
    // 走査がいまどうなっているか（#181。`ScanState` の doc）。初期値が `Settled` なのは、
    // まだ一度も開いていない＝走査は飛んでいない、という意味（一覧も空）。
    let scan_state = Rc::new(Cell::new(ScanState::Settled));
    // **文字起こしの状態**（#188）。変える口は `shoki_core::update` だけで、ここに直接
    // 書く経路は作らない。
    let app_state: Rc<RefCell<shoki_core::AppState>> =
        Rc::new(RefCell::new(shoki_core::AppState::default()));
    // 検索の世代（#161）。閉じるときにも降ろすので、閉じるハンドラより前に用意する。
    let search_generation: Rc<Cell<u64>> = Rc::new(Cell::new(0));
    // **最後の検索で本文を読めなかった録音**（#182。理由は `not_downloaded_count` の doc）。
    let search_not_downloaded: Rc<RefCell<Vec<std::path::PathBuf>>> =
        Rc::new(RefCell::new(Vec::new()));

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
    // 走査で見つかった全部（検索を解除したときに戻す元。`LibraryHandles::all_sessions`）。
    let all_sessions: Rc<RefCell<Vec<recordings::RecordingSession>>> =
        Rc::new(RefCell::new(Vec::new()));
    let (search_sender, search_receiver) = std::sync::mpsc::channel::<SearchResult>();
    let sessions_model = Rc::new(SessionRows::new());
    library_ui.set_sessions(sessions_model.model().into());
    // 選択中セッションのトランスクリプト（セグメントクリック→開始秒の解決、tick→現在セグメントの
    // 算出に使う）。選択のたびに読み直す。
    // 表示中の文字起こし（セグメントと「揃っているか」を 1 つの器に。#175）。
    let transcript_segments: Rc<RefCell<LoadedTranscript>> =
        Rc::new(RefCell::new(LoadedTranscript::unknown()));
    // core が返した依頼を実行するのに要るものを束ねる（#188。`EffectRunner` の doc）。
    let runner = EffectRunner {
        ui: library_ui.as_weak(),
        state: Rc::clone(&app_state),
        segments: Rc::clone(&transcript_segments),
        sessions: Rc::clone(&sessions),
        player: Rc::clone(&player),
        load_generation: Rc::clone(&load_generation),
        load_sender: load_sender.clone(),
    };

    {
        // 隠すときも**世代を進める**（#152）。進めないと、閉じたあとに届いた読み込み結果が
        // 誰も見ていない画面へ適用され、音声のハンドルと文字起こし本文を次に開くまで抱え続ける。
        let generation = Rc::clone(&load_generation);
        let search_generation = Rc::clone(&search_generation);
        let scan_generation = Rc::clone(&scan_generation);
        let close_state = Rc::clone(&app_state);
        let close_runner = runner.clone();
        library_ui.window().on_close_requested(move || {
            advance_load_generation(&generation);
            // **表示中の中身も落とす**（#188）。閉じたあとに届いた読み込みが誰も見ていない画面へ
            // 入り、発話本文を次に開くまで抱え続ける——世代を進めるだけでは止まらない
            // （届いた結果を捨てるかは core が `selected` で決めるので、そちらも解除する）。
            //
            // **再生は止めない**。音源を手放すのは「音源を差し替える」依頼のときと、開き直した
            // ときだけ——閉じても鳴っているものは鳴り続ける（既存の挙動）。
            let effects = {
                let mut state = close_state.borrow_mut();
                shoki_core::update(
                    &mut state,
                    shoki_core::Msg::Command(shoki_core::Command::Select(None)),
                )
            };
            run_effects(&close_runner, effects, None);
            // 検索も同じ理由で降ろす（走っていると、次に開いた一覧を後から絞り込む）。
            advance_search_generation(&search_generation);
            // **走査も降ろす**（#181）。進めないと、閉じたあとも保存先を舐め続けたうえ、
            // 届いた結果が誰も見ていない一覧へ普通に適用される（`ui.upgrade()` は
            // `HideWindow` なので成功する）。掃除も降りた走査では走らない。
            advance_scan_generation(&scan_generation);
            slint::CloseRequestResponse::HideWindow
        });
    }

    // セッション選択: 詳細を更新し、その音源を再生準備する。
    //
    // **重い読み込みは別スレッドへ出す**（#152）。文字起こし JSON のパースと、音声のデコーダを
    // 開く処理（MP3 は全長を得るためにファイルを走査する）は録音の長さに比例して重く、UI
    // スレッドでやると 1 時間の録音では数秒画面が固まる。ここでやるのは「すぐ出せるものを出す」
    // ことだけで、残りは届いた順に反映する。
    {
        let sessions = Rc::clone(&sessions);
        let app_state = Rc::clone(&app_state);
        let summarizer = summarizer.clone();
        let config = Rc::clone(&config);
        let rec_weak = library_ui.as_weak();
        let runner = runner.clone();
        library_ui.on_select_session(move |index| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            let session = {
                let sessions_ref = sessions.borrow();
                let Some(session) = usize::try_from(index)
                    .ok()
                    .and_then(|i| sessions_ref.get(i))
                else {
                    return;
                };
                session.clone()
            };

            // --- ここまでが即時。ディスクを読まずに出せるものだけを入れる ---
            rec.set_has_selection(true);
            // 一覧の行と**同じ組み立て**にする（`Aug 10, 2026 · 14:02`）。左右で日時の形が
            // 違うと、同じ録音を見ていることが読み取りにくい。
            rec.set_detail_datetime(
                format!("{} · {}", session.display_date(), session.display_time()).into(),
            );
            rec.set_detail_sources(session.source_summary().into());
            rec.set_has_transcript(session.has_transcript);
            rec.set_playing(false);

            // **選んだことを core へ伝える**（#188）。前の中身を落とすか・音源を差し替えるかを
            // 決めるのは `update`——同じ録音を選び直したときに落とすと、伏せてある途中結果が
            // 1 tick 開く（#175）。
            //
            // **借用を落としてから実行する**（`run_effects` の doc）。
            let effects = {
                let mut state = app_state.borrow_mut();
                shoki_core::update(
                    &mut state,
                    shoki_core::Msg::Command(shoki_core::Command::Select(Some(
                        session.dir.clone(),
                    ))),
                )
            };
            run_effects(&runner, effects, None);

            // 状態の表示は、落としたあとの状態から組み直す。
            refresh_detail_panes(&rec, &runner, &summarizer, &session, &config);
        });
    }

    // 再生/一時停止トグル。
    {
        let player = Rc::clone(&player);
        let rec_weak = library_ui.as_weak();
        library_ui.on_play_pause(move || {
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
        let rec_weak = library_ui.as_weak();
        library_ui.on_stop(move || {
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
        let rec_weak = library_ui.as_weak();
        library_ui.on_seek_to_segment(move |index| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            let loaded = transcript_segments.borrow();
            let Some(segment) = usize::try_from(index)
                .ok()
                .and_then(|i| loaded.transcript.segments.get(i))
            else {
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
        let rec_weak = library_ui.as_weak();
        library_ui.on_scrub_preview(move |ratio| {
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
        let rec_weak = library_ui.as_weak();
        library_ui.on_seek_to_ratio(move |ratio| {
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
    {
        let sessions = Rc::clone(&sessions);
        let config = Rc::clone(&config);
        let runner = runner.clone();
        let transcriber = transcriber.clone();
        // 読む領域は両タブまとめて組み直すので、相手のワーカーも要る（`refresh_detail_panes`）。
        let summarizer = summarizer.clone();
        let rec_weak = library_ui.as_weak();
        library_ui.on_transcribe_session(move |index| {
            let sessions = sessions.borrow();
            let Some(session) = usize::try_from(index).ok().and_then(|i| sessions.get(i)) else {
                return;
            };
            submit_transcription(session, &config, &transcriber, ChainNotes::FollowTheSetting);
            observe_jobs(&transcriber, &runner);
            if let Some(rec) = rec_weak.upgrade() {
                refresh_detail_panes(&rec, &runner, &summarizer, session, &config);
            }
        });
    }

    // Notes タブの「Transcribe, then write notes」: 文字起こしを走らせ、**全音源成功したときだけ**
    // 続けて議事録を書く（#165）。続けるかどうかの判断は `TranscribeWorker` が持っていて、
    // ここは依頼を添えるだけ（`TranscribeJob.summarize` の doc）。
    {
        let sessions = Rc::clone(&sessions);
        let config = Rc::clone(&config);
        let runner = runner.clone();
        let transcriber = transcriber.clone();
        let summarizer = summarizer.clone();
        let rec_weak = library_ui.as_weak();
        library_ui.on_transcribe_then_notes(move |index| {
            let sessions = sessions.borrow();
            let Some(session) = usize::try_from(index).ok().and_then(|i| sessions.get(i)) else {
                return;
            };
            submit_transcription(session, &config, &transcriber, ChainNotes::Always);
            observe_jobs(&transcriber, &runner);
            if let Some(rec) = rec_weak.upgrade() {
                refresh_detail_panes(&rec, &runner, &summarizer, session, &config);
            }
        });
    }

    // 「Summarize」: 選択中セッションの議事録生成を手動で（再）生成する。設定 `auto_summarize`
    // とは独立で、押されたら生成する（文字起こしが無いセッションは Slint 側でボタンが無効）。
    // ジョブの組み立て・設定のスナップショットは `manual_summarize_job`（その doc が正）。
    {
        let sessions = Rc::clone(&sessions);
        let config = Rc::clone(&config);
        let runner = runner.clone();
        let summarizer = summarizer.clone();
        let rec_weak = library_ui.as_weak();
        library_ui.on_summarize_session(move |index| {
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
                refresh_detail_panes(&rec, &runner, &summarizer, session, &config);
            }
        });
    }

    // 「Cancel」: キュー待ちの要約ジョブを取り消す（走り出したものは取り消せない。理由は
    // `SummarizeWorker::cancel` の doc）。ボタンはキュー待ちの間だけ Cancel になる。
    {
        let sessions = Rc::clone(&sessions);
        let summarizer = summarizer.clone();
        let config = Rc::clone(&config);
        let runner = runner.clone();
        let rec_weak = library_ui.as_weak();
        library_ui.on_cancel_summary(move |index| {
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
                refresh_detail_panes(&rec, &runner, &summarizer, session, &config);
            }
        });
    }

    // 「Stop」押下（#163）。走っている文字起こしへ中断を伝える（キュー待ちならその場で外す）。
    // **結果は即反映する**——押した瞬間に「Stopping…」へ移らないと、押しても何も起きていない
    // ように見える（実際に降りるのはワーカーが気づいたとき。tick が拾って未実施へ戻す）。
    {
        let sessions = Rc::clone(&sessions);
        let transcriber = transcriber.clone();
        let summarizer = summarizer.clone();
        let config = Rc::clone(&config);
        let runner = runner.clone();
        let rec_weak = library_ui.as_weak();
        library_ui.on_stop_transcription(move |index| {
            let sessions = sessions.borrow();
            let Some(session) = usize::try_from(index).ok().and_then(|i| sessions.get(i)) else {
                return;
            };
            match transcriber.stop(&session.dir) {
                transcribe::StopOutcome::Stopping => {
                    println!("Stopping the transcription that is running");
                }
                transcribe::StopOutcome::Cancelled => {
                    println!("Canceled the transcription that was waiting to start");
                }
                transcribe::StopOutcome::NotRunning => {
                    // 走ってもキューにも載っていなかった（終わった直後に押された）。tick が
                    // 状態を更新する前の数十 ms に押されると起こる。表示は次の tick が直す。
                    eprintln!("Skipping the stop because the transcription is no longer running");
                }
            }
            observe_jobs(&transcriber, &runner);
            if let Some(rec) = rec_weak.upgrade() {
                refresh_detail_panes(&rec, &runner, &summarizer, session, &config);
            }
        });
    }

    // 一覧の検索（#161）。**絞り込みは背景スレッド**で行い、結果は tick が拾って反映する。
    // ここでは世代を進めて投げるだけ——打ち込むたびに走るので、古い結果が新しい入力を
    // 上書きしないようにする。
    {
        let all_sessions = Rc::clone(&all_sessions);
        let search_generation = Rc::clone(&search_generation);
        let search_not_downloaded = Rc::clone(&search_not_downloaded);
        let search_sender = search_sender.clone();
        let rec_weak = library_ui.as_weak();
        library_ui.on_search(move |needle| {
            let generation = advance_search_generation(&search_generation);
            // **古い語の件数を出し続けない**（#182）。結果が届くまでは 0 になるが、いま打って
            // いる語と食い違う数を見せるより安全側（削除の経路もこの値から数え直す）。
            search_not_downloaded.borrow_mut().clear();
            // **入力を書き換えない**。空白だけを打っている最中（日本語入力の区切りなど）に
            // 欄の中身が消えると、何が起きたか分からない。解除は本当に空のときだけ。
            if needle.is_empty() {
                if let Some(rec) = rec_weak.upgrade() {
                    rec.invoke_clear_search();
                }
                return;
            }
            let needle = needle.trim().to_owned();
            if needle.is_empty() {
                // 空白だけ。絞り込まずに待つ（世代は上で進めたので、走っている検索は降りる）。
                return;
            }
            spawn_search(
                needle,
                all_sessions.borrow().clone(),
                generation,
                &search_sender,
            );
        });
    }

    // 検索の解除。**世代を進めてから戻す**——走っている検索の結果が後から届いて絞り直すのを
    // 防ぐ。
    {
        let all_sessions = Rc::clone(&all_sessions);
        let sessions = Rc::clone(&sessions);
        let sessions_model = Rc::clone(&sessions_model);
        let search_generation = Rc::clone(&search_generation);
        let search_not_downloaded = Rc::clone(&search_not_downloaded);
        let scan_state = Rc::clone(&scan_state);
        let runner = runner.clone();
        let rec_weak = library_ui.as_weak();
        library_ui.on_clear_search(move || {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            reset_search(&rec, &search_generation, &search_not_downloaded);
            let all = all_sessions.borrow().clone();
            let total = all.len();
            sessions_model.replace_all(&all, &runner.state.borrow());
            apply_list_counts(
                &rec,
                ListCounts {
                    shown: total,
                    total,
                    not_downloaded: 0,
                },
                &scan_state,
            );
            reselect_after_list_change(&rec, &sessions, all, &runner);
        });
    }

    // 読む領域の空表示から起こす操作（#154）。**振り分けはここ 1 箇所の網羅 match**——
    // Slint 側で分岐させると、操作を足したときに漏れても静かに何も起きないだけになる。
    // 行き先は既存のコールバックと同じで、押す場所が増えただけ。
    {
        let rec_weak = library_ui.as_weak();
        let app_weak = ui.as_weak();
        library_ui.on_pane_action(move |kind| {
            let Some(rec) = rec_weak.upgrade() else {
                return;
            };
            // 対象は常に選択中のセッション（空表示は選択中のものしか出ない）。
            let index = rec.get_selected_index();
            match kind {
                UiPaneActionKind::Transcribe => rec.invoke_transcribe_session(index),
                UiPaneActionKind::WriteNotes => rec.invoke_summarize_session(index),
                UiPaneActionKind::CancelNotes => rec.invoke_cancel_summary(index),
                UiPaneActionKind::StopTranscription => rec.invoke_stop_transcription(index),
                UiPaneActionKind::TranscribeThenNotes => rec.invoke_transcribe_then_notes(index),
                UiPaneActionKind::OpenTranscription => {
                    if let Some(ui) = app_weak.upgrade() {
                        ui.invoke_open_transcription_window();
                    }
                }
                UiPaneActionKind::OpenNotes => {
                    if let Some(ui) = app_weak.upgrade() {
                        ui.invoke_open_minutes_window();
                    }
                }
                // 途中結果を開く（#164）。ディスクには何も起こさず、伏せてある一覧を出すだけ
                // （畳む契機は `fold_partial_transcript` の doc）。
                UiPaneActionKind::ShowPartialTranscript => rec.set_show_partial_transcript(true),
            }
        });
    }

    // 確認モーダルの Delete: 選択中セッションをディレクトリごと OS のゴミ箱へ移動し、
    // 一覧・メモリの両方から除去する（完全削除への自動フォールバックはしない）。
    // 失敗はログのみでアプリ・一覧を壊さない（`docs/rules/error-handling.md`）。
    {
        let sessions = Rc::clone(&sessions);
        let sessions_model = Rc::clone(&sessions_model);
        let all_sessions = Rc::clone(&all_sessions);
        let search_generation = Rc::clone(&search_generation);
        let search_not_downloaded = Rc::clone(&search_not_downloaded);
        let scan_state = Rc::clone(&scan_state);
        let app_state = Rc::clone(&app_state);
        let runner = runner.clone();
        let player = Rc::clone(&player);
        let transcriber = transcriber.clone();
        let summarizer = summarizer.clone();
        let rec_weak = library_ui.as_weak();
        library_ui.on_delete_session(move |index| {
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
                .map(|s| (s.dir.clone(), recordings::playback_path(s)))
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
            // `refresh_detail_panes` がゲートを閉じるので、この窓が開かない。
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
                //
                // **ここは同期の `load` を使う**（#152 の非同期経路に載せていない）。削除の失敗は
                // まれで、そのときだけ数秒待たせるほうが、失敗処理を非同期にして順序を増やすより
                // 読みやすい。長い録音では体感できる待ちになる、という限界は承知のうえ。
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
            // **絞り込む前の一覧からも消す**（#161）。ここを忘れると、検索して解除したときに
            // ゴミ箱へ移した録音が戻ってくる（`all_sessions` は解除で戻す元）。添字は絞り込みで
            // 食い違うので、ディレクトリで引く。
            all_sessions
                .borrow_mut()
                .retain(|session| session.dir != dir);
            // **走っている検索も降ろす**（#161）。結果は削除前のスナップショットなので、
            // 届いた瞬間に消したはずの録音が一覧へ戻る。捨てる経路はここも含めて全部が
            // `advance_search_generation` を通る。
            advance_search_generation(&search_generation);
            // 削除で**隣接行の見出しと合計が変わる**。見出しは直前の行との比較で決まるので、
            // その日の先頭を消すと繰り上がった行が見出しを引き継ぐ必要がある。
            {
                let sessions = sessions.borrow();
                let all = all_sessions.borrow();
                apply_list_counts(
                    &rec,
                    ListCounts {
                        shown: sessions.len(),
                        total: all.len(),
                        // **消したぶんは落として数え直す**（#182）。絞り込みは解除されない
                        // ので、読めなかった話も消さずに残す。
                        not_downloaded: not_downloaded_count(&search_not_downloaded.borrow(), &all),
                    },
                    &scan_state,
                );
                sessions_model.set_heading(
                    i,
                    session_group_heading(&sessions, i, chrono::Local::now().naive_local()),
                );
            }
            // 進行状況マップに残ったエントリを掃除する（削除済みセッションの記録を残さない）。
            transcriber.forget(&dir);
            summarizer.forget(&dir);
            // **core にも伝える**（#188）。`forget` の直後に流さないと、次の tick の差分が
            // 「走っていたジョブが消えた」と読んで、ゴミ箱へ移した録音を読み直そうとする。
            let effects = {
                let mut state = app_state.borrow_mut();
                shoki_core::update(
                    &mut state,
                    shoki_core::Msg::Event(shoki_core::Event::Deleted { dir }),
                )
            };
            run_effects(&runner, effects, None);
            clear_library_selection(&rec, &runner);
        });
    }

    // 機能ごとの設定ウィンドウ（#141）。設定画面の「扉」から開く。設定・Library と同じく
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
            LibraryHandles {
                ui: library_ui.as_weak(),
                player: Rc::clone(&player),
                load_receiver,
                scan_receiver,
                scan_sender: scan_sender.clone(),
                scan_generation: Rc::clone(&scan_generation),
                scan_state: Rc::clone(&scan_state),
                state: Rc::clone(&app_state),
                runner: runner.clone(),
                sessions: Rc::clone(&sessions),
                all_sessions: Rc::clone(&all_sessions),
                search_receiver,
                search_generation: Rc::clone(&search_generation),
                search_not_downloaded: Rc::clone(&search_not_downloaded),
                sessions_model: Rc::clone(&sessions_model),
                transcript_segments: Rc::clone(&transcript_segments),
                transcriber: transcriber.clone(),
                summarizer: summarizer.clone(),
                config: Rc::clone(&config),
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

/// Library ウィンドウの操作・再生に必要なハンドル一式。`build_menu_event_handler` の引数を
/// 増やしすぎないためにまとめる。
struct LibraryHandles {
    ui: slint::Weak<LibraryWindow>,
    player: Rc<RefCell<Option<player::AudioPlayer>>>,
    /// 選んだ録音の読み込み結果の受け口（#152）。**tick が拾って反映する**——読み込みスレッドは
    /// UI スレッド専有のものに触れないので、送るだけにしてある。
    load_receiver: std::sync::mpsc::Receiver<LoadedSession>,
    /// **一覧に出ている**セッション。行と 1 対 1 で、添字が操作対象の解決に使われる
    /// （絞り込むとここも縮む。`docs/rules/slint.md`）。
    sessions: Rc<RefCell<Vec<recordings::RecordingSession>>>,
    /// 走査で見つかった**全部**のセッション（#161）。検索を解除したときに戻す元で、
    /// 絞り込みの対象でもある。
    all_sessions: Rc<RefCell<Vec<recordings::RecordingSession>>>,
    /// 一覧の走査結果の受け口（#181。`load_receiver` と同じ流儀）。
    scan_receiver: std::sync::mpsc::Receiver<ScannedSessions>,
    /// 走査を投げるための送り口（使うのは `open_library_window` の 1 箇所）。
    scan_sender: std::sync::mpsc::Sender<ScannedSessions>,
    /// いま出している走査の世代。届いた結果がこれと違えば**捨てる**。
    scan_generation: Rc<Cell<u64>>,
    /// 走査がいまどうなっているか（#181。`ScanState` の doc）。空表示の文をここから決める。
    scan_state: Rc<Cell<ScanState>>,
    /// **文字起こしの状態はここが正**（#188）。変える口は `shoki_core::update` だけ。
    state: Rc<RefCell<shoki_core::AppState>>,
    /// `Effect` を実行する口（`EffectRunner` の doc）。
    runner: EffectRunner,
    /// 検索結果の受け口。本文を読むので背景スレッドで絞り、結果だけ送る（`#152` と同じ流儀）。
    search_receiver: std::sync::mpsc::Receiver<SearchResult>,
    /// いま出している検索の世代。届いた結果がこれと違えば捨てる（打ち込むたびに投げるので、
    /// 古い結果が新しい入力を上書きしないように）。
    search_generation: Rc<Cell<u64>>,
    /// 最後の検索で本文を読めなかった録音（#182。理由は `not_downloaded_count`）。
    search_not_downloaded: Rc<RefCell<Vec<std::path::PathBuf>>>,
    sessions_model: Rc<SessionRows>,
    transcript_segments: Rc<RefCell<LoadedTranscript>>,
    transcriber: transcribe::TranscribeWorker,
    /// 詳細ペインの要約状態を tick で追従させるために読む（#81）。
    summarizer: summarize::SummarizeWorker,
    /// 読む領域の空表示が「なぜ無いのか」を言うために読む（#154。自動が OFF なのか、
    /// ON だがまだ回っていないのかで文が変わる）。
    config: Rc<RefCell<Config>>,
}

/// 機能ウィンドウを tick で追従させるために必要なハンドル一式（`LibraryHandles` と
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
    recordings: LibraryHandles,
    models: ModelsHandles,
    tray: &Tray,
    config: Rc<RefCell<Config>>,
    postprocessor: mixdown::PostProcessWorker,
    #[cfg(target_os = "macos")] app_monitor: app_audio_monitor::AppAudioMonitor,
) -> impl FnMut() + 'static {
    // Library ウィンドウ・再生・一覧のハンドルは LibraryHandles にまとめたまま使う
    // （引数の氾濫を避ける。open_library_window にも構造体ごと渡す）。
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
    // Library ウィンドウの初回ジオメトリを確定させたか。
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
                open_library_window(
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

        // Library ウィンドウが開いている間だけ、再生の経過時間・進捗・再生状態を反映する
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
                &recordings.transcript_segments.borrow().transcript.segments,
                position.as_secs_f64(),
            )
            .and_then(|index| i32::try_from(index).ok())
            .unwrap_or(-1);
            rec.set_current_segment(current);
        }

        // 走査の結果を一覧へ入れる（#181）。**閉じていても受け取って捨てる**（溜めたままに
        // しない。読み込み・検索と同じ）。閉じるときに世代を進めているので、閉じたあとに届いた
        // 結果は `apply_scanned_sessions` が世代で落とす。
        while let Ok(scanned) = recordings.scan_receiver.try_recv() {
            let Some(rec) = recordings.ui.upgrade() else {
                continue;
            };
            apply_scanned_sessions(
                &rec,
                &recordings.sessions_model,
                SessionLists {
                    all: &recordings.all_sessions,
                    shown: &recordings.sessions,
                },
                &recordings.scan_generation,
                &recordings.scan_state,
                &recordings.state,
                scanned,
            );
        }

        // 絞り込みが終わった検索結果を一覧へ入れる（#161）。読み込みと同じく、閉じていても
        // 受け取って捨てる（溜めたままにしない）。
        while let Ok(result) = recordings.search_receiver.try_recv() {
            if result.generation != recordings.search_generation.get() {
                // もっと打ち込まれている。古い絞り込みで一覧を書き換えない。
                continue;
            }
            let Some(rec) = recordings.ui.upgrade() else {
                continue;
            };
            let mut matched = {
                let all = recordings.all_sessions.borrow();
                apply_search_result(
                    &rec,
                    &recordings.search_not_downloaded,
                    &all,
                    &recordings.scan_state,
                    result,
                )
            };
            // 文字起こし・議事録の有無は**ワーカーの状態から埋め直す**。全件側から写すと、
            // それを埋めるのがこの tick の末尾なので 1 周ぶん古くなり、直後に組む行
            // （`session_rows` は現在の状態を見る）と食い違う。食い違うと行の差分が
            // 「変化なし」と判断し、以後どの tick でも直らない。
            for session in matched.iter_mut() {
                session.has_transcript |= recordings.transcriber.status_of(&session.dir)
                    == Some(transcribe::TranscribeStatus::Done);
                session.has_summary |= recordings.summarizer.status_of(&session.dir)
                    == Some(summarize::SummarizeStatus::Done);
            }
            recordings
                .sessions_model
                .replace_all(&matched, &recordings.state.borrow());
            reselect_after_list_change(&rec, &recordings.sessions, matched, &recordings.runner);
        }

        // 読み込みが終わった録音を表示へ入れる（#152）。**ウィンドウが閉じていても受け取る**——
        // 受け口に溜めたままにすると、次に開いたときに古い結果がまとめて流れ込む。
        //
        // **入れてよいかを決めるのは `update` 1 箇所**（#188）。ここでもう一度世代を見ると
        // 判定が 2 つになり、解除を挟んで世代が飛んだときに食い違う。
        while let Ok(loaded) = recordings.load_receiver.try_recv() {
            let effects = {
                let mut state = recordings.state.borrow_mut();
                shoki_core::update(
                    &mut state,
                    shoki_core::Msg::Event(shoki_core::Event::SessionLoaded {
                        dir: loaded.dir.clone(),
                        generation: loaded.generation,
                        has_readable_segments: !loaded.transcript.segments.is_empty(),
                        shortfall: loaded.transcript.shortfall,
                    }),
                )
            };
            run_effects(&recordings.runner, effects, Some(loaded));
        }

        // Library ウィンドウが開いている間だけ、文字起こし状態の変化を一覧・詳細ペインへ
        // 反映する（変化した行だけ set_row_data して無駄な再描画を避ける）。選択中セッションが
        // 文字起こし中→完了に変わったら、トランスクリプトを読み直して表示を差し替える。
        if let Some(rec) = recordings.ui.upgrade()
            && rec.window().is_visible()
        {
            let selected = usize::try_from(rec.get_selected_index()).ok();

            // **ジョブの様子を core へ流す**（#188）。ワーカーの状態マップと `AppState.jobs` を
            // 突き合わせ、**違うものだけ** `Event` にする。ジョブは通常 0〜2 件なので、
            // セッション数ではなくジョブ数に比例する（`progress_of` を足した #162 の意図を
            // 壊さない）。
            //
            // **読み込みの受け取りより後に置く**。先に置くと、`Effect::LoadSession` が世代を
            // 進めた直後に、その周回で届いていた正当な結果が落ちる。
            let job_effects = {
                let mut state = recordings.state.borrow_mut();
                job_changes(&recordings.transcriber, &state)
                    .into_iter()
                    .flat_map(|msg| shoki_core::update(&mut state, msg))
                    .collect::<Vec<_>>()
            };
            run_effects(&recordings.runner, job_effects, None);

            // **生成物の有無を書き戻す**（#161 / #188）。遷移ではなく**毎 tick の全件スキャン**
            // にしてある——遷移で拾うと、絞り込みで隠れている録音の完了を取りこぼし、検索を
            // 解除したときに済んでいるはずの文字起こしが「無い」に戻る。ロック 1 回のマップ
            // 引きだけで、ディスクは読まない。
            sweep_finished_jobs(&recordings);

            {
                let sessions_ref = recordings.sessions.borrow();
                let state = recordings.state.borrow();
                for (i, session) in sessions_ref.iter().enumerate() {
                    // 変わっていない行は `view_row` の確保も払わない（`SessionRows::refresh`）。
                    recordings.sessions_model.refresh(i, session, &state);
                }

                // 選択中セッションの要約状態も追従させる。一覧行には要約のインジケータが無い
                // （#81 のスコープ外）ので、行の差分更新ではなく詳細ペインの現在値と比べる。
                if let Some(session) = selected.and_then(|i| sessions_ref.get(i)) {
                    // **読む領域は毎回組み直す**（#154）。進捗の割合と経過は状態が変わらない
                    // まま動くので、状態の差分だけで更新すると数字が止まって見える
                    // （`docs/rules/slint.md` の「差分更新は表示に使う値ぜんぶで比べる」）。
                    // 議事録が完成したら、この中で読み直す（文字起こし側は `update` が
                    // `Effect` で返す）。
                    refresh_detail_panes(
                        &rec,
                        &recordings.runner,
                        &recordings.summarizer,
                        session,
                        &recordings.config,
                    );
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

/// その読み込みで再生を差し替えるか（`PlaybackLoad` の判断を 1 か所に置く）。
///
/// **選択が変わったときだけ true**。中身が変わって読み直しただけのときに差し替えると、
/// `AudioPlayer::adopt` が前の対象を手放すので**再生中の音が止まって先頭へ巻き戻る**——
/// 文字起こしの完成は再生しながら待つ場面なので、そこで止まるのは痛い。あわせて、変わって
/// いない音声を開き直す重い走査も避けられる。
fn load_replaces_playback(selection_changed: bool) -> bool {
    selection_changed
}

/// 選択の世代を 1 つ進めて、**走っている読み込みへ知らせる**。進めた世代を返す。
///
/// 世代は 2 つの役目を持つ。(1) 遅れて届いた結果を捨てる（判定は `shoki_core::AppState`）。(2) まだ重い処理に
/// 入っていない読み込みを**降ろす**——連打したぶんだけ数百 MB を読み切るのは、いま見たい録音の
/// 読み込みを自分で遅くする。
///
/// **世代を進める操作はすべてここを通す**（選ぶ・解除する・ウィンドウを閉じる・中身が変わって
/// 読み直す）。直接 `set` すると (2) が効かず、降りられたはずの読み込みが走り続ける。
fn advance_load_generation(generation: &Cell<u64>) -> u64 {
    let next = generation.get().wrapping_add(1);
    generation.set(next);
    LOAD_WATCHERS.with(|watchers| {
        let mut watchers = watchers.borrow_mut();
        watchers.retain(|w| Arc::strong_count(w) > 1);
        for w in watchers.iter() {
            w.store(next, Ordering::Relaxed);
        }
    });
    next
}

/// 走査の世代を 1 つ進め、走っている走査に「降りろ」と伝える（#181）。
///
/// **番号を進めるのと伝えるのを 1 つにしてある**（`advance_load_generation` と同じ理由）——
/// 別々にすると、進めたのに伝え忘れる書き方が残る。
fn advance_scan_generation(generation: &Cell<u64>) -> u64 {
    let next = generation.get().wrapping_add(1);
    generation.set(next);
    SCAN_WATCHERS.with(|watchers| {
        let mut watchers = watchers.borrow_mut();
        watchers.retain(|w| Arc::strong_count(w) > 1);
        for w in watchers.iter() {
            w.store(next, Ordering::Relaxed);
        }
    });
    next
}

/// 届いた走査結果を一覧へ入れる（#181）。
///
/// **繋ぎを丸ごと呼べる形にしてある**（`docs/rules/testing.md`）。ここが持つ判断は 4 つ
/// ——古い世代を捨てる・走れたかどうかを状態に残す・全件と表示中の両方へ入れる・絞り込みを
/// やり直す——で、どれも tick の中に書くとウィンドウ無しでは検査できない。
///
/// **`all_sessions` と `sessions` の両方へ入れる**（#161）。片方だけ更新する経路を残すと、
/// 検索を解除したときに消えたはずの録音が戻る。
///
/// **検索語が残っていたら絞り直す**。走査中も検索欄は生きているので、走っている間に打たれる
/// ことがある——そのときの絞り込みは空の `all_sessions` に対して行われて 0 件になっており、
/// ここで全件を入れると「検索語が残ったまま全件が並ぶ」画面になる。本番の入口
/// （`on_search`）をそのまま呼び直して、新しい全件に対して絞り直す。
///
/// **届くまでは絞り込まれていない一覧が見える**。投げ直した検索は本文（文字起こしと議事録）を
/// 全件ぶん読み直す（`search_sessions`）。検索の受け口をドレインするのは走査のすぐ後なので、
/// 読み切れれば同じ周回、まず間に合わないので次以降の周回になる（保存先が遅ければさらに後）。
/// スレッドを立てられなければ結果は来ないので、そのときは絞り込みが解けたまま残る——検索欄の
/// 語は残るので、打ち直せば絞り直せる。
fn apply_scanned_sessions(
    rec: &LibraryWindow,
    model: &SessionRows,
    lists: SessionLists,
    generation: &Cell<u64>,
    scan_state: &Cell<ScanState>,
    state: &RefCell<shoki_core::AppState>,
    scanned: ScannedSessions,
) {
    if scanned.generation != generation.get() {
        // 閉じた・閉じて開き直した。古い走査で一覧を書き換えない。
        return;
    }
    let sessions = match scanned.outcome {
        ScanOutcome::Scanned(sessions) => {
            scan_state.set(ScanState::Settled);
            sessions
        }
        ScanOutcome::CouldNotStart => {
            // **「録音が無い」とは言わない**。走らなかったので 1 件も見ていない。
            scan_state.set(ScanState::CouldNotStart);
            Vec::new()
        }
    };
    model.replace_all(&sessions, &state.borrow());
    // **件数も空表示もここを通す**（`docs/rules/slint.md` の「表示値の導出は 1 つの関数に
    // 集める」）。読めなかった件数が 0 なのは、絞り込みの結果ではないから——絞り込みが
    // 残っていれば、下で投げ直した検索の結果が届いたときに入る。
    apply_list_counts(
        rec,
        ListCounts {
            shown: sessions.len(),
            total: sessions.len(),
            not_downloaded: 0,
        },
        scan_state,
    );
    *lists.all.borrow_mut() = sessions.clone();
    *lists.shown.borrow_mut() = sessions;
    // **借用を手放してから投げる**——`on_search` が `all_sessions` を借りる。
    let needle = rec.get_search_text();
    if !needle.is_empty() {
        rec.invoke_search(needle);
    }
}

/// 走査結果の反映先の 2 つの一覧（#181）。
///
/// **まとめて渡す**——どちらも `RefCell<Vec<RecordingSession>>` なので、引数で並べると渡し
/// 違えても通る（`docs/rules/coding-conventions.md` の「同型の引数を並べた関数に切り出さない」）。
/// 名前付きのフィールドなら位置で取り違えられない。
struct SessionLists<'a> {
    /// 走査で見つかった**全部**（検索を解除したときに戻す元。#161）。
    all: &'a RefCell<Vec<recordings::RecordingSession>>,
    /// いま**一覧に出ている**ぶん（絞り込むと縮む）。
    shown: &'a RefCell<Vec<recordings::RecordingSession>>,
}

/// 選んだ録音の重い読み込みを別スレッドで始める。
///
/// **`set_segments` を書く経路をここ 1 本に絞る**ための入口（#152）。読み込み中に文字起こしや
/// 議事録が完成することがあり、そのとき tick が直接表示を差し替えると、少し前に始まった読み込みの
/// **古いスナップショット**（まだ何も無かった頃の内容）があとから届いて上書きし、
/// **完成した文字起こしが消える**。tick も表示を書かずにこれを呼び、世代を進めて読み直す。
fn spawn_session_load(
    session: &recordings::RecordingSession,
    generation_id: u64,
    sender: &std::sync::mpsc::Sender<LoadedSession>,
    // 音声も読み直すか。**中身だけ変わった読み直しでは false**（理由は `PlaybackLoad`）。
    load_playback: bool,
) {
    let dir = session.dir.clone();
    // 揃っているかは「在る音源ごとに JSON があるか」で決まる（#175）ので、音源の並びも渡す。
    let speakers = session.speakers();
    let playback_path = recordings::playback_path(session);
    // スレッドへ渡す口と、失敗したときにこのスレッドから送る口を分けて持つ。
    let thread_sender = sender.clone();
    let fallback_sender = sender.clone();
    // 読み込みスレッドからも見える世代（`Rc` は渡せないので値を写す）。**重い処理に入る前に
    // 確かめて、すでに古ければ何も読まない**——連打したぶんだけ数百 MB を読み切るのは、
    // いま見たい録音の読み込みを自分で遅くする。
    let live = Arc::new(AtomicU64::new(generation_id));
    // 世代が進んだことをスレッドへ伝える手を登録する（書き込むのは `advance_load_generation`）。
    LOAD_WATCHERS.with(|watchers| {
        let mut watchers = watchers.borrow_mut();
        // 終わったスレッドの手は落とす（`Vec` だけが持っている＝相手がいない）。
        watchers.retain(|w| Arc::strong_count(w) > 1);
        watchers.push(Arc::clone(&live));
    });

    let spawned = std::thread::Builder::new()
        .name("session-load".to_owned())
        .spawn(move || {
            // **重い処理の前に降りられるか見る**（軽い読み込みは先に済ませてしまう）。
            // **取り寄せてよい**（#182）——録音を選んだのはユーザーなので、退避されていれば
            // 落としてでも読む。頼まれていない読み取り（一覧の走査・検索）だけを止める。
            //
            // **打鍵のたびにここへ再入する**（絞り込み後も選択が残っていれば
            // `reselect_after_list_change` が読み直しを起こす）。読むのは選んだ 1 件ぶんの
            // JSON と `summary.md` だけで、2 回目以降は実体が落ちているので取り寄せは
            // 走らない。全件を舐める検索とは桁が違うので、ここは止めない。
            let transcript =
                transcript::load_transcript(&dir, &speakers, dataless::Fetch::allowed());
            let summary = summarize::load_summary(&dir, dataless::Fetch::allowed()).text;
            let summary_written = summary.is_some().then(|| {
                std::fs::metadata(dir.join(summarize::SUMMARY_FILENAME))
                    .and_then(|meta| meta.modified())
                    .ok()
            });
            let playback = if !load_playback {
                // 音声は変わっていない（中身だけ読み直した）。鳴っているものをそのまま使う。
                PlaybackLoad::Keep
            } else if live.load(Ordering::Relaxed) != generation_id {
                // すでに別の録音が選ばれている。数百 MB を読む意味はない。
                PlaybackLoad::Replace(None)
            } else {
                PlaybackLoad::Replace(playback_path.and_then(|path| {
                    match player::AudioPlayer::prepare(&path) {
                        Ok(prepared) => Some(prepared),
                        Err(err) => {
                            eprintln!("Failed to load the recording for playback: {err}");
                            None
                        }
                    }
                }))
            };
            // **結果は送るだけ**。Slint のプロパティも `Rc` の共有状態も UI スレッド専有なので、
            // ここからは触れない（受け取って反映するのは tick）。
            let loaded = LoadedSession {
                generation: generation_id,
                dir: dir.clone(),
                transcript,
                summary,
                summary_written: summary_written.flatten(),
                playback,
            };
            if thread_sender.send(loaded).is_err() {
                // 受け手が畳まれている（アプリ終了中）。表示は捨ててよい。
                eprintln!("Skipping the loaded recording because the app is shutting down");
            }
        });
    if let Err(err) = spawned {
        // スレッドを作れない（資源の枯渇）。**`loading` を残さない**ため、空の結果を自分で送る。
        eprintln!("Loading the recording on this thread because spawning failed: {err}");
        let _ = fallback_sender.send(LoadedSession {
            generation: generation_id,
            dir: session.dir.clone(),
            // **食い違っているとは言わない**。読めなかっただけで、途中結果だと決めつけると
            // 資源枯渇のたびに全セッションが伏せられる（#175）。
            transcript: transcript::Transcript {
                segments: Vec::new(),
                shortfall: None,
            },
            summary: None,
            summary_written: None,
            playback: PlaybackLoad::Replace(None),
        });
    }
}

thread_local! {
    /// いま走っている読み込みへ「世代が進んだ」ことを伝える手（`spawn_session_load` の doc）。
    /// UI スレッド専有なので `thread_local` で足りる。
    static LOAD_WATCHERS: RefCell<Vec<Arc<AtomicU64>>> = const { RefCell::new(Vec::new()) };
    /// 走査版（#181。`spawn_session_scan` の doc）。
    static SCAN_WATCHERS: RefCell<Vec<Arc<AtomicU64>>> = const { RefCell::new(Vec::new()) };
    /// 検索版（`spawn_search` の doc）。
    static SEARCH_WATCHERS: RefCell<Vec<Arc<AtomicU64>>> = const { RefCell::new(Vec::new()) };
    // **3 つに分けてあるのは、値を分けたいから**——1 つにまとめると、選択を切り替えただけで
    // 走っている検索や走査まで降りる。同型のコードが 3 本並ぶが、統合するなら「世代 + 配り先」を
    // 1 つの型にしてから（`advance_load_generation` / `advance_scan_generation` /
    // `advance_search_generation` を薄い包みにする）。
}

/// 検索の世代を進め、走っている検索へ「もう要らない」を伝える。**検索を捨てる経路は必ず
/// ここを通す**（世代だけ進めても、走っているスレッドは全件を読み切ってしまう）。
fn advance_search_generation(generation: &Cell<u64>) -> u64 {
    let next = generation.get().wrapping_add(1);
    generation.set(next);
    SEARCH_WATCHERS.with(|watchers| {
        let mut watchers = watchers.borrow_mut();
        watchers.retain(|w| Arc::strong_count(w) > 1);
        for watcher in watchers.iter() {
            watcher.store(next, Ordering::Relaxed);
        }
    });
    next
}

/// 検索を解除した状態へ畳む（表示だけ。一覧の作り直しは呼び出し側）。
///
/// **開く・閉じる・解除の 3 経路がここを通る**。どれかが世代を進め忘れると、走っていた検索の
/// 結果が後から届いて、まっさらなはずの一覧を黙って絞り込む（検索欄は空なので原因が出ない）。
///
/// **検索にまつわる値はここで全部落とす**（#182 で「読めなかった録音」を足した）。1 つでも
/// 外に置くと、解除の経路を足した人が確実に落とす。
fn reset_search(
    rec: &LibraryWindow,
    generation: &Cell<u64>,
    not_downloaded_dirs: &RefCell<Vec<std::path::PathBuf>>,
) {
    advance_search_generation(generation);
    rec.set_search_text(slint::SharedString::new());
    rec.set_search_summary(slint::SharedString::new());
    not_downloaded_dirs.borrow_mut().clear();
}

/// いま画面に出しているセグメント（#175 / #188）。
///
/// **再生位置のハイライトに要る**（`transcript::current_index`）。「最後まで読み切れているか」の
/// 判断は core が持つようになった（`shoki_core::view_detail`）ので、ここが答えるのは
/// 「いま何行出しているか」だけ。
///
/// **どの録音のものかは持たない**。core の `loaded` が対象と世代を照合しているので、ここへ
/// 入るのは受け入れられた結果だけ——照合をもう 1 つ持つと、判定が 2 つになって食い違う。
struct LoadedTranscript {
    transcript: transcript::Transcript,
}

impl LoadedTranscript {
    /// まだ何も出していない状態。
    fn unknown() -> Self {
        Self {
            transcript: transcript::Transcript {
                segments: Vec::new(),
                shortfall: None,
            },
        }
    }
}

/// 一覧の走査結果（#181。別スレッドで作り、UI スレッドへ渡す）。
///
/// **`LoadedSession` と同じ流儀**（#152）——走査スレッドは UI スレッド専有のものに触れないので、
/// 送るだけにして、拾って反映するのは tick の仕事。
struct ScannedSessions {
    /// どの走査に対する結果か。**受け取る側が世代を確かめて、古い結果を捨てる**（閉じて開き
    /// 直したときに、前の走査が後から届いて一覧を書き換えないように）。
    generation: u64,
    outcome: ScanOutcome,
}

/// 走査が**どう終わったか**（#181）。
///
/// **走れなかったことを結果として運ぶ**——空の一覧として送ると、受け取る側は区別できず
/// 「録音が無い」と言い切ってしまう。1 件も見ていないことと、見た結果 0 件だったことは違う。
enum ScanOutcome {
    /// 走り切った（0 件のこともある）。
    Scanned(Vec<recordings::RecordingSession>),
    /// 走査スレッドを立てられなかった（資源枯渇）。**1 件も見ていない**。
    CouldNotStart,
}

/// 一覧の走査を別スレッドで始める（#181）。
///
/// **ウィンドウは先に出す**。保存先が遅いボリューム（ネットワーク共有・外付け・スピンドル）だと、
/// UI スレッドで走査を待つとトレイから押しても窓が出ない。走査そのものの計算は軽いが、支配的
/// なのは保存先の I/O（`read_dir` と 1 セッションずつの `stat`）なので、非同期にするしかない。
///
/// **世代を持たせる**のは `spawn_session_load` と同じ理由（`advance_scan_generation`）。
///
/// **降りられるのは 2 点だけ**——`list_sessions` の前と後。`list_sessions` の途中では降りない
/// ので、走り始めた走査は最後まで保存先を舐める。閉じて開き直しを繰り返せば、そのぶん走査は
/// 並走する（防いでいるのは「古い結果で一覧を書き換えること」と「降りたのに消すこと」であって、
/// 並走そのものではない）。
///
/// 一時ファイルの回収（`spawn_session_part_sweep`）も**走査スレッドの中で呼ぶ**——走査結果が要る
/// うえに表示には使わない副作用なので、UI スレッドへ戻す必要が無い。**降りていないときだけ**
/// 呼ぶ（下の 2 つ目の世代チェック）。
fn spawn_session_scan(
    recording_dir: std::path::PathBuf,
    generation_id: u64,
    sender: &std::sync::mpsc::Sender<ScannedSessions>,
) {
    let thread_sender = sender.clone();
    let live = Arc::new(AtomicU64::new(generation_id));
    // 世代が進んだことを走査スレッドへ伝える手を登録する（書き込むのは `advance_scan_generation`）。
    SCAN_WATCHERS.with(|watchers| {
        let mut watchers = watchers.borrow_mut();
        watchers.retain(|w| Arc::strong_count(w) > 1);
        watchers.push(Arc::clone(&live));
    });

    let spawned = std::thread::Builder::new()
        .name("session-scan".to_owned())
        .spawn(move || {
            // **重い処理の前に降りられるか見る**。閉じて開き直したぶんだけ走査が積み上がると、
            // いま見たい一覧を自分で遅くする（`spawn_session_load` と同じ）。
            if live.load(Ordering::Relaxed) != generation_id {
                return;
            }
            let sessions = recordings::list_sessions(&recording_dir);
            // **消す前にもう一度見る**。降りた走査の結果は一覧に出ないので、掃除が拠って立つ
            // 「一覧に出たセッションの直下だけ」という絞りが成り立たなくなる
            // （`docs/rules/security.md`）。とくに閉じて開き直した直後は、ユーザーがもう見て
            // いない保存先に対して削除が走る。掃除は次に開いたときにまた走るので、降りて困らない。
            if live.load(Ordering::Relaxed) != generation_id {
                return;
            }
            // 一覧に出たセッションに取り残された一時ファイルを回収する（強制終了などで残った
            // もの。範囲と時期の判断は `recordings::spawn_session_part_sweep` の doc）。
            // 表示には使わない副作用なので、完了は待たない。
            recordings::spawn_session_part_sweep(&sessions, SystemTime::now());
            if thread_sender
                .send(ScannedSessions {
                    generation: generation_id,
                    outcome: ScanOutcome::Scanned(sessions),
                })
                .is_err()
            {
                eprintln!("Skipping the scanned recordings because the app is shutting down");
            }
        });
    if let Err(err) = spawned {
        // スレッドを立てられないのは資源枯渇（`docs/rules/error-handling.md`）。**走れなかった
        // ことを送る**——送らないと一覧が `Looking for recordings…` のまま二度と埋まらないし、
        // 空の結果を送ると「録音が無い」と言い切ってしまう（1 件も見ていないのに）。
        eprintln!("Not looking for recordings because the scan thread could not start: {err}");
        if sender
            .send(ScannedSessions {
                generation: generation_id,
                outcome: ScanOutcome::CouldNotStart,
            })
            .is_err()
        {
            eprintln!("Skipping the scan failure because the app is shutting down");
        }
    }
}

/// 選んだ録音の**重い読み込みの結果**（別スレッドで作り、UI スレッドへ渡す）。
///
/// Slint の型を持たせないのは、生成をイベントループの外で行うため。UI へ入れる形への変換は
/// `apply_loaded_session` が行う。
struct LoadedSession {
    /// どの選択に対する結果か。**受け取る側が世代を確かめて、古い結果を捨てる**。
    generation: u64,
    /// どの録音を読んだか。**`Event::SessionLoaded` に載せて core へ渡す**（#188。対象と世代の
    /// 照合は `update` が 1 箇所でやる）。
    dir: std::path::PathBuf,
    /// 読んだ文字起こし（**セグメントと「揃っているか」を 1 つの値で**。#175）。
    transcript: transcript::Transcript,
    summary: Option<String>,
    /// 議事録を書いた時刻（`summary.md` の更新時刻）。**読み込みスレッドで取る**——UI スレッドの
    /// tick から毎回 stat すると、保存先がネットワーク越しのときに引っかかる。無ければ出典を
    /// 出さない。
    summary_written: Option<SystemTime>,
    /// 再生の準備。**中身だけ読み直したときは触らない**（`Keep`）。
    playback: PlaybackLoad,
}

/// 読み込み結果のうち、再生をどう扱うか。
///
/// **音声を差し替えてよいのは選択が変わったときだけ**。中身（文字起こし・議事録）が変わって
/// 読み直しただけのときに差し替えると、`adopt` が前の対象を手放すので**再生中の音が止まって
/// 先頭へ巻き戻る**。文字起こしの完成は再生しながら待つ場面なので、そこで止まるのは痛い。
enum PlaybackLoad {
    /// いま鳴らしているものをそのまま使う（音声ファイルは変わっていない）。
    Keep,
    /// 差し替える（`None` は対象が無い・開けなかった）。
    Replace(Option<player::PreparedSource>),
}

/// 読み込みの結果を表示へ入れる（**イベントループ上でだけ呼ぶ**）。
///
/// ここに来た時点で世代の確認は済んでいる。やることは「読み込み中」を解いて、届いた中身を
/// 一度に入れることだけ——段階的に入れると、文字起こしだけ出て再生がまだ、という中途半端な
/// 表示が挟まる。
fn apply_loaded_session(
    rec: &LibraryWindow,
    player: &Rc<RefCell<Option<player::AudioPlayer>>>,
    segments_cell: &RefCell<LoadedTranscript>,
    loaded: LoadedSession,
) {
    let LoadedSession {
        transcript,
        summary,
        summary_written,
        playback,
        ..
    } = loaded;

    rec.set_segments(Rc::new(slint::VecModel::from(transcript_rows(&transcript.segments))).into());
    // **選択が変わったときだけハイライトを戻す**。読み込み中に付いた行番号を引き継ぐと、
    // 差し替わった別の内容の同じ行番号が光る。中身だけ読み直したときは、次の tick が再生位置
    // から付け直すので触らない（触ると再生中の印が一瞬消える）。
    if matches!(playback, PlaybackLoad::Replace(_)) {
        rec.set_current_segment(-1);
    }
    // 再生位置のハイライトが読む（`transcript::current_index`）。
    *segments_cell.borrow_mut() = LoadedTranscript { transcript };
    let summary_rows = summary.map(|text| summary_rows(&text)).unwrap_or_default();
    rec.set_summary_rows(Rc::new(slint::VecModel::from(summary_rows)).into());
    // 出典は**議事録の中身と一緒に**入れる。行を書くのはここだけなので、両者がずれない
    // （tick から組み直すと、書き換わったときにしか変わらない値のために毎回 stat することになる）。
    rec.set_detail_summary_footer(summary_footer_text(summary_written).into());

    match playback {
        // 中身だけ読み直した。**再生には触れない**（触ると鳴っている音が止まる。`PlaybackLoad`）。
        PlaybackLoad::Keep => {}
        PlaybackLoad::Replace(prepared) => {
            let duration = prepared.as_ref().and_then(player::PreparedSource::duration);
            // **開けたかどうかが「再生できる」の答え**。両音源で mix.mp3 が未生成のセッションや、
            // ファイルを開けなかったセッションはここで `None` になる（選択時にその場でミックス
            // して UI を固めることはしない）。
            let playable = prepared.is_some();
            if let Some(prepared) = prepared
                && let Some(p) = player.borrow_mut().as_mut()
            {
                p.adopt(prepared);
            }
            rec.set_playable(playable);
            apply_playback_position(rec, Duration::ZERO, duration);
            // 全体長が分からないと比率→秒の換算ができないため、その場合はシークバーを表示専用に
            // 縮退させる。
            rec.set_seekable(playable && duration.is_some());
        }
    }
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

/// Library ウィンドウの選択・再生表示を未選択状態へ初期化する
/// （ウィンドウを開いたとき・セッション削除後に共用する）。
///
/// 表示中だった文字起こし・議事録も手放す: どちらも発話由来の機微データで、詳細ペインが
/// 隠れている間もモデルとして持ち続ける理由が無い（削除したセッションの内容が残らないように。
/// `docs/rules/security.md`）。
/// 選択を解除して、詳細ペインを未選択の状態へ畳む。
///
/// **世代も進める**（#152）。進めないと、解除の直前に始まった読み込みがあとから届いて、選択が
/// 無いのに中身だけ入る（削除した録音の文字起こしが残る、という形で出る）。
fn clear_library_selection(rec: &LibraryWindow, runner: &EffectRunner) {
    advance_load_generation(&runner.load_generation);
    // **core にも伝える**（#188）。伝えないと `selected` が古いままになり、削除した録音を
    // 読み直そうとする／表示中の中身（発話）が `AppState` に残る（`docs/rules/security.md`）。
    let effects = {
        let mut state = runner.state.borrow_mut();
        shoki_core::update(
            &mut state,
            shoki_core::Msg::Command(shoki_core::Command::Select(None)),
        )
    };
    run_effects(runner, effects, None);
    rec.set_selected_index(-1);
    rec.set_has_selection(false);
    rec.set_playing(false);
    rec.set_seekable(false);
    rec.set_has_transcript(false);
    // 状態も未実施へ畳む（次の選択で必ず上書きされるが、`detail-files-in-use` /
    // `detail-jobs-pending` の入力なので前の
    // セッションの「実行中」を持ち越さない）。文字起こし・要約で対称にする。
    // ここは「選択が無い」ときの畳み方なので、設定の状態は関係しない（次の選択で必ず
    // 上書きされる）。自動の有無は false で組む。
    apply_detail_transcript_status(rec, &blank_detail_view(), false);
    apply_detail_summary_status(rec, &SummaryPane::NotSummarized { auto_on: false }, false);
}

/// 背景スレッドで絞り込んだ結果（#161）。
struct SearchResult {
    /// どの検索に対する結果か。**受け取る側が世代を確かめて、古い結果を捨てる**。
    generation: u64,
    /// 一致したセッション（元の並び順のまま）。**件数は持たない**——絞り込んでいる間に
    /// 削除されることがあるので、合計は受け取った側が `all_sessions` から数える。
    matched: Vec<recordings::RecordingSession>,
    /// 実体が無くて本文を読めなかった録音（#182）。**ここも件数ではなく録音そのもの**——
    /// 理由は `not_downloaded_count` の doc（正はそこ）。
    not_downloaded_dirs: Vec<std::path::PathBuf>,
}

/// 検索 1 件ぶんの結果（#182）。**「当たらなかった」と「読めなかった」を分ける**——
/// 一緒にすると、退避された録音が黙って対象から外れて「検索に出てこない＝無い」と読める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchOutcome {
    /// 読めた本文に当たった。
    Matched,
    /// 読めた本文には当たらず、読めなかった本文も無かった。
    Missed,
    /// **読めた範囲では**当たらなかった。読めなかった本文に当たるかは分からない。
    NotDownloaded,
}

/// 読んだ本文から、この録音の結果を決める（#182）。
///
/// **読み取りを含まないので、退避されたファイルを用意できない環境でも「読めなかった」の
/// 扱いを検査できる**（`docs/rules/testing.md`。読み取りを差し替える継ぎ目は作らない——
/// 閉包の中に直接読み取りを書けてしまい、囲いを素通りできる）。
///
/// **当たったら `Matched`**。読めなかった本文が残っていても、当たった事実は変わらない
/// （当たった録音まで「読めなかった」に数えると、件数が一覧より多くなって意味を成さない）。
/// **当たった録音の欠落は数えない**という割り切りで、その代わり当たらなかった録音では
/// 読めなかったことを必ず言う。
fn outcome_of(
    summary: &summarize::Summary,
    transcript: &transcript::Segments,
    needle: &str,
) -> SearchOutcome {
    if summary_mentions(summary, needle)
        || transcript
            .segments
            .iter()
            .any(|segment| mentions(&segment.text, needle))
    {
        return SearchOutcome::Matched;
    }
    if summary.not_downloaded || transcript.not_downloaded {
        return SearchOutcome::NotDownloaded;
    }
    SearchOutcome::Missed
}

/// 議事録が検索語を含むか（#182）。**短絡と `outcome_of` で同じ判定を使う**ための 1 箇所——
/// 写しを置くと、片方だけ大小の扱いを変えた日に「当たったのに出てこない」が起きる。
fn summary_mentions(summary: &summarize::Summary, needle: &str) -> bool {
    summary
        .text
        .as_deref()
        .is_some_and(|text| mentions(text, needle))
}

/// 本文が検索語を含むか。**大小を無視する**（打ち込むときに気にさせない）。
fn mentions(text: &str, needle: &str) -> bool {
    text.to_lowercase().contains(needle)
}

/// 検索を 1 周走らせる（#182）。
///
/// **取り寄せを止めるのはここ 1 箇所**——囲いを剥がすと証を作れず、`Fetch::blocked` も
/// 作れないのでコンパイルが通らない（`docs/rules/testing.md`。`recordings::list_sessions` と
/// 同じ形）。
///
/// `fetch` を組むのもここだけにしてあるが、**それは読みやすさのためで、守りではない**——
/// `Fetch::allowed()` はどこからでも書けるので、読み取り 1 つだけを差し替える形は型でも
/// 警告でも止まらない。止めているのは `Fetch::allowed` 自身で、囲いの中で作られたものは
/// 証を持たないだけで扱いは `blocked` と同じになる（`dataless::Fetch` の doc）。
///
/// 対象は**文字起こしと議事録の本文**。日時や音源は目で追えるので入れない——入れると
/// `mic` のような語が全件に当たって絞り込みにならない。
///
/// 世代が進んでいたら**途中で降りる**（`None`）。読み切ってから捨てるのでは、打鍵のたびに
/// 全件を読む時間を自分で積む。
fn search_sessions(
    sessions: Vec<recordings::RecordingSession>,
    needle: &str,
    live: &AtomicU64,
    generation: u64,
) -> Option<SearchResult> {
    dataless::without_downloads(|downloads_off| {
        let fetch = dataless::Fetch::blocked(downloads_off);
        let mut judged = Vec::with_capacity(sessions.len());
        for session in sessions {
            // **1 件読むごとに降りられるか見る**。結果も送らない（送っても捨てられる）。
            if live.load(Ordering::Relaxed) != generation {
                return None;
            }
            let summary = summarize::load_summary(&session.dir, fetch);
            // **議事録に当たったら文字起こしは読まない**（#182）。当たった録音の欠落は
            // 数えないので（`outcome_of`）読んでも結果は変わらず、退避された保存先では
            // 1 件につきファイルプロバイダへの往復が 2 回増えるだけになる。
            let outcome = if summary_mentions(&summary, needle) {
                SearchOutcome::Matched
            } else {
                // **本文しか要らない**（揃っているかの判断は読む領域の仕事。#175）。
                let transcript = transcript::load_segments(&session.dir, fetch);
                outcome_of(&summary, &transcript, needle)
            };
            judged.push((session, outcome));
        }
        Some(collect_findings(generation, judged))
    })
}

/// 1 周ぶんの判定を、送る形へまとめる（#182）。
///
/// **繋ぎを純関数にしてある**——`SearchOutcome` から結果への振り分けは、退避されたファイルを
/// 用意できない環境でも検査できる唯一の場所（`docs/rules/testing.md` の「テストが見ている
/// 入口と、本番が通る入口をずらさない」）。ここが `NotDownloaded` を捨てると、読めなかった
/// ことが画面に出なくなる。件数ではなく録音そのものを運ぶ理由は `not_downloaded_count`。
fn collect_findings(
    generation: u64,
    judged: Vec<(recordings::RecordingSession, SearchOutcome)>,
) -> SearchResult {
    let mut result = SearchResult {
        generation,
        matched: Vec::new(),
        not_downloaded_dirs: Vec::new(),
    };
    for (session, outcome) in judged {
        match outcome {
            SearchOutcome::Matched => result.matched.push(session),
            SearchOutcome::NotDownloaded => result.not_downloaded_dirs.push(session.dir),
            SearchOutcome::Missed => {}
        }
    }
    result
}

/// 検索を背景スレッドで走らせる。**本文は UI スレッドで読まない**——文字起こし JSON と
/// `summary.md` は数百 KB になりうるし、保存先はネットワーク越しのこともある（`#152` と同じ理由）。
///
/// 空の検索語では走らせない（呼び出し側が解除として扱う）。
fn spawn_search(
    needle: String,
    sessions: Vec<recordings::RecordingSession>,
    generation: u64,
    sender: &std::sync::mpsc::Sender<SearchResult>,
) {
    let sender = sender.clone();
    // **走っている検索へ「もう要らない」を伝える手**（`spawn_session_load` と同じ機構）。
    // 打鍵のたびに投げるので、降りる手が無いと 1 語打つ間に何本もが全件を読み切り、いま見たい
    // 検索の結果を自分で遅くする。世代で結果を捨てるだけでは I/O は減らない。
    let live = Arc::new(AtomicU64::new(generation));
    SEARCH_WATCHERS.with(|watchers| {
        let mut watchers = watchers.borrow_mut();
        watchers.retain(|w| Arc::strong_count(w) > 1);
        watchers.push(Arc::clone(&live));
    });
    let spawned = std::thread::Builder::new()
        .name("session-search".to_owned())
        .spawn(move || {
            let needle = needle.to_lowercase();
            let Some(result) = search_sessions(sessions, &needle, &live, generation) else {
                // 世代が進んだので降りた。結果は送らない（送っても捨てられる）。
                return;
            };
            if sender.send(result).is_err() {
                eprintln!("Skipping the search result because the app is shutting down");
            }
        });
    if let Err(err) = spawned {
        // スレッドを作れない（資源の枯渇）。**絞り込まないまま**返し、一覧を消さない。
        eprintln!("Skipping the search because spawning failed: {err}");
    }
}

/// 一覧を入れ替えたあとの選択を決める。
///
/// **選んでいた録音が新しい一覧にも居るなら、添字を付け替えて選択も再生も続ける**——1 文字
/// 打つたびに聴いているものを止められるのは、探しながら聴く使い方を壊す。
/// 居ないなら、他の「一覧を入れ替える」経路（開く・削除）と同じく**再生対象を手放してから**
/// 選択を畳む（畳むだけだと、選択は無いのに音が鳴り続ける）。
fn reselect_after_list_change(
    rec: &LibraryWindow,
    sessions: &Rc<RefCell<Vec<recordings::RecordingSession>>>,
    next: Vec<recordings::RecordingSession>,
    runner: &EffectRunner,
) {
    // 入れ替える**前**に、いま選んでいる録音を控える（添字は入れ替えで意味が変わる）。
    let selected_dir = usize::try_from(rec.get_selected_index())
        .ok()
        .and_then(|index| sessions.borrow().get(index).map(|s| s.dir.clone()));
    let moved_to = selected_dir.and_then(|dir| {
        next.iter()
            .position(|session| session.dir == dir)
            .and_then(|index| i32::try_from(index).ok())
    });
    *sessions.borrow_mut() = next;
    match moved_to {
        Some(index) => {
            rec.set_selected_index(index);
            // **中身は読み直す**。絞り込んでいる間に文字起こし・議事録が終わっていることが
            // あり、添字を付け替えるだけでは古い内容が残る。音声は読み直さないので、
            // 鳴っているものは止まらない（`PlaybackLoad::Keep`）。
            let dir = {
                let sessions = sessions.borrow();
                usize::try_from(index)
                    .ok()
                    .and_then(|i| sessions.get(i))
                    .map(|session| session.dir.clone())
            };
            if let Some(dir) = dir {
                // **音は読み直さない**（`PlaybackLoad::Keep`）。同じ録音を選び直しただけなので、
                // 差し替えると鳴っているものが止まる。判断は `update` が持つ。
                let effects = {
                    let mut state = runner.state.borrow_mut();
                    shoki_core::update(
                        &mut state,
                        shoki_core::Msg::Command(shoki_core::Command::Select(Some(dir))),
                    )
                };
                run_effects(runner, effects, None);
            }
        }
        None => {
            clear_library_selection(rec, runner);
        }
    }
}

/// 一覧の走査が**いまどうなっているか**（#181）。
///
/// **状態として持ち、呼び出し側に値を選ばせない**——真偽値を引数で渡す形にすると、走査と
/// 関係のない経路（削除・検索・解除）が毎回「走査中ではない」と書くことになり、走査中に
/// そのどれかが通っただけで空表示が `Looking for recordings…` から `No recordings yet` へ
/// 黙って戻る。#181 が消したはずの嘘が、別の経路から復活する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScanState {
    /// 走査を投げて、結果を待っている。件数はまだ無い（0 件とは違う）。
    Awaiting,
    /// 走査を**始められなかった**（`ScanOutcome::CouldNotStart`）。1 件も見ていない。
    CouldNotStart,
    /// 走査は終わっている。**起動直後もここ**——まだ開いていない＝飛んでいる走査は無い。
    Settled,
}

/// 一覧の下端に出す件数（#182）。
///
/// **構造体で渡す**——どれも `usize` なので、引数で並べると渡し違えても通る
/// （`docs/rules/coding-conventions.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ListCounts {
    /// いま一覧に出ている件数（絞り込むと縮む）。
    shown: usize,
    /// 走査で見つかった全件。
    total: usize,
    /// 最後の検索で本文を読めなかった件数（#182）。
    not_downloaded: usize,
}

/// 一覧の下の件数と空表示を入れる。**走査中か・絞り込み中かで文が変わる**ので、件数と走査の
/// 状態をまとめて渡して**ここ 1 箇所で決める**（削除・検索・解除・走査のどこから来ても同じ形に
/// なる）。
///
/// **引数ではなく共有の 1 つの状態を読む**。値で受けると呼び出し側が「走査中ではない」と書ける
/// ので、上の `ScanState` の doc に書いた復活が起きる。
///
/// **型は書き込みを禁じていない**（`&Cell` は `set` できる）。効いているのは「**書くのは走査の
/// 経路だけ**」という約束——投げるとき（`open_library_window`）と着地したとき
/// （`apply_scanned_sessions`）の 2 箇所だけが `set` し、削除・検索・解除は読むだけ。走査以外から
/// `set` しないことはコンパイラではなく約束で守っている。
fn apply_list_counts(rec: &LibraryWindow, counts: ListCounts, scan: &Cell<ScanState>) {
    let ListCounts {
        shown,
        total,
        not_downloaded,
    } = counts;
    rec.set_library_summary(library_text::library_summary(total).into());
    // 空表示の文も**同じ値から**決める（#182）。件数の文とは別に組み立てると、片方だけ
    // 「読めなかった」を言う画面ができる。
    //
    // **走査の状態を先に見る**（#181）。走査中は件数がまだ無いので、絞り込みの有無を見ても
    // 意味のある答えにならない——0 件は「当たらなかった」ではなく「まだ数えていない」。
    // 走査中でも検索欄は生きている（打てる）ので、この優先順位は実際に効く。
    let (heading, body) = library_text::empty_list_message(match scan.get() {
        ScanState::Awaiting => library_text::EmptyList::Scanning,
        ScanState::CouldNotStart => library_text::EmptyList::ScanFailed,
        ScanState::Settled if rec.get_search_text().is_empty() => {
            library_text::EmptyList::NoRecordings
        }
        ScanState::Settled => library_text::EmptyList::NoMatches { not_downloaded },
    });
    rec.set_empty_heading(heading.into());
    rec.set_empty_body(body.into());
    // 絞り込んでいないなら件数の文は出さない（`library_summary` が出る）。**このとき
    // `not_downloaded` は必ず 0**——`SearchOutcome` は 1 録音につき 1 値で `Matched` と
    // `NotDownloaded` が排他、どちらも同じ一覧で絞ってから数えるので、全件が当たったなら
    // 読めなかったものは無い（#182）。
    rec.set_search_summary(if shown == total {
        slint::SharedString::new()
    } else {
        library_text::search_summary_text(shown, total, not_downloaded).into()
    });
}

/// 届いた検索結果を、**控えて・数えて・画面へ入れる**（#182）。当たった録音を返す。
///
/// **控えるところから画面までを 1 本にしてある**。控える 1 行だけが外に残ると、そこを消した
/// だけで「読めなかったことを伝える」機能が丸ごと死ぬのに、部品はどれも単体で緑のまま通る
/// （`docs/rules/testing.md` の「テストが見ている入口と、本番が通る入口をずらさない」。
/// 継ぎ目を下げるたびに穴も 1 段下がるので、**ウィンドウごと呼べる形にして止める**）。
///
/// 控えるのは、削除の経路が同じ値から数え直すため（`not_downloaded_count`）。
fn apply_search_result(
    rec: &LibraryWindow,
    stored: &RefCell<Vec<std::path::PathBuf>>,
    all: &[recordings::RecordingSession],
    scan_state: &Cell<ScanState>,
    result: SearchResult,
) -> Vec<recordings::RecordingSession> {
    *stored.borrow_mut() = result.not_downloaded_dirs;
    let counted = count_search_result(result.matched, &stored.borrow(), all);
    // **走査中でも届く**（#181）。走っている間に打たれた検索は空の全件を舐めるので必ず 0 件に
    // なる——ここで「当たらなかった」と言わないよう、空表示の文は `scan_state` から決める
    // （`apply_list_counts`）。走査が着地したら `apply_scanned_sessions` が投げ直す。
    apply_list_counts(
        rec,
        ListCounts {
            shown: counted.matched.len(),
            total: counted.total,
            not_downloaded: counted.not_downloaded,
        },
        scan_state,
    );
    counted.matched
}

/// 届いた検索結果を、いまの一覧に合わせて数え直す（#182）。
///
/// **繋ぎを純関数にしてある**——ここが「読めなかった件数」を落としても、部品はどれも単体で
/// 緑のまま通ってしまう（`docs/rules/testing.md` の「テストが見ている入口と、本番が通る入口を
/// ずらさない」）。この 1 式が機能そのもの: 潰すと、退避されて検索できなかったことは画面から
/// 完全に消える。
///
/// 結果は**打鍵した時点のスナップショット**なので、そのままでは古い。
fn count_search_result(
    mut matched: Vec<recordings::RecordingSession>,
    not_downloaded_dirs: &[std::path::PathBuf],
    all: &[recordings::RecordingSession],
) -> CountedSearch {
    // 絞り込んでいる間に消えた録音は落とす（削除は世代を進めるので通常は届かないが、
    // 取りこぼしたときに消したはずの行を戻さない）。
    matched.retain(|session| all.iter().any(|other| other.dir == session.dir));
    CountedSearch {
        matched,
        total: all.len(),
        not_downloaded: not_downloaded_count(not_downloaded_dirs, all),
    }
}

/// 数え直した検索結果（#182）。**3 つを 1 つの値で返す**——別々に返すと、呼び出し側で
/// 取り違えても通る形（`usize` が 2 つ並ぶ）が残る。
struct CountedSearch {
    /// いまも一覧に在る、当たった録音。
    matched: Vec<recordings::RecordingSession>,
    /// 一覧の全件。
    total: usize,
    /// 実体が無くて検索できなかった件数。
    not_downloaded: usize,
}

/// いま一覧に在るもののうち、**実体が無くて検索できなかった件数**（#182）。
///
/// 検索結果は打鍵した時点のスナップショットなので、**削除されたぶんを落としてから数える**。
/// そのまま数えると、絞り込み中に 1 件消しただけで「10 件中 11 件が未ダウンロード」という
/// 合計より多い数が出る（`matched` を `retain` しているのと同じ理由）。
fn not_downloaded_count(
    not_downloaded_dirs: &[std::path::PathBuf],
    all: &[recordings::RecordingSession],
) -> usize {
    // **一覧を 1 度だけ引ける形にする**。総当たりだと、この修正が狙う「保存先まるごと退避」
    // （読めなかった件数＝全件）で打鍵ごとに n² 回の比較が UI スレッドに乗る。
    let alive: std::collections::HashSet<&std::path::Path> =
        all.iter().map(|session| session.dir.as_path()).collect();
    not_downloaded_dirs
        .iter()
        .filter(|dir| alive.contains(dir.as_path()))
        .count()
}

/// `Effect` を実行するのに要るものだけを束ねたもの（#188）。
///
/// **`LibraryHandles` から切り出してある**。コールバックを登録するのは `LibraryHandles` を
/// 組むより前なので、あちらを要求すると選択の経路から `Effect` を実行できない。
#[derive(Clone)]
struct EffectRunner {
    ui: slint::Weak<LibraryWindow>,
    state: Rc<RefCell<shoki_core::AppState>>,
    /// 画面に出しているセグメント（再生位置のハイライトが読む）。
    segments: Rc<RefCell<LoadedTranscript>>,
    sessions: Rc<RefCell<Vec<recordings::RecordingSession>>>,
    player: Rc<RefCell<Option<player::AudioPlayer>>>,
    load_generation: Rc<Cell<u64>>,
    load_sender: std::sync::mpsc::Sender<LoadedSession>,
}

/// core が返した依頼を実行する（#188）。
///
/// **呼ぶときは借用をすべて落としておくこと**。この中で `AppState` も `sessions` も借りるし、
/// 読み込みを投げると次の `Msg` が起きうる（`docs/rules/coding-conventions.md` の
/// 「`RefCell` の借用を持ったまま、作り直しの経路を呼ばない」）。
///
/// `loaded` は「いま届いた読み込み結果」。`Effect::ShowLoaded` が返ってきたときだけ画面へ入れる
/// ——**受け入れるかを決めるのは core 1 箇所**で、ここでもう一度世代を見ると判定が 2 つになる。
fn run_effects(
    recordings: &EffectRunner,
    effects: Vec<shoki_core::Effect>,
    mut loaded: Option<LoadedSession>,
) {
    let Some(rec) = recordings.ui.upgrade() else {
        return;
    };
    for effect in effects {
        match effect {
            shoki_core::Effect::LoadSession {
                dir,
                replaces_playback,
            } => {
                let session = recordings
                    .sessions
                    .borrow()
                    .iter()
                    .find(|session| session.dir == dir)
                    .cloned();
                let Some(session) = session else {
                    // 一覧に居ない（閉じている間に完了して、開き直した直後で走査がまだ）。
                    // **読み込み中を残さない**——残すと選び直すまでその表示のまま固まる。
                    let effects = {
                        let mut state = recordings.state.borrow_mut();
                        shoki_core::update(
                            &mut state,
                            shoki_core::Msg::Event(shoki_core::Event::LoadCouldNotStart { dir }),
                        )
                    };
                    run_effects(recordings, effects, None);
                    continue;
                };
                if replaces_playback {
                    // **前の録音の音声はすぐ手放す**（読み込みを待つ間に前の音が鳴らないように）。
                    // 音源を差し替える依頼のときだけ——中身の読み直しで手放すと、鳴っている音が
                    // 止まって先頭へ戻る（`PlaybackLoad` の doc）。
                    if let Some(p) = recordings.player.borrow_mut().as_mut() {
                        p.unload();
                    }
                }
                let generation_id = advance_load_generation(&recordings.load_generation);
                spawn_session_load(
                    &session,
                    generation_id,
                    &recordings.load_sender,
                    replaces_playback,
                );
            }
            shoki_core::Effect::ShowLoaded => {
                if let Some(loaded) = loaded.take() {
                    apply_loaded_session(&rec, &recordings.player, &recordings.segments, loaded);
                }
            }
            shoki_core::Effect::ClearLoaded => {
                *recordings.segments.borrow_mut() = LoadedTranscript::unknown();
                clear_shown_transcript(&rec);
            }
        }
    }
}

/// ワーカーを変えた**直後にその場で** `AppState` へ写す（#188）。
///
/// tick を待つと最大 100ms のあいだ画面が「何も起きていない」と答える。押した瞬間に
/// 「Stopping…」へ移らないと、押しても効いていないように見える（#163）。**削除のガードも
/// これに乗っている**——`on_delete_session` は「投入の直後にゲートが閉じる」ことを前提に、
/// 走行中セッションを消せない形にしている。
///
/// **返った依頼は必ず実行する**。ここが `jobs` を揃えてしまうので、捨てると次の tick の差分は
/// 1 件も立たない——押す直前にワーカーが完了していると、その読み直しがここで消える
/// （完成した発話が画面に出ないまま残る）。**借用を落としてから**実行するのは `run_effects` の doc
/// のとおり。
fn observe_jobs(transcriber: &transcribe::TranscribeWorker, runner: &EffectRunner) {
    let effects = {
        let mut state = runner.state.borrow_mut();
        job_changes(transcriber, &state)
            .into_iter()
            .flat_map(|msg| shoki_core::update(&mut state, msg))
            .collect::<Vec<_>>()
    };
    run_effects(runner, effects, None);
}

/// ワーカーの状態マップと `AppState.jobs` を突き合わせ、**違うものだけ**を `Msg` にする（#188）。
///
/// **差分にするのは、全部流すと 100ms ごとに全ジョブぶんの `Msg` が立つから**。`update` は
/// 「走っていた状態から降りたか」で読み直しを決めるので、変わっていないものを流すと判断が
/// 濁る（前も今も `Done` で降りた扱いになりかねない）。
fn job_changes(
    transcriber: &transcribe::TranscribeWorker,
    state: &shoki_core::AppState,
) -> Vec<shoki_core::Msg> {
    let snapshot = transcriber.snapshot();
    let mut changes = Vec::new();
    let mut seen: std::collections::HashSet<&std::path::Path> = std::collections::HashSet::new();
    for (dir, seq, worker_state) in &snapshot {
        seen.insert(dir.as_path());
        let job = shoki_core::Job {
            id: shoki_core::JobId(*seq),
            phase: job_phase_of(worker_state),
        };
        if state.job(dir) != Some(&job) {
            changes.push(shoki_core::Msg::Event(shoki_core::Event::JobChanged {
                dir: dir.clone(),
                job: Some(job),
            }));
        }
    }
    // **消えたエントリも流す**。止めた・対象が無かったジョブはマップから消えるので、
    // これを拾わないと表示が古いまま残る。
    for dir in state.jobs().keys() {
        if !seen.contains(dir.as_path()) {
            changes.push(shoki_core::Msg::Event(shoki_core::Event::JobChanged {
                dir: dir.clone(),
                job: None,
            }));
        }
    }
    changes
}

/// ワーカーの状態を core の語彙へ写す（**網羅 match**——相を足したら写し忘れで割れる）。
fn job_phase_of(state: &transcribe::TranscribeState) -> shoki_core::JobPhase {
    match state {
        transcribe::TranscribeState::Transcribing {
            model_label,
            percent,
        } => shoki_core::JobPhase::Running {
            model_label: model_label.clone(),
            percent: *percent,
        },
        transcribe::TranscribeState::Stopping { model_label } => shoki_core::JobPhase::Stopping {
            model_label: model_label.clone(),
        },
        transcribe::TranscribeState::Done { shortfall } => shoki_core::JobPhase::Done {
            shortfall: *shortfall,
        },
        transcribe::TranscribeState::Failed { reason } => shoki_core::JobPhase::Failed {
            reason: reason.clone(),
        },
    }
}

/// 生成物の有無を書き戻す（#161 / #188）。**毎 tick の全件スキャン**。
///
/// 遷移で拾うと、絞り込みで隠れている録音の完了を取りこぼす（検索を解除したときに、済んで
/// いるはずの文字起こしが「無い」に戻る）。ロック 1 回のマップ引きだけで、ディスクは読まない。
fn sweep_finished_jobs(recordings: &LibraryHandles) {
    let state = recordings.state.borrow();
    let mark = |list: &mut Vec<recordings::RecordingSession>| {
        for session in list.iter_mut() {
            if !session.has_transcript
                && matches!(
                    state.job(&session.dir).map(|job| &job.phase),
                    Some(shoki_core::JobPhase::Done { .. })
                )
            {
                session.has_transcript = true;
            }
            if !session.has_summary
                && recordings.summarizer.status_of(&session.dir)
                    == Some(summarize::SummarizeStatus::Done)
            {
                session.has_summary = true;
            }
        }
    };
    mark(&mut recordings.all_sessions.borrow_mut());
    mark(&mut recordings.sessions.borrow_mut());
}

/// 表示中の文字起こしと議事録を手放す（#188 の `Effect::ClearLoaded`）。
///
/// どちらも発話由来の機微データなので、詳細ペインが隠れている間も持ち続けない
/// （`docs/rules/security.md`）。**core の `loaded` を落とすのと対**——片方だけ落とすと、
/// 画面には残っているのに状態は「読み込み中」、という食い違いになる。
fn clear_shown_transcript(rec: &LibraryWindow) {
    rec.set_segments(Rc::new(slint::VecModel::<TranscriptRow>::default()).into());
    rec.set_summary_rows(Rc::new(slint::VecModel::<SummaryRow>::default()).into());
    rec.set_detail_summary_footer(slint::SharedString::new());
    // **議事録の状態も畳む**（#188）。`refresh_detail_panes` は「前の状態」をこのプロパティから
    // 取るが、プロパティは**どの録音のものかを持たない**——畳まないと、A を要約中に B を選んだ
    // ときに「A の生成中 → B の生成済み」を遷移と読み、B の音声つき読み込みを降ろしてしまう
    // （世代が進むので、選んだ録音が再生できないまま残る）。
    rec.set_detail_summary_status(slint_map::to_ui_summary_status(
        shoki_core::SummaryStatus::NotSummarized,
    ));
    rec.set_current_segment(-1);
    // **読み込みが終わるまでは再生できない**（音源をまだ開いていない）。
    rec.set_playable(false);
    rec.set_seekable(false);
    apply_playback_position(rec, Duration::ZERO, None);
    // 開いた途中結果も畳む（理由は `fold_partial_transcript` の doc）。
    fold_partial_transcript(rec);
}

/// 一覧のモデルと、行ごとの `RowKey` を**一緒に**持つ（#188）。
///
/// `RowKey` は「この行の表示が変わったか」を確保なしで見るための値。モデルと別の `Vec` に置くと
/// ずれうる（削除の `remove` は以降の行が 1 つ繰り上がる）ので、**触る口をこの型のメソッドに
/// 限る**。ここを通らずにモデルへ書く経路を作らないこと（`model()` はモデルを Slint へ渡す
/// ためだけに開けてある）。
///
/// **ずれても誤った間引きは起きない**。キーと一緒に**どの録音か**を持ち、両方が一致したときだけ
/// 間引くので、ずれた行は「一致しない」側へ倒れて組み直され、そこで正しい対が入る（自己修復）。
/// `remove` で詰めるのは、長さをモデルと揃えて余計な組み直しを避けるため——**正しさを作って
/// いるのは識別子の比較のほう**。
struct SessionRows {
    model: Rc<slint::VecModel<SessionRow>>,
    /// 行ごとの「どの録音か」と「表示に効く値」。
    ///
    /// **識別子も持つ**（#188）。`RowKey` は表示に効く値だけなので、facts が同じ 2 つの録音は
    /// 同じキーになる——キーだけで比べると、並びがずれたときに「変わっていない」と誤判定して
    /// その行が二度と更新されない。ずれない仕組みは下のメソッドが持つが、**気づける手も残す**。
    keys: RefCell<Vec<(std::path::PathBuf, shoki_core::RowKey)>>,
}

impl SessionRows {
    fn new() -> Self {
        Self {
            model: Rc::new(slint::VecModel::default()),
            keys: RefCell::new(Vec::new()),
        }
    }

    /// Slint へ渡すモデル（**書き込みには使わない**）。
    fn model(&self) -> Rc<slint::VecModel<SessionRow>> {
        Rc::clone(&self.model)
    }

    /// いま出ている行数（テストが読む）。
    #[cfg(test)]
    fn row_count(&self) -> usize {
        use slint::Model as _;
        self.model.row_count()
    }

    /// 一覧を丸ごと入れ替える。
    fn replace_all(&self, list: &[recordings::RecordingSession], state: &shoki_core::AppState) {
        self.model.set_vec(session_rows(list, state));
        *self.keys.borrow_mut() = list
            .iter()
            .map(|session| (session.dir.clone(), shoki_core::row_key(state, session)))
            .collect();
    }

    /// 1 行消す（以降が繰り上がる）。
    fn remove(&self, index: usize) {
        self.model.remove(index);
        let mut keys = self.keys.borrow_mut();
        if index < keys.len() {
            keys.remove(index);
        }
    }

    /// 見出しだけ差し替える（削除で繰り上がった行）。**キーは動かない**——見出しは
    /// `RowKey` にも `Row` にも入っていない（`shoki_core::view::RowKey` の doc）。
    fn set_heading(&self, index: usize, heading: String) {
        use slint::Model as _;
        if let Some(mut row) = self.model.row_data(index) {
            row.group_heading = heading.into();
            self.model.set_row_data(index, row);
        }
    }

    /// 表示が変わっていれば差し替える。**変わっていなければ何もしない**（`view_row` の確保も
    /// 払わない）。見出しは既存の行から引き継ぐ。
    fn refresh(
        &self,
        index: usize,
        session: &recordings::RecordingSession,
        state: &shoki_core::AppState,
    ) {
        use slint::Model as _;
        let key = shoki_core::row_key(state, session);
        if self
            .keys
            .borrow()
            .get(index)
            .is_some_and(|(dir, previous)| {
                // **識別子も見る**。表示に効く値だけで比べると、facts が同じ 2 つの録音を
                // 取り違える（`keys` の doc）。
                dir == &session.dir && previous == &key
            })
        {
            return;
        }
        let Some(previous) = self.model.row_data(index) else {
            return;
        };
        let row = to_session_row(
            &shoki_core::view_row(state, session),
            previous.group_heading,
        );
        self.model.set_row_data(index, row);
        let mut keys = self.keys.borrow_mut();
        if let Some(slot) = keys.get_mut(index) {
            *slot = (session.dir.clone(), key);
        }
    }
}

/// セッションの並びから一覧の行を組み立てる。**開くときと絞り込み後で同じ経路を通す**
/// （片方だけ古い組み立てのまま残らないように。`docs/rules/slint.md`）。
///
/// 行と渡した並びは 1 対 1。**間引くならこの関数へ渡す前**に間引くこと——ここで絞ると添字が
/// ずれ、`get(i)` は範囲内を返すので黙って別の録音を操作する。
fn session_rows(
    list: &[recordings::RecordingSession],
    state: &shoki_core::AppState,
) -> Vec<SessionRow> {
    let now = chrono::Local::now().naive_local();
    list.iter()
        .enumerate()
        .map(|(index, session)| {
            // 見出しは**その日の最初の行だけ**が持つ（直前の行と比べて決める）。行に持たせる
            // 理由は `SessionRow` の doc。**見出しだけ shell が組む**——`view_row` は行 1 つを
            // 見る関数なので、直前の行との比較には答えられない。
            let heading = session_group_heading(list, index, now);
            to_session_row(&shoki_core::view_row(state, session), heading)
        })
        .collect()
}

/// core が組んだ行を Slint の行にする（#188）。
///
/// **見出しは別に渡す**。`Row` に含めていないので、ここで足りない 1 つを補う形にしてある
/// ——差分更新のときは既存の行の見出しをそのまま渡すことで、見出しが消えない。
fn to_session_row(row: &shoki_core::Row, heading: impl Into<slint::SharedString>) -> SessionRow {
    SessionRow {
        group_heading: heading.into(),
        time_text: row.time_text.as_str().into(),
        date_text: row.date_text.as_str().into(),
        detail_text: row.detail_text.as_str().into(),
        transcript_status: slint_map::to_ui_transcript_status(row.transcript_status),
    }
}

/// トレイの「Library…」で Library ウィンドウを開く。保存先を走査して一覧を更新し、
/// 選択・再生状態を初期化してから表示する（初回表示はジオメトリを明示する。`docs/rules/slint.md`）。
fn open_library_window(
    rec: &LibraryWindow,
    handles: &LibraryHandles,
    config: &Rc<RefCell<Config>>,
    geometry_committed: &mut bool,
    last_play_secs: &mut Option<u64>,
) {
    // **走査は別スレッドへ投げ、窓は待たずに出す**（#181）。保存先が遅いボリューム
    // （ネットワーク共有・外付け・スピンドル）だと、ここで待つとトレイから押しても窓が出ない。
    // 結果は tick が拾う（`apply_scanned_sessions`）。
    //
    // **走査を投げるのはここだけ**。保存先を変えても（`on_choose_folder`）一覧は作り直さない
    // ので、開いたまま変えると前の保存先の録音が並んだままになる（次に開き直すと直る）。
    // PR 前から同じ挙動で、直すには Settings 側から一覧の作り直しを起こす必要がある。
    //
    // **世代を先に進める**——閉じて開き直したとき、前の走査の結果が後から届いて一覧を
    // 書き換えないように（`spawn_session_load` と同じ流儀）。
    let generation = advance_scan_generation(&handles.scan_generation);
    spawn_session_scan(
        config.borrow().recording_dir.clone(),
        generation,
        &handles.scan_sender,
    );
    // 開くたびに検索は解除しておく（前に開いたときの絞り込みが残っていると、録音が消えたように
    // 見える）。**世代も進める**——走っていた検索の結果が後から届いて絞り込むのを防ぐ。
    reset_search(
        rec,
        &handles.search_generation,
        &handles.search_not_downloaded,
    );
    // **前の一覧は残さず、空にして待つ**（#181）。残すと、保存先を変えてから開き直した直後に
    // 前の保存先の録音が並び、押せてしまう（選ぶと読み込みが失敗する）。
    handles
        .sessions_model
        .replace_all(&[], &handles.state.borrow());
    // **走査中であることを言う**（#181）。0 件のまま「録音が無い」と言うと、遅い保存先で
    // 開いた人は録音を失ったと思う。ここで出るのは `Looking for recordings…`
    // （`library_text::EmptyList::Scanning`）。
    //
    // **体感時間を決めているのは走査の速さではなく 100ms のポーリング間隔**。走査を投げるのも
    // 結果を拾うのも同じ tick の中（メニューイベントの処理 → 受け口のドレイン）なので、走査
    // スレッドがその間に読み切れば同じ周回で着地するが、まず間に合わないので次以降の周回になる。
    // 例外は走査スレッドを立てられなかったとき——`spawn_session_scan` が UI スレッドから同期で
    // 送るので、この文言は出ないまま `Could not look for recordings` へ差し替わる。
    handles.scan_state.set(ScanState::Awaiting);
    apply_list_counts(
        rec,
        ListCounts {
            shown: 0,
            total: 0,
            not_downloaded: 0,
        },
        &handles.scan_state,
    );
    handles.all_sessions.borrow_mut().clear();
    // 開くたびに未選択・停止表示へ初期化する。
    clear_library_selection(rec, &handles.runner);
    // **開き直しでは再生対象を手放す**（未選択表示に合わせて「何もロードされていない」状態へ
    // 揃える。理由は `AudioPlayer::unload` の doc）。閉じただけでは手放さない——閉じても
    // 鳴っているものは鳴り続ける、という既存の挙動を変えないため。
    if let Some(p) = handles.player.borrow_mut().as_mut() {
        p.unload();
    }
    handles.sessions.borrow_mut().clear();
    *last_play_secs = None;
    show_window(
        rec.window(),
        geometry_committed,
        slint::LogicalPosition::new(LIBRARY_X, LIBRARY_Y),
        slint::LogicalSize::new(LIBRARY_WIDTH, LIBRARY_HEIGHT),
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
fn apply_playback_position(rec: &LibraryWindow, position: Duration, duration: Option<Duration>) {
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
/// 保存後、（設定 ON なら）文字起こしをワーカーへ投入し、両音源が保存できていれば Library 用の
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

/// 文字起こしのあと議事録まで続けるか（#165）。
///
/// **真偽値で渡さない**——呼び出し側が 2 つあり、`true` / `false` だけだと、どちらが「続ける」
/// なのかが呼び出し行から読めない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChainNotes {
    /// 押されたら必ず続ける（Notes タブの「Transcribe, then write notes」）。
    Always,
    /// 設定 `auto_summarize` に従う（Transcript タブの Transcribe / Re-transcribe）。
    ///
    /// 手動の再実行でも設定 ON なら要約を作り直す——作り直さないと `summary.md` が古い
    /// 文字起こしのまま残り、内容が食い違う。
    FollowTheSetting,
}

/// 選択中セッションの文字起こしを（再）実行する。
///
/// **2 つのボタンで同じ関数を通す**（#165）。違うのは議事録まで続けるかどうかだけで、別々に
/// 組むと片方だけ設定のスナップショットや音源の選び方を落とす。設定値はここでスナップショット
/// し、処理中の設定変更の影響を受けない。
///
/// 音源が 1 つも無いセッションには投げない（`run_job` も空を弾くが、状態を「文字起こし中」に
/// してから何もしないのを避ける）。
fn submit_transcription(
    session: &recordings::RecordingSession,
    config: &RefCell<Config>,
    transcriber: &transcribe::TranscribeWorker,
    chain: ChainNotes,
) {
    let audio_paths = recordings::audio_source_paths(session);
    if audio_paths.is_empty() {
        return;
    }
    let config_ref = config.borrow();
    let summarize = match chain {
        ChainNotes::Always => Some(chained_summarize_job(&config_ref, &session.dir)),
        ChainNotes::FollowTheSetting => auto_summarize_job(&config_ref, &session.dir),
    };
    transcriber.submit(transcribe::TranscribeJob {
        session_dir: session.dir.clone(),
        audio_paths,
        model_id: config_ref.whisper_model.clone(),
        model_override: config_ref.whisper_model_path.clone(),
        language: config_ref.transcribe_language.clone(),
        summarize,
    });
}

/// 手動（Library ウィンドウの Summarize）の議事録生成の依頼を組み立てる。設定値
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
    config
        .auto_summarize
        .then(|| chained_summarize_job(config, session_dir))
}

/// 文字起こしジョブへぶら下げる議事録生成の依頼（設定は見ない）。
///
/// **`existing_is_stale` を立てるのはここ 1 箇所**。この経路では既存の `summary.md` は
/// **前の文字起こし**の議事録なので、新しい文字起こしができた時点で古い。設定 ON の自動投入
/// （`auto_summarize_job`）と、Notes タブの「Transcribe, then write notes」（#165）が同じ
/// ものを使う——別々に組むと、片方だけ `existing_is_stale` を落とす事故が起きる。
fn chained_summarize_job(
    config: &Config,
    session_dir: &std::path::Path,
) -> summarize::SummarizeJob {
    summarize::SummarizeJob {
        existing_is_stale: true,
        ..manual_summarize_job(config, session_dir)
    }
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

/// ウィンドウを表示する。設定・Library の両ウィンドウで共用する。
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
/// 行う。対象の NSWindow を直接キー化するため、設定・Library の両方が開いていても選んだ
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

/// 詳細ペインの文字起こし表示（状態テキスト・状態依存の配色・縮退ラベル）を反映する。
/// 選択時・手動投入直後・tick 追従の全経路でここを通し、表示ロジックを 1 箇所にする。
///
/// **ボタンの活性は Rust から set しない**。Slint 側が状態 enum から導出する 2 つのゲートで
/// 決める（bool を別途渡して enum と食い違う余地を作らないため。`docs/rules/slint.md`）:
/// `detail-files-in-use`（文字起こし中・要約生成中＝ワーカーがファイルを読み書き中）が Delete を、
/// `detail-jobs-pending`（それ＋要約のキュー待ち）が Transcribe / Summarize を止める。
fn apply_detail_transcript_status(
    rec: &LibraryWindow,
    detail: &shoki_core::DetailView,
    jobs_pending: bool,
) {
    // **読み込み中かも core が決める**（#188）。shell が別に立てると、`stored` の判定と
    // 食い違う組み合わせ（読み込み中ではないのに別の録音の結果として扱う）ができる。
    // **未選択の経路もここを通る**（`blank_detail_view`）ので、書く場所は 1 つ。
    rec.set_loading(detail.loading);
    let pane = &detail.transcript;
    let status = pane.status();
    let message = pane.message();
    rec.set_detail_transcript_text(pane.status_text().into());
    rec.set_detail_transcript_heading(message.heading.into());
    rec.set_detail_transcript_body(message.body.into());
    set_pane_actions(
        rec.get_detail_transcript_actions(),
        // **掛けるのはここ**（#188）。`view_detail` は掛けずに返す——議事録側の busy を知らない
        // ので、あちらで掛けると議事録生成中に Re-transcribe が押せる。
        &actions_allowed_while_busy(detail.actions.clone(), jobs_pending),
        |actions| rec.set_detail_transcript_actions(actions),
    );
    rec.set_detail_transcript_status(slint_map::to_ui_transcript_status(status));
    // 途中結果かどうかは状態 enum から出せない（#164。`TranscriptPane::shows_partial` の doc）
    // ので、見出し・理由・操作と**同じ値から**入れる。
    rec.set_detail_transcript_partial(pane.shows_partial());
    // 走り始めたら畳む（理由は `fold_partial_transcript` の doc）。**投入の経路が増えても
    // ここを通る**——表示はすべてこの関数から入る（`docs/rules/slint.md` の「導出は 1 つの
    // 関数に集める」）。
    if matches!(
        status,
        shoki_core::TranscriptStatus::Transcribing | shoki_core::TranscriptStatus::Stopping
    ) {
        fold_partial_transcript(rec);
    }
}

/// 選択が無いときに詳細ペインへ入れる形（#188）。
///
/// **`view_detail` を通さない**——通すには録音が要るが、ここは「録音が選ばれていない」場面。
/// `NotTranscribed { auto_on: false }` にするのは、次の選択で必ず上書きされるので設定の状態が
/// 関係しないため。
fn blank_detail_view() -> shoki_core::DetailView {
    let transcript = TranscriptPane::NotTranscribed { auto_on: false };
    let actions = transcript.message().actions;
    shoki_core::DetailView {
        transcript_input: shoki_core::TranscriptInput::Missing,
        transcript_busy: false,
        actions,
        loading: false,
        transcript,
    }
}

/// 開いた途中結果を畳む（#164）。**理由の正はここ**（呼び出し側は参照だけを置く）。
///
/// `Show partial` は「いま出ているこの途中結果を読む」という同意なので、対象が変わったら
/// 引き継がない。引き継ぐと、別の録音の途中結果や、Try again で作り直された**別の**途中結果が
/// 伏せられずにいきなり一覧で出る。
///
/// 畳むのは、対象が変わりうるすべての契機——録音を選び直したとき・選択を解除したとき・
/// 文字起こしが走り始めたとき。**列挙を増やすときはここへ**（呼び出し側に条件を書かない）。
fn fold_partial_transcript(rec: &LibraryWindow) {
    rec.set_show_partial_transcript(false);
}

/// 空表示のボタン列を**変わったときだけ**差し替える（0〜2 件）。
///
/// `ModelRc` の比較はポインタなので、同じ中身でも毎回入れ直すと Slint はリピータを畳んで
/// ボタンを作り直す。tick は 100ms ごとにここを通るので、入れっぱなしにすると押している最中に
/// ボタンが消える（文字列や enum のプロパティは Slint が値で比べるため、この心配は無い）。
fn set_pane_actions(
    current: slint::ModelRc<UiPaneAction>,
    actions: &[shoki_core::PaneAction],
    set: impl FnOnce(slint::ModelRc<UiPaneAction>),
) {
    use slint::Model as _;
    // **比べるのは Slint へ入れる形**（#188）。core の値のまま比べると、写像を変えたときに
    // 差分が立たず、古いボタンが残る。
    //
    // **比べるために確保しない**。ここは 100ms tick を通るので、一致して早期 return する
    // 大半のケースでも `Vec` と `SharedString` を作ることになる（実測で 1 回あたり約 78ns の差）。
    let same = current.row_count() == actions.len()
        && current
            .iter()
            .zip(actions.iter())
            .all(|(current, next)| slint_map::ui_pane_action_matches(&current, next));
    if same {
        return;
    }
    set(slint_map::to_ui_pane_actions(actions));
}

/// 選択中セッションの読む領域（両タブ）を組み直し、**議事録が完成へ移っていたら本文を
/// 読み直す**（#188。世代を進めて `spawn_session_load` を起こす。音声は差し替えない）。
///
/// **表示するだけの関数ではない**。**議事録の完成を契機に読み直すのはここだけ**——選択・文字
/// 起こしの完了・一覧の入れ替えからの読み直しは `run_effects` の `Effect::LoadSession` が起こす。
///
/// 世代を進める場所は 4 つ（数えた）。読み直しを起こすのは `Effect::LoadSession` とここの 2 つで、
/// 残りの 2 つ——`clear_library_selection` とウィンドウを閉じるとき——は**起こさず無効化する
/// だけ**。
///
/// ここで起こすのは、遷移を観測できるのが 1 回だけ（すぐ上書きする）だから。呼び出し側へ返して
/// 守らせると、押した瞬間の再描画が最初の観測者になったときに消える（ワーカーは別スレッドで、
/// tick が最初とは限らない）。
///
/// **選択時・手動投入直後・tick 追従の全経路がここを通る**。進捗の割合と経過は状態が変わらない
/// まま動くので、状態の差分だけで更新すると数字が止まって見える（`docs/rules/slint.md`）。
/// ボタン列だけは値が同じなら差し替えない（`set_pane_actions`）。
///
/// **ボタンの活性は Rust から set しない**。Slint 側が状態 enum から導出する 2 つのゲートで
/// 決める（bool を別途渡して enum と食い違う余地を作らないため。`docs/rules/slint.md`）:
/// `detail-files-in-use`（文字起こし中・要約生成中＝ワーカーがファイルを読み書き中）が Delete を、
/// `detail-jobs-pending`（それ＋要約のキュー待ち）が Transcribe / Summarize を止める。空表示の
/// ボタンは Slint に `enabled` を持たないので、同じ条件で**出すかどうか**を Rust が決める。
fn refresh_detail_panes(
    rec: &LibraryWindow,
    runner: &EffectRunner,
    summarizer: &summarize::SummarizeWorker,
    session: &recordings::RecordingSession,
    config: &RefCell<Config>,
) {
    let (auto_transcribe, auto_summarize) = {
        let config = config.borrow();
        (config.auto_transcribe, config.auto_summarize)
    };
    let state = &runner.state.borrow();
    // **文字起こし側の状態を答えるのはここ 1 本**（#188）。旧 4 関数
    // （`transcript_display_status` / `transcript_pane_of` / `LoadedTranscript::stored` /
    // `TranscriptInput::of`）はこの中へ畳んである。
    let detail = shoki_core::view_detail(state, session, auto_transcribe);
    // 議事録は旧経路のまま（#189 で core へ）。入力の様子だけ core から受け取る。
    let worker_state = summarizer.state_of(&session.dir);
    // **完成したという事実はワーカーの記録から取る**（#188）。ペインの `Done` は
    // `summary.md` の有無からも立つので、記録が**消えただけ**（取り消し・入力が無くて
    // 飛ばした）でも「完成した」に見える——そこで読み直すと世代が繰り上がり、選択直後に
    // 投げた音声つきの読み込みが降りて、選んだ録音が再生できないまま残る。
    let worker_finished = matches!(worker_state, Some(summarize::SummarizeState::Done));
    let summary = summary_pane_of(
        worker_state,
        session.has_summary,
        detail.transcript_input,
        auto_summarize,
    );
    // **両方を見てからボタンを決める**。走っているジョブは片方の状態にしか出ないので、
    // タブごとに判断すると、もう一方で走っているジョブを見落としたボタンが出る。
    // `view_detail` はボタンを掛けずに返す（議事録側を知らないので、あちらで掛けると穴が開く）。
    let jobs_pending = detail.transcript_busy || shoki_core::summary_is_pending(summary.status());
    // **書く前に読む**（#188）。前の状態はウィンドウの現在値から取るので、`apply_*` が書いた
    // あとで読むと「前」と「いま」が同じ値になり、遷移が永久に立たない
    // （`docs/rules/coding-conventions.md` の「値を 2 度読まず、書いた値そのもので分岐する」）。
    //
    // **観測と読み直しをここで完結させる**（呼び出し側に順序を守らせない）。プロパティは
    // どの録音のものかを持たないので、表示する録音が変わるときは `clear_shown_transcript` が
    // 畳んでいる（そちらの doc）。
    let previous = slint_map::from_ui_summary_status(rec.get_detail_summary_status());
    let just_finished = shoki_core::summary_is_pending(previous) && worker_finished;
    apply_detail_transcript_status(rec, &detail, jobs_pending);
    apply_detail_summary_status(rec, &summary, jobs_pending);
    if just_finished {
        // **ここで読み直す**（#188）。遷移を観測できるのは 1 回だけ（すぐ上で上書きする）なので、
        // 呼び出し側へ返して「立っていたら読み直す」を守らせると、押した瞬間の再描画が最初の
        // 観測者になったときに消える——ワーカーは別スレッドなので、tick が最初とは限らない。
        //
        // **音声は読み直さない**。変わったのは議事録だけで、差し替えると再生中の音が止まって
        // 先頭へ戻る（`PlaybackLoad`）。
        let generation_id = advance_load_generation(&runner.load_generation);
        spawn_session_load(
            session,
            generation_id,
            &runner.load_sender,
            load_replaces_playback(false),
        );
    }
}

/// `summary.md` を Summary タブの表示行へ分ける（**Markdown をどこまで解釈するかの正はここ**。
/// `ui/library-window.slint` の `SummaryRow` はこの doc を参照する）。
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
fn apply_detail_summary_status(rec: &LibraryWindow, pane: &SummaryPane, jobs_pending: bool) {
    let status = pane.status();
    let message = pane.message();
    rec.set_detail_summary_status_text(summary_status_text(status).into());
    rec.set_detail_summary_heading(message.heading.into());
    rec.set_detail_summary_body(message.body.into());
    set_pane_actions(
        rec.get_detail_summary_actions(),
        &actions_allowed_while_busy(message.actions, jobs_pending),
        |actions| rec.set_detail_summary_actions(actions),
    );
    rec.set_detail_summary_status(slint_map::to_ui_summary_status(status));
}

/// どの状態に落とすかを決める純関数。**ワーカーの記録が先、無ければ `summary.md` の有無**
/// （文字起こし側の `shoki_core::view_detail` と同じ流儀）。
///
/// 議事録側が core へ移るのは段階 03（`docs/plans/done/20260829-core-shell-layers.md`）。
/// それまではここが正。
fn summary_pane_of(
    state: Option<summarize::SummarizeState>,
    has_summary: bool,
    input: TranscriptInput,
    auto_on: bool,
) -> SummaryPane {
    match state {
        Some(summarize::SummarizeState::Queued { position, .. }) => {
            SummaryPane::Queued { position }
        }
        Some(summarize::SummarizeState::Summarizing {
            model_label,
            elapsed,
        }) => SummaryPane::Summarizing {
            model: model_label,
            started_ago: elapsed_text(elapsed),
        },
        Some(summarize::SummarizeState::Done) => SummaryPane::Done,
        Some(summarize::SummarizeState::Failed { reason }) => SummaryPane::Failed { reason },
        None if has_summary => SummaryPane::Done,
        // 文字起こしが無いと議事録は動かせない。**「まだ書いていない」ではなく「まだ書けない」**
        // と言い分けるのは、押しても何も起きないボタンを出さないため。無いときは、なぜ無いのか
        // で 3 つに割れる（#165。待っている／失敗した／まだ何もしていない）。
        // 入力の様子で 3 つに割れる（#165。対応表の正は `TranscriptInput::pane_when_no_notes`
        // ——確認用バイナリも同じところを通す）。
        None => input.pane_when_no_notes(auto_on),
    }
}

/// 議事録が出来上がっているときに一覧の下へ出す出典（誰がいつ書いたか）。
///
/// **どのモデルが書いたかはファイルに残っていない**（`summary.md` は本文だけ）ので、ここでは
/// 書かれた時刻だけを言う。時刻はファイルの更新時刻から取る——生成のたびに置き換わるので、
/// 「この議事録がいつのものか」と一致する。読めなければ段ごと出さない。
fn summary_footer_text(written: Option<SystemTime>) -> String {
    let Some(written) = written else {
        return String::new();
    };
    // **`DateTime::from(SystemTime)` を使わない**。chrono の実装は範囲外の時刻で `unwrap` する
    // ので、外（ファイルシステム・同期ツール・手編集）から来た更新時刻でアプリごと落ちる。
    // 読めなければ段ごと出さない。
    let Ok(since_epoch) = written.duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    let Ok(seconds) = i64::try_from(since_epoch.as_secs()) else {
        return String::new();
    };
    let Some(written) = chrono::DateTime::from_timestamp(seconds, since_epoch.subsec_nanos())
    else {
        return String::new();
    };
    // 日時の形は一覧・詳細ヘッダと揃える（左右で違うと同じ録音を見ていることが読み取りにくい）。
    format!(
        "Written from the transcript · {}",
        written
            .with_timezone(&chrono::Local)
            .format(shoki_core::DISPLAY_DATETIME_FORMAT)
    )
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
///   初回要約、または Library ウィンドウの「Summarize」による手動生成）に `ensure_model` が行う。
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
///   初回要約か、Library ウィンドウからの手動生成）。
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
    use shoki_core::{
        FailedSource, KeptFromSource, SummarizeFailure, TranscribeFailure, TranscriptShortfall,
        summarize_failure_text, transcribe_failure_text, transcript_status_text,
    };
    // **状態の語彙は core を指す**（#188）。同名の Slint 生成型はテストでは使わない——
    // 使うなら `slint_map` を通す。
    use super::{
        StatusTone, SummaryPane, TranscriptPane, actions_allowed_while_busy, app_version_text,
        breathing_level, model_downloads_on_select, model_status_line, not_downloaded_count,
        outcome_of, playback_progress, seek_position_from_ratio, summary_model_status_line,
        summary_pane_of, summary_rows, summary_status_text, whisper_model_status_line,
    };
    use super::{elapsed_text, recordings, summarize};
    use chrono::{Datelike as _, Timelike as _};
    use shoki_core::{PaneAction, PaneActionKind, SummaryStatus, TranscriptStatus};

    use std::time::Duration;

    /// 時計表記は `mm:ss`、1 時間以上だけ `h:mm:ss`（#164 で `tray` から
    /// `shoki_core::format_elapsed` へ寄せた。実装と同じ場所で押さえる）。
    #[test]
    fn elapsed_is_shown_as_a_clock() {
        use shoki_core::format_elapsed;

        assert_eq!(format_elapsed(Duration::from_secs(0)), "00:00");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "01:05");
        assert_eq!(format_elapsed(Duration::from_secs(599)), "09:59");
        // 1 時間未満の上限。ここまでは時を出さず mm:ss のまま（分は 60 以上になりうる）。
        assert_eq!(format_elapsed(Duration::from_secs(3599)), "59:59");
        // 分は 2 桁ゼロ詰め、時は詰めない。
        assert_eq!(format_elapsed(Duration::from_secs(3600)), "1:00:00");
        assert_eq!(format_elapsed(Duration::from_secs(3661)), "1:01:01");
    }

    /// 途中結果かどうかが、状態と**同じ値から**ウィンドウへ届くこと（#164）。
    ///
    /// 決めるのは Rust（`TranscriptPane::shows_partial`）、伏せるのは Slint
    /// （`detail-transcript-held-back`）で、どちらも単体では検査済み。**繋いでいるのはこの
    /// setter だけ**なので、ここが抜けると両側が緑のまま途中結果が完成品として出る
    /// （`docs/rules/testing.md` の「配線は、繋いでいる関数に継ぎ目を入れてテストする」）。
    #[test]
    fn the_pane_tells_the_window_whether_what_is_readable_is_partial() {
        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        // `apply_detail_transcript_status` が受けるのは core が組んだ `DetailView`。ここで
        // 見たいのは「ペインの値が画面へどう入るか」なので、状態から素直に組む。
        let view = |transcript: TranscriptPane| {
            let actions = transcript.message().actions;
            shoki_core::DetailView {
                transcript_input: shoki_core::TranscriptInput::Missing,
                transcript_busy: false,
                actions,
                loading: false,
                transcript,
            }
        };

        let partial = TranscriptPane::Failed {
            reason: TranscribeFailure::Files {
                failed: vec![FailedSource::new(
                    "mic.mp3",
                    KeptFromSource::Upto(Duration::from_secs(252)),
                )],
                kept_other_sources: false,
            },
        };
        super::apply_detail_transcript_status(&rec, &view(partial.clone()), false);
        assert!(rec.get_detail_transcript_partial());

        // 前回の完成した文字起こしが残っているだけの失敗では伏せない。
        let nothing_kept = TranscriptPane::Failed {
            reason: TranscribeFailure::ModelLoad,
        };
        super::apply_detail_transcript_status(&rec, &view(nothing_kept), false);
        assert!(!rec.get_detail_transcript_partial());

        // 走り終わった文字起こしも同じ（伏せる理由が無い）。
        super::apply_detail_transcript_status(&rec, &view(TranscriptPane::Done), false);
        assert!(!rec.get_detail_transcript_partial());

        // **ディスクの印から立つ途中結果も伏せる**（#175）。ここが緩むと、再起動後に欠けた
        // 文字起こしが完成品として読める。
        super::apply_detail_transcript_status(
            &rec,
            &view(TranscriptPane::NotWhole {
                shortfall: TranscriptShortfall::StopsPartway,
            }),
            false,
        );
        assert!(rec.get_detail_transcript_partial());
        // **状態行も同じ値から出す**。ここだけ `transcript_status_text(pane.status())` に
        // 戻すと、途中結果を伏せたまま「Transcribed」と言う画面になる——`status()` は
        // `NotWhole` を `Done` へ畳むので、この一言でしか差が出ない。
        assert_eq!(rec.get_detail_transcript_text(), "Transcribed in part");

        // 走り始めたら、開いた途中結果は畳む（理由は `fold_partial_transcript` の doc）。
        super::apply_detail_transcript_status(&rec, &view(partial.clone()), false);
        rec.set_show_partial_transcript(true);
        super::apply_detail_transcript_status(
            &rec,
            &view(TranscriptPane::Transcribing {
                model: "Medium".to_owned(),
                percent: None,
            }),
            true,
        );
        assert!(!rec.get_show_partial_transcript());
    }

    /// 世代を進めたら、**走っている走査へ必ず伝わる**こと（#181）。
    ///
    /// 番号を進めるのと伝えるのは `advance_scan_generation` の中で 1 つになっているが、
    /// **伝える側だけを消しても、届いた結果を捨てる仕組みは動き続ける**——古い結果は捨てられる
    /// ので画面は正しいまま、走っているスレッドだけが降りずに走り切る。連打したぶんだけ
    /// 走査が積み上がり、いま見たい一覧を自分で遅くする（`spawn_session_load` の doc）。
    /// 症状が画面に出ないので、ここで留めておかないと気づけない。
    #[test]
    fn advancing_the_scan_generation_tells_the_running_scans_to_stand_down() {
        use std::cell::Cell;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let generation = Cell::new(3u64);
        // 走っている走査が持つ手（`spawn_session_scan` が登録するのと同じもの）。
        let running = Arc::new(AtomicU64::new(3));
        // すでに終わったスレッドの手（`Vec` だけが持っている＝相手がいない）。落ちること。
        let finished = Arc::new(AtomicU64::new(3));
        super::SCAN_WATCHERS.with(|watchers| {
            let mut watchers = watchers.borrow_mut();
            watchers.clear();
            watchers.push(Arc::clone(&running));
            watchers.push(finished);
        });

        let next = super::advance_scan_generation(&generation);

        assert_eq!(next, 4);
        assert_eq!(generation.get(), 4);
        assert_eq!(
            running.load(Ordering::Relaxed),
            4,
            "a running scan must be told to stand down"
        );
        super::SCAN_WATCHERS.with(|watchers| {
            assert_eq!(
                watchers.borrow().len(),
                1,
                "the hand of a thread that already finished is dropped"
            );
        });
    }

    /// 走査結果が**一覧まで届き、古い世代は捨てられる**こと（#181）。
    ///
    /// 走査を別スレッドへ出した以上、結果を反映する繋ぎは本番だけが通る 1 本になる。ここを
    /// 通らないと、`set_vec` の行を消しても、世代の照合を外しても全部緑のままになる
    /// （`docs/rules/testing.md` の「繋いでいる関数は、呼べるなら丸ごと呼ぶ」）。
    ///
    /// ワーカーは立てるがジョブは投げない（走らせた記録が無い＝ディスクの有無だけが効く場面）。
    #[test]
    fn a_scan_result_reaches_the_list_and_stale_ones_are_dropped() {
        use std::cell::{Cell, RefCell};

        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        let summarizer = super::summarize::SummarizeWorker::start(
            super::model_download::ModelDownloader::new(),
            super::inference_slot::InferenceSlot::new(),
        );
        // ワーカーは立てるが読まない（表示の状態は `AppState` から出す。#188）。
        let _transcriber = super::transcribe::TranscribeWorker::start(
            super::model_download::ModelDownloader::new(),
            summarizer,
            super::inference_slot::InferenceSlot::new(),
        );
        let model = super::SessionRows::new();
        let all = RefCell::new(Vec::new());
        let shown = RefCell::new(Vec::new());
        let generation = Cell::new(7u64);
        let scan_state = Cell::new(super::ScanState::Awaiting);
        let app_state = RefCell::new(shoki_core::AppState::default());
        let apply = |scanned| {
            super::apply_scanned_sessions(
                &rec,
                &model,
                super::SessionLists {
                    all: &all,
                    shown: &shown,
                },
                &generation,
                &scan_state,
                &app_state,
                scanned,
            );
        };
        let sessions = |count: usize| -> Vec<recordings::RecordingSession> {
            (0..count)
                .map(|i| {
                    let mut session = recordings::session_for_test(
                        chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                            .expect("a real date")
                            .and_hms_opt(14, 2, 0)
                            .expect("a real time"),
                    );
                    session.dir = std::path::PathBuf::from(format!("20260810-14020{i}"));
                    session.has_mic = true;
                    session
                })
                .collect()
        };

        // 開いた直後の画面を作っておく（`open_library_window` と同じ）。古い結果がここを
        // 書き換えないことを見たいので、先に走査中の文言を入れる。
        super::apply_list_counts(
            &rec,
            super::ListCounts {
                shown: 0,
                total: 0,
                not_downloaded: 0,
            },
            &scan_state,
        );
        assert_eq!(rec.get_empty_heading(), "Looking for recordings…");

        // **古い世代は捨てる**。閉じた・閉じて開き直した走査が後から届いても、一覧を
        // 書き換えないし、走査中という状態も動かさない。
        apply(super::ScannedSessions {
            generation: 6,
            outcome: super::ScanOutcome::Scanned(sessions(3)),
        });
        {
            assert_eq!(model.row_count(), 0, "a stale scan must not fill the list");
        }
        assert!(all.borrow().is_empty());
        assert!(shown.borrow().is_empty());
        assert_eq!(
            scan_state.get(),
            super::ScanState::Awaiting,
            "a stale scan must not say the scan has landed"
        );
        assert_eq!(rec.get_empty_heading(), "Looking for recordings…");

        // いまの世代なら、行・件数・2 つの一覧のすべてが揃う。
        apply(super::ScannedSessions {
            generation: 7,
            outcome: super::ScanOutcome::Scanned(sessions(2)),
        });
        {
            assert_eq!(model.row_count(), 2);
        }
        // **両方の一覧へ入れる**（#161）。片方だけだと、検索を解除したときに食い違う。
        assert_eq!(all.borrow().len(), 2);
        assert_eq!(shown.borrow().len(), 2);
        assert_eq!(rec.get_library_summary(), "2 recordings");
        assert_eq!(scan_state.get(), super::ScanState::Settled);

        // **0 件で着地したら「録音が無い」へ変わる**（#181）。走査中の文言が残ると、空の保存先を
        // 開いた人は永久に「探している」画面を見る。
        generation.set(8);
        scan_state.set(super::ScanState::Awaiting);
        apply(super::ScannedSessions {
            generation: 8,
            outcome: super::ScanOutcome::Scanned(Vec::new()),
        });
        assert_eq!(rec.get_empty_heading(), "No recordings yet");

        // **走れなかったときは「録音が無い」と言わない**（#181）。1 件も見ていないので、
        // 空の結果として扱うと嘘になる。
        generation.set(9);
        apply(super::ScannedSessions {
            generation: 9,
            outcome: super::ScanOutcome::CouldNotStart,
        });
        assert_eq!(scan_state.get(), super::ScanState::CouldNotStart);
        assert_eq!(rec.get_empty_heading(), "Could not look for recordings");
    }

    /// トレイから開いたとき、**窓は待たずに出て、走査中だと言う**こと（#181）。
    ///
    /// **`open_library_window` を丸ごと呼ぶ**（`docs/rules/testing.md` の「『重そうだから
    /// 呼べない』は確かめてから言う」）。ここを通らないと、走査中であることを立てる 1 行を
    /// 消しても、前の一覧を空にする 1 行を消しても全部緑のまま通る——どちらも #181 が直した
    /// バグ（「録音が無い」と言う嘘／前の保存先の録音が押せる）がそのまま戻る。
    #[test]
    fn opening_the_library_says_it_is_still_looking() {
        use std::cell::{Cell, RefCell};

        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        let summarizer = super::summarize::SummarizeWorker::start(
            super::model_download::ModelDownloader::new(),
            super::inference_slot::InferenceSlot::new(),
        );
        let transcriber = super::transcribe::TranscribeWorker::start(
            super::model_download::ModelDownloader::new(),
            summarizer.clone(),
            super::inference_slot::InferenceSlot::new(),
        );
        let (load_sender, load_receiver) = std::sync::mpsc::channel();
        let runner_load_sender = load_sender.clone();
        let (scan_sender, scan_receiver) = std::sync::mpsc::channel();
        let segments = std::rc::Rc::new(RefCell::new(super::LoadedTranscript::unknown()));
        let (_search_sender, search_receiver) = std::sync::mpsc::channel();
        let sessions_model = std::rc::Rc::new(super::SessionRows::new());
        // **前に開いたときの状態**を作っておく。開き直したらこれが残っていないことを見る。
        let stale = {
            let mut session = recordings::session_for_test(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                    .expect("a real date")
                    .and_hms_opt(14, 2, 0)
                    .expect("a real time"),
            );
            session.dir = std::path::PathBuf::from("20260810-140200");
            session.has_mic = true;
            session
        };
        let app_state: std::rc::Rc<RefCell<shoki_core::AppState>> =
            std::rc::Rc::new(RefCell::new(shoki_core::AppState::default()));
        sessions_model.replace_all(std::slice::from_ref(&stale), &app_state.borrow());
        rec.set_search_text("release".into());

        // 走査の相手には**録音を 1 件置く**。走査スレッドがこの保存先を実際に読んだことは、
        // 届いた結果の中身でしか分からない——ディレクトリが無くても `scan_sessions` は空一覧を
        // 返すし、スレッドが立たなかったときも UI スレッドから同期で結果が届く。
        //
        // **プロセスごとに別の名前にし、先に消す**（既存のテストと同じ作法）。走査スレッドは
        // 一時ファイルの回収まで通るので、他が置いたものを拾える固定パスは使わない。
        let recording_dir =
            std::env::temp_dir().join(format!("shoki-open-library-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&recording_dir);
        let session_dir = recording_dir.join("20260810-140200");
        std::fs::create_dir_all(&session_dir).expect("make a recordings folder with one session");
        std::fs::write(session_dir.join("mic.mp3"), b"").expect("write the mic source");
        let config = std::rc::Rc::new(RefCell::new(super::Config {
            recording_dir: recording_dir.clone(),
            ..super::Config::default()
        }));

        let handles = super::LibraryHandles {
            ui: slint::ComponentHandle::as_weak(&rec),
            player: std::rc::Rc::new(RefCell::new(None)),
            load_receiver,
            sessions: std::rc::Rc::new(RefCell::new(vec![stale.clone()])),
            all_sessions: std::rc::Rc::new(RefCell::new(vec![stale])),
            scan_receiver,
            scan_sender,
            scan_generation: std::rc::Rc::new(Cell::new(0)),
            scan_state: std::rc::Rc::new(Cell::new(super::ScanState::Settled)),
            search_receiver,
            search_generation: std::rc::Rc::new(Cell::new(0)),
            search_not_downloaded: std::rc::Rc::new(RefCell::new(Vec::new())),
            sessions_model: std::rc::Rc::clone(&sessions_model),
            transcript_segments: std::rc::Rc::clone(&segments),
            transcriber,
            summarizer,
            config: std::rc::Rc::clone(&config),
            state: std::rc::Rc::clone(&app_state),
            runner: super::EffectRunner {
                ui: slint::ComponentHandle::as_weak(&rec),
                state: std::rc::Rc::clone(&app_state),
                segments: std::rc::Rc::clone(&segments),
                sessions: std::rc::Rc::new(RefCell::new(Vec::new())),
                player: std::rc::Rc::new(RefCell::new(None)),
                load_generation: std::rc::Rc::new(Cell::new(0)),
                load_sender: runner_load_sender,
            },
        };

        let mut geometry_committed = false;
        let mut last_play_secs = None;
        super::open_library_window(
            &rec,
            &handles,
            &config,
            &mut geometry_committed,
            &mut last_play_secs,
        );

        // **走査中だと言う**。0 件のまま「録音が無い」と言うと、遅い保存先で開いた人は
        // 録音を失ったと思う。
        assert_eq!(handles.scan_state.get(), super::ScanState::Awaiting);
        assert_eq!(rec.get_empty_heading(), "Looking for recordings…");
        // **前の一覧は残さない**。残すと、保存先を変えてから開き直した直後に前の保存先の
        // 録音が並び、押せてしまう。
        {
            assert_eq!(sessions_model.row_count(), 0);
        }
        assert!(handles.all_sessions.borrow().is_empty());
        assert!(handles.sessions.borrow().is_empty());
        // 絞り込みも持ち越さない（残ると、録音が消えたように見える）。
        assert_eq!(rec.get_search_text(), "");
        // **走査が実際に飛んだところまで見る**。世代だけを見ると、`spawn_session_scan` の
        // 呼び出しを消しても通る——症状は「`Looking for recordings…` のまま永久に埋まらない」で、
        // #181 が作った状態のうちいちばん出してはいけない画面。
        assert_eq!(handles.scan_generation.get(), 1);
        let scanned = handles
            .scan_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the scan must actually be spawned and report back");
        assert_eq!(scanned.generation, 1);
        // **中身まで見る**。世代と「何か届いた」だけだと、スレッドが立たなかったとき
        // （`ScanOutcome::CouldNotStart` を UI スレッドから同期で送る）と区別できない。
        match scanned.outcome {
            super::ScanOutcome::Scanned(sessions) => {
                assert_eq!(sessions.len(), 1, "the scan must have read this folder");
                assert_eq!(sessions[0].dir, session_dir);
            }
            super::ScanOutcome::CouldNotStart => panic!("the scan thread must start"),
        }
        let _ = std::fs::remove_dir_all(&recording_dir);
    }

    /// 削除で行が繰り上がっても、**行とキーがずれない**こと（#188）。
    ///
    /// キーは位置で引くので、`remove` で片方だけ詰めると以降の行が全部 1 つずれる。ずれた行は
    /// 「変わっていない」と判定され続けて**二度と更新されない**——症状は「文字起こしが終わった
    /// のに一覧の行が transcribing のまま」で、選び直しても直らない（一覧を開き直すまで）。
    #[test]
    fn removing_a_row_keeps_the_keys_lined_up() {
        use std::cell::RefCell;

        super::init_test_backend();
        let session = |dir: &str, hour: u32| {
            let mut session = recordings::session_for_test(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                    .expect("a real date")
                    .and_hms_opt(hour, 2, 0)
                    .expect("a real time"),
            );
            session.dir = std::path::PathBuf::from(dir);
            session.has_mic = true;
            session
        };
        let list = vec![session("a", 14), session("b", 9)];
        let state = RefCell::new(shoki_core::AppState::default());
        let rows = super::SessionRows::new();
        rows.replace_all(&list, &state.borrow());

        // 先頭を消す。残った "b" は 0 番へ繰り上がる。
        rows.remove(0);
        let list = [session("b", 9)];
        assert_eq!(rows.row_count(), 1);

        // "b" の文字起こしが終わった。**繰り上がった行も更新される**。
        {
            let mut state = state.borrow_mut();
            shoki_core::update(
                &mut state,
                shoki_core::Msg::Event(shoki_core::Event::JobChanged {
                    dir: std::path::PathBuf::from("b"),
                    job: Some(shoki_core::Job {
                        id: shoki_core::JobId(1),
                        phase: shoki_core::JobPhase::Done { shortfall: None },
                    }),
                }),
            );
        }
        rows.refresh(0, &list[0], &state.borrow());
        {
            use slint::Model as _;
            let row = rows.model().row_data(0).expect("the row is there");
            assert_eq!(
                row.transcript_status,
                super::slint_map::to_ui_transcript_status(shoki_core::TranscriptStatus::Done),
                "the row that moved up must still follow its own session"
            );
        }
    }

    /// 議事録が完成したら、**本文を読み直す**こと（#188）。
    ///
    /// 遷移を観測できるのは 1 回だけ（`refresh_detail_panes` がすぐ上書きする）。呼び出し側へ
    /// 返して「立っていたら読み直す」を守らせると、押した瞬間の再描画が最初の観測者になった
    /// ときに消える——ワーカーは別スレッドなので、tick が最初とは限らない。だから読み直しも
    /// この関数の中でやる。ここが落ちると、出来上がった議事録が画面に出ない
    /// （`summary_rows` を書くのは `apply_loaded_session` だけ）。
    #[test]
    fn a_summary_that_just_finished_is_read_back() {
        use std::cell::RefCell;

        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        let summarizer = super::summarize::SummarizeWorker::start(
            super::model_download::ModelDownloader::new(),
            super::inference_slot::InferenceSlot::new(),
        );
        let mut session = recordings::session_for_test(
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                .expect("a real date")
                .and_hms_opt(14, 2, 0)
                .expect("a real time"),
        );
        session.dir = std::env::temp_dir().join(format!("shoki-summary-{}", std::process::id()));
        session.has_mic = true;
        session.has_transcript = true;
        session.has_summary = true;

        let (load_sender, load_receiver) = std::sync::mpsc::channel();
        let runner = super::EffectRunner {
            ui: slint::ComponentHandle::as_weak(&rec),
            state: std::rc::Rc::new(RefCell::new(shoki_core::AppState::default())),
            segments: std::rc::Rc::new(RefCell::new(super::LoadedTranscript::unknown())),
            sessions: std::rc::Rc::new(RefCell::new(Vec::new())),
            player: std::rc::Rc::new(RefCell::new(None)),
            load_generation: std::rc::Rc::new(std::cell::Cell::new(0)),
            load_sender,
        };
        let config = RefCell::new(super::Config::default());

        // 画面は「生成中」を出していて、ワーカーは走り終わった記録を持っている。
        rec.set_detail_summary_status(super::slint_map::to_ui_summary_status(
            shoki_core::SummaryStatus::Summarizing,
        ));
        summarizer.mark_done_for_test(&session.dir);
        super::refresh_detail_panes(&rec, &runner, &summarizer, &session, &config);
        let loaded = load_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("the notes that just finished must be read back");
        assert_eq!(loaded.dir, session.dir);

        // **もう一度呼んでも読み直さない**。毎 tick 読み直すことになる。
        super::refresh_detail_panes(&rec, &runner, &summarizer, &session, &config);
        assert!(
            load_receiver
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "the transition fires once"
        );
    }

    /// ワーカーの**記録が消えただけ**では読み直さないこと（#188）。
    ///
    /// ペインの `Done` は `summary.md` の有無からも立つ。取り消し（`cancel`）と、入力が無くて
    /// 飛ばした場合（`Skipped`）は記録ごと消えるので、既に議事録が在る録音では
    /// 「キュー待ち → 生成済み」に見える。そこで読み直すと世代が繰り上がり、**選択直後に投げた
    /// 音声つきの読み込みが降りて**、選んだ録音が再生できないまま残る（同じ行を選び直しても
    /// 音は差し替えないので直らない）。
    #[test]
    fn a_cancelled_summary_does_not_look_like_it_finished() {
        use std::cell::RefCell;

        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        let summarizer = super::summarize::SummarizeWorker::start(
            super::model_download::ModelDownloader::new(),
            super::inference_slot::InferenceSlot::new(),
        );
        let mut session = recordings::session_for_test(
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                .expect("a real date")
                .and_hms_opt(14, 2, 0)
                .expect("a real time"),
        );
        session.dir = std::path::PathBuf::from("20260810-140200");
        session.has_mic = true;
        session.has_transcript = true;
        // **議事録は既に在る**（取り消しても `summary.md` の有無で `Done` に見える）。
        session.has_summary = true;

        let (load_sender, _load_receiver) = std::sync::mpsc::channel();
        let runner = super::EffectRunner {
            ui: slint::ComponentHandle::as_weak(&rec),
            state: std::rc::Rc::new(RefCell::new(shoki_core::AppState::default())),
            segments: std::rc::Rc::new(RefCell::new(super::LoadedTranscript::unknown())),
            sessions: std::rc::Rc::new(RefCell::new(vec![session.clone()])),
            player: std::rc::Rc::new(RefCell::new(None)),
            load_generation: std::rc::Rc::new(std::cell::Cell::new(3)),
            load_sender,
        };
        let config = RefCell::new(super::Config::default());

        // 画面はキュー待ちを出していて、ワーカーの記録は（取り消しで）もう無い。
        rec.set_detail_summary_status(super::slint_map::to_ui_summary_status(
            shoki_core::SummaryStatus::Queued,
        ));
        assert!(summarizer.state_of(&session.dir).is_none());

        super::refresh_detail_panes(&rec, &runner, &summarizer, &session, &config);
        assert_eq!(
            runner.load_generation.get(),
            3,
            "a record that only disappeared must not stand down a load"
        );
    }

    /// 別の録音を選んだときに、**偽の遷移で読み直さない**こと（#188）。
    ///
    /// 「前の議事録の状態」はウィンドウのプロパティから取るが、あれは**どの録音のものかを
    /// 持たない**。畳まないと「A の生成中 → B の生成済み」を遷移と読み、世代を進めて B の
    /// 音声つき読み込みを降ろす——**選んだ録音が再生できないまま残る**（同じ行を選び直しても
    /// 音は差し替えないので直らない）。
    #[test]
    fn choosing_another_recording_does_not_look_like_finished_notes() {
        use std::cell::RefCell;

        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        let summarizer = super::summarize::SummarizeWorker::start(
            super::model_download::ModelDownloader::new(),
            super::inference_slot::InferenceSlot::new(),
        );
        let session = |dir: &str, has_summary: bool| {
            let mut session = recordings::session_for_test(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                    .expect("a real date")
                    .and_hms_opt(14, 2, 0)
                    .expect("a real time"),
            );
            session.dir = std::path::PathBuf::from(dir);
            session.has_mic = true;
            session.has_transcript = true;
            session.has_summary = has_summary;
            session
        };
        // B は議事録が出来ている（ワーカーの記録は無いので `summary.md` の有無で `Done`）。
        let b = session("b", true);
        let (load_sender, _load_receiver) = std::sync::mpsc::channel();
        let runner = super::EffectRunner {
            ui: slint::ComponentHandle::as_weak(&rec),
            state: std::rc::Rc::new(RefCell::new(shoki_core::AppState::default())),
            segments: std::rc::Rc::new(RefCell::new(super::LoadedTranscript::unknown())),
            sessions: std::rc::Rc::new(RefCell::new(vec![session("a", false), b.clone()])),
            player: std::rc::Rc::new(RefCell::new(None)),
            load_generation: std::rc::Rc::new(std::cell::Cell::new(0)),
            load_sender,
        };
        let config = RefCell::new(super::Config::default());

        // A を選んでいて、その議事録は生成中。
        {
            let mut state = runner.state.borrow_mut();
            let _ = shoki_core::update(
                &mut state,
                shoki_core::Msg::Command(shoki_core::Command::Select(Some(
                    std::path::PathBuf::from("a"),
                ))),
            );
        }
        rec.set_detail_summary_status(super::slint_map::to_ui_summary_status(
            shoki_core::SummaryStatus::Summarizing,
        ));

        // B を選ぶ（本番と同じ順序——`update` の依頼を実行してから組み直す）。
        let effects = {
            let mut state = runner.state.borrow_mut();
            shoki_core::update(
                &mut state,
                shoki_core::Msg::Command(shoki_core::Command::Select(Some(b.dir.clone()))),
            )
        };
        super::run_effects(&runner, effects, None);
        let after_select = runner.load_generation.get();
        super::refresh_detail_panes(&rec, &runner, &summarizer, &b, &config);
        assert_eq!(
            runner.load_generation.get(),
            after_select,
            "picking another recording must not stand down the load it just started"
        );
    }

    /// 押す直前にワーカーが終わっていたら、**その場で読み直す**こと（#188）。
    ///
    /// `observe_jobs` は `AppState.jobs` をワーカーへ揃えてしまうので、返った依頼を捨てると
    /// **次の tick の差分は 1 件も立たない**——完成した発話が画面に出ないまま残る（`summary_rows`
    /// と `segments` を書くのは `apply_loaded_session` だけ）。tick が最初の観測者とは限らない。
    #[test]
    fn a_job_that_finished_just_before_a_press_is_read_back() {
        use std::cell::RefCell;

        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        let summarizer = super::summarize::SummarizeWorker::start(
            super::model_download::ModelDownloader::new(),
            super::inference_slot::InferenceSlot::new(),
        );
        let transcriber = super::transcribe::TranscribeWorker::start(
            super::model_download::ModelDownloader::new(),
            summarizer,
            super::inference_slot::InferenceSlot::new(),
        );
        let mut session = recordings::session_for_test(
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                .expect("a real date")
                .and_hms_opt(14, 2, 0)
                .expect("a real time"),
        );
        session.dir = std::env::temp_dir().join(format!("shoki-press-{}", std::process::id()));
        session.has_mic = true;

        let (load_sender, load_receiver) = std::sync::mpsc::channel();
        let runner = super::EffectRunner {
            ui: slint::ComponentHandle::as_weak(&rec),
            state: std::rc::Rc::new(RefCell::new(shoki_core::AppState::default())),
            segments: std::rc::Rc::new(RefCell::new(super::LoadedTranscript::unknown())),
            sessions: std::rc::Rc::new(RefCell::new(vec![session.clone()])),
            player: std::rc::Rc::new(RefCell::new(None)),
            load_generation: std::rc::Rc::new(std::cell::Cell::new(0)),
            load_sender,
        };

        // この録音を選んでいて、文字起こしが走っている。
        {
            let mut state = runner.state.borrow_mut();
            let _ = shoki_core::update(
                &mut state,
                shoki_core::Msg::Command(shoki_core::Command::Select(Some(session.dir.clone()))),
            );
        }
        // 選択が投げた読み込みは、ここでは見ない。
        let _ = load_receiver.recv_timeout(std::time::Duration::from_secs(5));
        transcriber.mark_running_for_test(&session.dir, "Medium");
        super::observe_jobs(&transcriber, &runner);
        assert!(
            load_receiver
                .recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "starting a job does not reload"
        );

        // ワーカーが降りた（押す直前に完了した、と同じ形）。
        transcriber.forget(&session.dir);
        super::observe_jobs(&transcriber, &runner);
        let loaded = load_receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("coming off the worker must read the recording back");
        assert_eq!(loaded.dir, session.dir);
    }

    /// **ワーカーの様子が core を通って画面まで届く**こと（#188）。
    ///
    /// `job_changes` → `update` → `view_row` / `view_detail` が繋がっていないと、文字起こしを
    /// 走らせても一覧の行も読む領域も動かない。部品はどれも単体で検査済みだが、**繋いでいるのは
    /// この経路だけ**なので、ここを通らないと `job_changes` を空 `Vec` にしても全部緑のまま通る
    /// （`docs/rules/testing.md` の「繋いでいる関数は、呼べるなら丸ごと呼ぶ」）。
    #[test]
    fn what_the_worker_is_doing_reaches_the_row_and_the_pane() {
        use std::cell::RefCell;

        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        let summarizer = super::summarize::SummarizeWorker::start(
            super::model_download::ModelDownloader::new(),
            super::inference_slot::InferenceSlot::new(),
        );
        let transcriber = super::transcribe::TranscribeWorker::start(
            super::model_download::ModelDownloader::new(),
            summarizer.clone(),
            super::inference_slot::InferenceSlot::new(),
        );
        let mut session = recordings::session_for_test(
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                .expect("a real date")
                .and_hms_opt(14, 2, 0)
                .expect("a real time"),
        );
        session.dir = std::path::PathBuf::from("20260810-140200");
        session.has_mic = true;

        let state = std::rc::Rc::new(RefCell::new(shoki_core::AppState::default()));
        let config = RefCell::new(super::Config::default());
        let (load_sender, _load_receiver) = std::sync::mpsc::channel();
        let runner = super::EffectRunner {
            ui: slint::ComponentHandle::as_weak(&rec),
            state: std::rc::Rc::clone(&state),
            segments: std::rc::Rc::new(RefCell::new(super::LoadedTranscript::unknown())),
            sessions: std::rc::Rc::new(RefCell::new(Vec::new())),
            player: std::rc::Rc::new(RefCell::new(None)),
            load_generation: std::rc::Rc::new(std::cell::Cell::new(0)),
            load_sender,
        };
        let rows = super::SessionRows::new();
        rows.replace_all(std::slice::from_ref(&session), &state.borrow());

        // まだ走らせていない。
        super::refresh_detail_panes(&rec, &runner, &summarizer, &session, &config);
        assert_eq!(
            rec.get_detail_transcript_status(),
            super::slint_map::to_ui_transcript_status(shoki_core::TranscriptStatus::NotTranscribed)
        );

        // ワーカーへ直接エントリを置き、tick と同じ道（差分 → `update`）を通す。
        transcriber.mark_running_for_test(&session.dir, "Medium");
        let effects = {
            let mut state = state.borrow_mut();
            super::job_changes(&transcriber, &state)
                .into_iter()
                .flat_map(|msg| shoki_core::update(&mut state, msg))
                .collect::<Vec<_>>()
        };
        assert!(
            effects.is_empty(),
            "starting a job does not reload anything by itself"
        );

        // 行にも読む領域にも届く。
        rows.refresh(0, &session, &state.borrow());
        {
            use slint::Model as _;
            let row = rows.model().row_data(0).expect("the row is there");
            assert_eq!(
                row.transcript_status,
                super::slint_map::to_ui_transcript_status(
                    shoki_core::TranscriptStatus::Transcribing
                )
            );
            assert_eq!(row.detail_text, "Mic only · transcribing");
        }
        super::refresh_detail_panes(&rec, &runner, &summarizer, &session, &config);
        assert_eq!(rec.get_detail_transcript_text(), "Transcribing…");
    }

    /// 走査中に打たれた検索が、**走査の着地で黙って捨てられない**こと（#181）。
    ///
    /// 走査を別スレッドへ出したことで「一覧が空のまま検索欄だけ生きている」時間ができた。
    /// そこで打つと空の全件を舐めるので必ず 0 件になり、そのあと走査が着地すると全件が並ぶ
    /// ——**検索語が残ったまま絞り込みが消えた**画面になる。着地したら投げ直すのが正。
    #[test]
    fn a_search_typed_while_scanning_is_run_again_when_the_scan_lands() {
        use std::cell::{Cell, RefCell};

        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        let summarizer = super::summarize::SummarizeWorker::start(
            super::model_download::ModelDownloader::new(),
            super::inference_slot::InferenceSlot::new(),
        );
        // ワーカーは立てるが読まない（表示の状態は `AppState` から出す。#188）。
        let _transcriber = super::transcribe::TranscribeWorker::start(
            super::model_download::ModelDownloader::new(),
            summarizer,
            super::inference_slot::InferenceSlot::new(),
        );
        let model = super::SessionRows::new();
        let all = RefCell::new(Vec::new());
        let shown = RefCell::new(Vec::new());
        let generation = Cell::new(1u64);
        let scan_state = Cell::new(super::ScanState::Awaiting);
        let app_state = RefCell::new(shoki_core::AppState::default());

        // 本番の入口（`on_search`）の代わりに、投げ直されたことだけを控える。
        let asked = std::rc::Rc::new(RefCell::new(Vec::<String>::new()));
        {
            let asked = std::rc::Rc::clone(&asked);
            rec.on_search(move |needle| asked.borrow_mut().push(needle.to_string()));
        }

        // 走査中に打った。
        rec.set_search_text("release".into());
        // 走査中は「当たらなかった」と言わない——まだ数えていない。
        super::apply_list_counts(
            &rec,
            super::ListCounts {
                shown: 0,
                total: 0,
                not_downloaded: 0,
            },
            &scan_state,
        );
        assert_eq!(rec.get_empty_heading(), "Looking for recordings…");

        let mut session = recordings::session_for_test(
            chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                .expect("a real date")
                .and_hms_opt(14, 2, 0)
                .expect("a real time"),
        );
        session.dir = std::path::PathBuf::from("20260810-140200");
        session.has_mic = true;
        super::apply_scanned_sessions(
            &rec,
            &model,
            super::SessionLists {
                all: &all,
                shown: &shown,
            },
            &generation,
            &scan_state,
            &app_state,
            super::ScannedSessions {
                generation: 1,
                outcome: super::ScanOutcome::Scanned(vec![session]),
            },
        );
        assert_eq!(
            asked.borrow().as_slice(),
            ["release"],
            "the search typed while scanning must be run again against the new list"
        );
    }

    /// 中身を読み直しただけのときは**再生を差し替えない**。
    ///
    /// 差し替えると `adopt` が前の対象を手放し、再生中の音が止まって先頭へ巻き戻る。文字起こしの
    /// 完成は再生しながら待つ場面なので、そこで止まるのは痛い。
    #[test]
    fn only_a_new_selection_replaces_playback() {
        assert!(
            super::load_replaces_playback(true),
            "picking another recording swaps the audio"
        );
        assert!(
            !super::load_replaces_playback(false),
            "a reload after the transcript finished must not stop playback"
        );
    }

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
            crate::recordings::session_for_test(now.with_hour(14).expect("a valid hour")),
            crate::recordings::session_for_test(now.with_hour(9).expect("a valid hour")),
            crate::recordings::session_for_test(
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

    /// 一覧の合計は**件数だけ**（単数形も出す）。容量は全ファイルを開かないと分からない。
    #[test]
    fn the_library_summary_counts_recordings() {
        assert_eq!(super::library_text::library_summary(0), "0 recordings");
        assert_eq!(super::library_text::library_summary(1), "1 recording");
        assert_eq!(super::library_text::library_summary(148), "148 recordings");
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
            transcript_status_text(TranscriptStatus::Stopping),
            "Stopping…"
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

    /// 読む領域の空表示（#154）。**全状態で見出し・理由・次の操作が揃う**ことを固定する。
    /// `Done` は「セグメントが空＝JSON の欠落・破損」の経路でだけ表示される。
    #[test]
    fn transcript_pane_message_covers_all_states() {
        let panes = [
            TranscriptPane::NotTranscribed { auto_on: false },
            TranscriptPane::NotTranscribed { auto_on: true },
            TranscriptPane::Transcribing {
                model: "Medium".to_owned(),
                percent: Some(48),
            },
            TranscriptPane::Transcribing {
                model: "Medium".to_owned(),
                percent: None,
            },
            TranscriptPane::Stopping {
                model: "Medium".to_owned(),
            },
            TranscriptPane::Done,
            TranscriptPane::Failed {
                reason: TranscribeFailure::Panicked,
            },
            TranscriptPane::NotWhole {
                shortfall: TranscriptShortfall::StopsPartway,
            },
            TranscriptPane::NotWhole {
                shortfall: TranscriptShortfall::HasGaps,
            },
            TranscriptPane::NotWhole {
                shortfall: TranscriptShortfall::StopsPartwayWithGaps,
            },
        ];
        for pane in &panes {
            let message = pane.message();
            assert!(!message.heading.is_empty(), "{pane:?} needs a heading");
            assert!(!message.body.is_empty(), "{pane:?} needs a reason");
            assert!(
                message.actions.iter().filter(|a| a.primary).count() <= 1,
                "{pane:?} must not offer two primary actions"
            );
        }

        // 見出しは状態をそのまま言う。走っている間は割合まで出す。
        assert_eq!(
            TranscriptPane::NotTranscribed { auto_on: false }
                .message()
                .heading,
            "No transcript yet"
        );
        assert_eq!(
            TranscriptPane::Transcribing {
                model: "Medium".to_owned(),
                percent: Some(48),
            }
            .message()
            .heading,
            "Transcribing — 48%"
        );
        // 割合が来ていない間は数字を出さない（0% と出すと止まって見える）。
        assert_eq!(
            TranscriptPane::Transcribing {
                model: "Medium".to_owned(),
                percent: None,
            }
            .message()
            .heading,
            "Transcribing…"
        );
        // 走っている間は止められる（#163）。**主操作にはしない**——押しに来る人より進み具合を
        // 見に来る人のほうが多い。
        let running = TranscriptPane::Transcribing {
            model: "Medium".to_owned(),
            percent: None,
        }
        .message();
        assert_eq!(
            running
                .actions
                .iter()
                .map(|action| (action.kind, action.primary))
                .collect::<Vec<_>>(),
            vec![(PaneActionKind::StopTranscription, false)]
        );
        // 止めるよう伝えた後は、もう押せる操作を出さない（二度押しても何も変わらない）。
        assert!(
            TranscriptPane::Stopping {
                model: "Medium".to_owned(),
            }
            .message()
            .actions
            .is_empty()
        );
        // 使っているモデルは理由の中に出る。
        assert!(
            TranscriptPane::Transcribing {
                model: "Large v3".to_owned(),
                percent: None,
            }
            .message()
            .body
            .contains("Large v3")
        );
        // 失敗は種別から文を組む。**件数で形を変えない**ので、1 本でも複数でも音源ごとに
        // 1 文ずつ並ぶ。何も残っていないので、途中結果の 1 文もボタンも出ない。
        let nothing_kept = TranscriptPane::Failed {
            reason: TranscribeFailure::Files {
                failed: vec![FailedSource::new("mic.mp3", KeptFromSource::Nothing)],
                kept_other_sources: false,
            },
        };
        assert_eq!(
            nothing_kept.message().body,
            "mic.mp3 could not be transcribed."
        );
        assert!(!nothing_kept.shows_partial());
        assert!(
            !nothing_kept
                .message()
                .actions
                .iter()
                .any(|action| action.kind == PaneActionKind::ShowPartialTranscript),
            "a button that would reveal nothing is not offered"
        );
        assert_eq!(
            TranscriptPane::Failed {
                reason: TranscribeFailure::Files {
                    failed: vec![
                        FailedSource::new("mic.mp3", KeptFromSource::Nothing),
                        FailedSource::new("system.mp3", KeptFromSource::Nothing),
                    ],
                    kept_other_sources: false,
                },
            }
            .message()
            .body,
            "mic.mp3 could not be transcribed. system.mp3 could not be transcribed."
        );
        // 途中まで読めた音源は、**どこまで読めたか**を言う（#164）。
        let cut_short = TranscriptPane::Failed {
            reason: TranscribeFailure::Files {
                failed: vec![FailedSource::new(
                    "mic.mp3",
                    KeptFromSource::Upto(Duration::from_secs(252)),
                )],
                kept_other_sources: false,
            },
        };
        assert_eq!(
            cut_short.message().body,
            "mic.mp3 could not be read past 04:12. Everything that was read is kept."
        );
        // もう 1 本が最後まで行っていれば、失敗した音源から何も残らなくても読める。
        let other_source_kept = TranscriptPane::Failed {
            reason: TranscribeFailure::Files {
                failed: vec![FailedSource::new("system.mp3", KeptFromSource::Nothing)],
                kept_other_sources: true,
            },
        };
        assert_eq!(
            other_source_kept.message().body,
            "system.mp3 could not be transcribed. Everything that was read is kept."
        );
        // **抜けもあるなら位置を言わない**（#176）。残せた長さは読み飛ばしたぶん前へ詰まって
        // いて、音声の位置ではない。それでも**残っているので開く手は出す**。
        let cut_short_with_gaps = TranscriptPane::Failed {
            reason: TranscribeFailure::Files {
                failed: vec![FailedSource::new("mic.mp3", KeptFromSource::SomeWithGaps)],
                kept_other_sources: false,
            },
        };
        assert_eq!(
            cut_short_with_gaps.message().body,
            "mic.mp3 could not be read to the end, and parts of what was read are missing. \
             Everything that was read is kept."
        );

        // 残っていれば、開く手を出す（#164）。**主操作はやり直しのまま**——読めるのが途中まで
        // だと分かった人が次にしたいのは、たいてい取り直しではなく再実行。
        for pane in [&cut_short, &other_source_kept, &cut_short_with_gaps] {
            assert!(pane.shows_partial());
            assert_eq!(
                pane.message()
                    .actions
                    .iter()
                    .map(|action| (action.kind, action.primary))
                    .collect::<Vec<_>>(),
                vec![
                    (PaneActionKind::Transcribe, true),
                    (PaneActionKind::ShowPartialTranscript, false),
                ]
            );
        }
        // 走り終わっているが録音と食い違う（#175）。**開く手を必ず添える**——伏せた一覧を出す口は
        // これだけなので、落とすとセグメントが在るのに永久に読めなくなる。
        let stops_partway = TranscriptPane::NotWhole {
            shortfall: TranscriptShortfall::StopsPartway,
        };
        assert!(stops_partway.shows_partial());
        assert_eq!(
            stops_partway.message().heading,
            "This transcript stops partway"
        );
        assert_eq!(
            stops_partway
                .message()
                .actions
                .iter()
                .map(|action| (action.kind, action.primary))
                .collect::<Vec<_>>(),
            vec![
                (PaneActionKind::Transcribe, true),
                (PaneActionKind::ShowPartialTranscript, false),
            ]
        );

        // **食い違いごとに違うことを言う**（#176）。同じ文言へ畳むと、最後まで読めた録音に
        // 「一部しか文字起こしできていない」を出すことになる。**操作の並びは 3 つとも同じ**
        // ——押す位置が画面に出ない区別で入れ替わると、押し間違いを誘う。
        let messages: Vec<_> = [
            TranscriptShortfall::StopsPartway,
            TranscriptShortfall::HasGaps,
            TranscriptShortfall::StopsPartwayWithGaps,
        ]
        .into_iter()
        .map(|shortfall| TranscriptPane::NotWhole { shortfall }.message())
        .collect();
        for message in &messages {
            assert_eq!(
                message
                    .actions
                    .iter()
                    .map(|action| (action.kind, action.primary))
                    .collect::<Vec<_>>(),
                vec![
                    (PaneActionKind::Transcribe, true),
                    (PaneActionKind::ShowPartialTranscript, false),
                ]
            );
        }
        let bodies: std::collections::HashSet<&str> =
            messages.iter().map(|m| m.body.as_str()).collect();
        assert_eq!(bodies.len(), 3, "each shortfall must explain itself");
        // 抜けだけのときは、届いていないとは言わない。
        assert_eq!(messages[1].heading, "This transcript has gaps");
        assert!(!messages[1].body.contains("only part of this recording"));

        // **なぜ止まったか分からない**ときは、分かったふりをしない。
        assert_eq!(
            TranscriptPane::Failed {
                reason: TranscribeFailure::Panicked,
            }
            .message()
            .body,
            "Transcribing this recording stopped unexpectedly."
        );
    }

    /// 検索の一致判定。**大小を無視し、対象は文字起こしと議事録の本文だけ**（日時や音源を
    /// 入れると `mic` のような語が全件に当たって絞り込みにならない）。
    #[test]
    fn a_search_looks_at_the_transcript_and_the_notes() {
        // 走査はディレクトリ名が日時形式で、音源が 1 つ以上あるものだけを拾う。
        let root = std::env::temp_dir().join(format!("shoki-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("20260810-140200");
        std::fs::create_dir_all(&dir).expect("creating the temp dir should succeed");
        std::fs::write(dir.join("mic.mp3"), b"not a real mp3")
            .expect("writing the audio placeholder should succeed");
        std::fs::write(
            dir.join("mic.json"),
            r#"{"source":"mic","model":"small","segments":[{"start":0.0,"end":1.0,"text":"Closing on the Recording Format"}]}"#,
        )
        .expect("writing the transcript should succeed");
        std::fs::write(
            dir.join("summary.md"),
            "決定事項
- リリースは来週",
        )
        .expect("writing the notes should succeed");
        // 走査結果からそのまま取る（`RecordingSession` は非公開のフィールドを持つので、
        // テストで組み立てない）。
        let session = recordings::list_sessions(&root)
            .into_iter()
            .find(|session| session.dir == dir)
            .expect("the session should be listed");

        // **本番と同じ入口を通す**（#182）。囲いも `Fetch` も `search_sessions` の中で組む
        // ので、ここから呼べば読み取りから結果までの繋ぎがまるごと検査対象になる
        // （`docs/rules/testing.md`。`#[cfg(test)]` の証コンストラクタは足さない——足した
        // 瞬間に、囲いの外から読む書き方がテストだけ通るようになる）。
        let live = std::sync::atomic::AtomicU64::new(7);
        let search = |needle: &str| {
            let result = super::search_sessions(vec![session.clone()], needle, &live, 7)
                .expect("the search should not be cancelled");
            (result.matched.len(), result.not_downloaded_dirs.len())
        };
        // 文字起こしに一致する（大小を無視する。検索語は小文字化して渡される）。
        assert_eq!(search("recording format"), (1, 0));
        // 議事録にも当たる。
        assert_eq!(search("リリース"), (1, 0));
        // 当たらない語は落ちる。**読めなかったのではなく、当たらなかった**。
        assert_eq!(search("no such phrase"), (0, 0));
        // **日時や音源では当たらない**（`mic.json` というファイル名に引きずられない）。
        assert_eq!(search("mic"), (0, 0));

        // **世代が進んでいたら 1 件も読まずに降りる**（打鍵のたびに全件を読む時間を積まない）。
        let stale = std::sync::atomic::AtomicU64::new(8);
        assert!(
            super::search_sessions(vec![session], "recording format", &stale, 7).is_none(),
            "a search that has been overtaken must not report"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 絞り込み中の件数。**解除すれば戻ることが分かる形**にする。
    #[test]
    fn search_summary_text_shows_both_counts() {
        assert_eq!(
            super::library_text::search_summary_text(3, 148, 0),
            "3 of 148 recordings mention it"
        );
        // 0 件でも同じ形（件数で文の形は変えない。`docs/rules/messages.md`）。
        assert_eq!(
            super::library_text::search_summary_text(0, 148, 0),
            "0 of 148 recordings mention it"
        );
        // 名詞の単複は揃える（`library_summary` が `1 recording` と分けているのと同じ）。
        assert_eq!(
            super::library_text::search_summary_text(1, 1, 0),
            "1 of 1 recording mentions it"
        );
        assert_eq!(
            super::library_text::search_summary_text(0, 1, 0),
            "0 of 1 recording mentions it"
        );
    }

    /// **読めなかった録音があることを画面に出す**（#182）。黙って対象から外すと
    /// 「検索に出てこない＝無い」と読める。
    #[test]
    fn the_search_says_how_many_it_could_not_read() {
        assert_eq!(
            super::library_text::search_summary_text(3, 11, 8),
            "3 of 11 recordings mention it · 8 not downloaded"
        );
        // 全件が読めないときも同じ形（0 件でも理由が出る）。
        assert_eq!(
            super::library_text::search_summary_text(0, 11, 11),
            "0 of 11 recordings mention it · 11 not downloaded"
        );
        // 1 件でも文が壊れない（`not downloaded` は形容詞句なので単複を分けない）。
        assert_eq!(
            super::library_text::search_summary_text(0, 1, 1),
            "0 of 1 recording mentions it · 1 not downloaded"
        );
    }

    /// 読めなかった理由が**ウィンドウまで届く**こと（#182）。
    ///
    /// 件数の文を組む関数は単体で検査済みだが、**それを画面へ入れるかを決めているのは
    /// `apply_list_counts` の 1 行**。ここが「絞り込まれていなければ出さない」だけを見ると、
    /// 全件が当たって一部が退避、という組み合わせで理由が黙って消える
    /// （`docs/rules/testing.md` の「配線は、繋いでいる関数に継ぎ目を入れてテストする」）。
    #[test]
    fn the_window_is_told_what_the_search_could_not_read() {
        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        // 走査は終わっている前提の検査（走査中の優先順位は下の別テスト）。
        let scan_state = std::cell::Cell::new(super::ScanState::Settled);

        // 絞り込んでおらず、読めなかったものも無い＝言うことが無い。
        super::apply_list_counts(
            &rec,
            super::ListCounts {
                shown: 11,
                total: 11,
                not_downloaded: 0,
            },
            &scan_state,
        );
        assert_eq!(rec.get_search_summary(), "");

        // 絞り込み中は件数を出す。
        super::apply_list_counts(
            &rec,
            super::ListCounts {
                shown: 2,
                total: 11,
                not_downloaded: 0,
            },
            &scan_state,
        );
        assert_eq!(rec.get_search_summary(), "2 of 11 recordings mention it");

        // **読めなかったものが在るなら、件数の行にも言う**（本番で実際に出る組み合わせ。
        // 全件一致かつ一部が退避、は `SearchOutcome` が排他なので作れない）。
        super::apply_list_counts(
            &rec,
            super::ListCounts {
                shown: 0,
                total: 11,
                not_downloaded: 8,
            },
            &scan_state,
        );
        assert_eq!(
            rec.get_search_summary(),
            "0 of 11 recordings mention it · 8 not downloaded"
        );

        // **空表示の文も同じ呼び出しで入る**（別々に組むと、片方だけ「読めなかった」を
        // 言う画面ができる）。検索中かはウィンドウの検索欄から見る。
        rec.set_search_text("release".into());
        super::apply_list_counts(
            &rec,
            super::ListCounts {
                shown: 0,
                total: 11,
                not_downloaded: 8,
            },
            &scan_state,
        );
        assert_eq!(rec.get_empty_heading(), "No matches");
        assert!(
            rec.get_empty_body()
                .contains("8 recordings are not downloaded to this Mac"),
            "the empty list says what could not be searched, got {:?}",
            rec.get_empty_body()
        );
        rec.set_search_text("".into());
        super::apply_list_counts(
            &rec,
            super::ListCounts {
                shown: 0,
                total: 0,
                not_downloaded: 0,
            },
            &scan_state,
        );
        assert_eq!(rec.get_empty_heading(), "No recordings yet");
    }

    /// 一覧が空のときの文。**まだ数えていないのか・数えられなかったのか・録音が無いのか・
    /// 絞り込んで消えたのか**で言い分ける（同じ文にすると、検索語を消せば戻ることが分からない
    /// し、数えていないだけのときに録音を失ったと思わせる）。#182 で「読めなかった」、
    /// #181 で「走査中」と「走れなかった」を足した。
    ///
    /// 一覧の下端の 1 行は幅が足りなければ省略されるので、**0 件のときに理由が確実に見える
    /// 場所はここ**。
    #[test]
    fn an_empty_list_says_why_it_is_empty() {
        use super::library_text::EmptyList;

        let (heading, body) = super::library_text::empty_list_message(EmptyList::NoRecordings);
        assert_eq!(heading, "No recordings yet");
        assert!(body.starts_with("Start one from the shoki icon"));

        // **走査中は「録音が無い」と言わない**（#181）。まだ数えていないだけで、在るかどうかは
        // 分かっていない。ここが `NoRecordings` に潰れると、遅い保存先で開いた人は録音を
        // 失ったと思う。
        let (heading, body) = super::library_text::empty_list_message(EmptyList::Scanning);
        assert_eq!(heading, "Looking for recordings…");
        assert!(body.starts_with("Reading the save location"));

        // **走れなかったときも「録音が無い」と言わない**（#181）。空の結果として扱うと、
        // 1 件も見ていないのに「無い」と言い切ることになる。
        let (heading, body) = super::library_text::empty_list_message(EmptyList::ScanFailed);
        assert_eq!(heading, "Could not look for recordings");
        assert!(body.starts_with("Reading the save location did not start"));
        // **導線の名前をトレイのラベルと突き合わせる**。ここだけが「もう一度やる方法」を
        // 伝える文なので、実在しない項目名を書くと詰む（実際に `Recordings` と書いていた）。
        assert!(
            body.contains(super::tray::LIBRARY_LABEL),
            "the way back must name the menu item that exists, got {body:?}"
        );

        let (heading, body) =
            super::library_text::empty_list_message(EmptyList::NoMatches { not_downloaded: 0 });
        assert_eq!(heading, "No matches");
        assert_eq!(
            body,
            "No transcript or notes mention it. Recordings that have not been transcribed are \
             not searched."
        );

        // **読めなかったものが在るなら、0 件の理由に加える**。件数は独立した文にする
        // （カンマで繋ぐと、直前の「文字起こしされていない録音」に係って読める）。
        let (_, body) =
            super::library_text::empty_list_message(EmptyList::NoMatches { not_downloaded: 8 });
        assert!(
            body.ends_with(
                " 8 recordings are not downloaded to this Mac, so what they say could not be \
                 searched."
            ),
            "got {body:?}"
        );
        // 1 件でも文が壊れない（名詞も動詞も単数に合わせる）。
        let (_, body) =
            super::library_text::empty_list_message(EmptyList::NoMatches { not_downloaded: 1 });
        assert!(
            body.ends_with(
                " 1 recording is not downloaded to this Mac, so what they say could not be \
                 searched."
            ),
            "got {body:?}"
        );

        // 絞り込んでいなければ、読めなかったものの話はしない（検索していないので）。
        // **状態が持てる形にしたので、そもそも件数を渡せない**（#181）。
        let (heading, body) = super::library_text::empty_list_message(EmptyList::NoRecordings);
        assert_eq!(heading, "No recordings yet");
        assert!(!body.contains("not downloaded"));
    }

    /// 判定が**送る形へ落ちる**こと（#182）。
    ///
    /// 読み取りは退避されたファイルを用意できないと再現できないが、**判定から結果への
    /// 振り分けは繋いで検査できる**——ここが `NotDownloaded` を捨てると、読めなかったことは
    /// どこにも残らず、画面に理由が出なくなる（`docs/rules/testing.md` の「テストが見ている
    /// 入口と、本番が通る入口をずらさない」）。
    #[test]
    fn every_judgement_lands_somewhere() {
        use super::SearchOutcome as O;

        let session = |dir: &str| {
            let mut session = recordings::session_for_test(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                    .expect("a real date")
                    .and_hms_opt(14, 2, 0)
                    .expect("a real time"),
            );
            session.dir = std::path::PathBuf::from(dir);
            session
        };
        let result = super::collect_findings(
            42,
            vec![
                (session("hit"), O::Matched),
                (session("away"), O::NotDownloaded),
                (session("miss"), O::Missed),
                (session("hit2"), O::Matched),
            ],
        );

        // どの検索に対する結果かを運ぶ（受け取る側が古い結果を捨てるのに使う）。
        assert_eq!(result.generation, 42);
        // 当たったものは一覧へ（並びは渡した順のまま）。
        assert_eq!(
            result
                .matched
                .iter()
                .map(|session| session.dir.clone())
                .collect::<Vec<_>>(),
            [
                std::path::PathBuf::from("hit"),
                std::path::PathBuf::from("hit2")
            ]
        );
        // **読めなかったものは件数の元へ**。当たらなかっただけのものは、どちらにも入らない。
        assert_eq!(
            result.not_downloaded_dirs,
            [std::path::PathBuf::from("away")]
        );
    }

    /// 届いた検索結果が、**控えられ・数えられ・画面へ入る**こと（#182）。
    ///
    /// 控える 1 行だけを外に残すと、そこを消しただけで「読めなかったことを伝える」機能が
    /// 丸ごと死ぬのに、部品はどれも単体で緑のまま通る——実際、継ぎ目を下げるたびに同じ形の
    /// 穴が 1 段ずつ下がった（`docs/rules/testing.md`）。**ウィンドウごと呼んで止める。**
    #[test]
    fn a_search_result_reaches_the_window_and_stays_for_the_next_count() {
        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        rec.set_search_text("release".into());

        let session = |dir: &str| {
            let mut session = recordings::session_for_test(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                    .expect("a real date")
                    .and_hms_opt(14, 2, 0)
                    .expect("a real time"),
            );
            session.dir = std::path::PathBuf::from(dir);
            session
        };
        let all = vec![session("hit"), session("away"), session("miss")];
        let stored = std::cell::RefCell::new(Vec::new());

        let scan_state = std::cell::Cell::new(super::ScanState::Settled);
        let matched = super::apply_search_result(
            &rec,
            &stored,
            &all,
            &scan_state,
            super::SearchResult {
                generation: 1,
                matched: vec![session("hit")],
                not_downloaded_dirs: vec![std::path::PathBuf::from("away")],
            },
        );

        assert_eq!(matched.len(), 1, "the hit goes back to the list");
        // **読めなかったことが画面に出る**（件数の行と、0 件のときの説明の両方）。
        assert_eq!(
            rec.get_search_summary(),
            "1 of 3 recordings mention it · 1 not downloaded"
        );
        assert!(
            rec.get_empty_body()
                .contains("1 recording is not downloaded to this Mac"),
            "got {:?}",
            rec.get_empty_body()
        );
        // **控えが残る**——削除の経路はここから数え直す（残らないと、絞り込み中に 1 件消した
        // 瞬間に理由が消える）。
        assert_eq!(
            *stored.borrow(),
            [std::path::PathBuf::from("away")],
            "the next count reads this"
        );
    }

    /// 検索を解除したら、**検索にまつわる値が全部落ちる**こと（#182）。
    ///
    /// 1 つでも残ると、絞り込んでいないのに前の語の件数を出し続ける。解除の経路を足した人が
    /// 落とさないよう、後始末はこの関数に畳んである。
    #[test]
    fn clearing_the_search_drops_everything_it_left() {
        super::init_test_backend();
        let rec = super::LibraryWindow::new().expect("create the library window");
        rec.set_search_text("release".into());
        rec.set_search_summary("1 of 3 recordings mention it · 1 not downloaded".into());
        let generation = std::cell::Cell::new(4);
        let stored = std::cell::RefCell::new(vec![std::path::PathBuf::from("away")]);

        super::reset_search(&rec, &generation, &stored);

        assert_eq!(rec.get_search_text(), "");
        assert_eq!(rec.get_search_summary(), "");
        assert!(stored.borrow().is_empty());
        // **世代も進める**——走っていた検索の結果が後から届いて絞り直すのを防ぐ。
        assert_ne!(generation.get(), 4);
    }

    /// 届いた検索結果が、**いまの一覧に合わせて数え直される**こと（#182）。
    ///
    /// 検索結果は打鍵した時点のスナップショットなので、そのまま数えると「10 件中 11 件が
    /// 未ダウンロード」という合計より多い数が出る。**この 1 式が機能そのもの**——潰すと、
    /// 退避されて検索できなかったことは画面から完全に消えるのに、部品はどれも単体で緑の
    /// まま通る（`docs/rules/testing.md` の「テストが見ている入口と、本番が通る入口を
    /// ずらさない」）。
    #[test]
    fn the_count_drops_recordings_that_are_gone() {
        let session = |dir: &str| {
            let mut session = recordings::session_for_test(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                    .expect("a real date")
                    .and_hms_opt(14, 2, 0)
                    .expect("a real time"),
            );
            session.dir = std::path::PathBuf::from(dir);
            session
        };
        let all = vec![session("a"), session("b")];
        let not_downloaded = [
            std::path::PathBuf::from("a"),
            std::path::PathBuf::from("b"),
            // 絞り込んでいる間にゴミ箱へ移された。
            std::path::PathBuf::from("c"),
        ];

        assert_eq!(not_downloaded_count(&not_downloaded, &all), 2);
        assert_eq!(not_downloaded_count(&[], &all), 0);
        assert_eq!(not_downloaded_count(&not_downloaded, &[]), 0);

        // **画面へ渡る 3 つが同じ一覧から出る**こと。当たった録音のうち消えたものも落ちる。
        let counted =
            super::count_search_result(vec![session("a"), session("gone")], &not_downloaded, &all);
        assert_eq!(
            counted
                .matched
                .iter()
                .map(|session| session.dir.clone())
                .collect::<Vec<_>>(),
            [std::path::PathBuf::from("a")],
            "a recording that has been deleted must not come back"
        );
        assert_eq!(counted.total, 2);
        assert_eq!(
            counted.not_downloaded, 2,
            "what the search could not read must reach the window"
        );
    }

    /// 読めなかった本文があることが、**読んだ結果から結果へ運ばれる**こと（#182）。
    ///
    /// 退避されたファイルは CI でも手元でも作れないので、`Deadlock` から
    /// `NotDownloaded` までの繋ぎは実測でしか確かめられない。**その手前と先は繋いで検査
    /// できる**——ここが抜けると、読み手が「読めなかった」と言っているのに検索が黙って
    /// 「当たらなかった」に丸める形を書けてしまう。
    #[test]
    fn what_could_not_be_read_reaches_the_outcome() {
        use super::SearchOutcome as O;

        let summary = |text: Option<&str>, not_downloaded: bool| super::summarize::Summary {
            text: text.map(str::to_owned),
            not_downloaded,
        };
        let transcript = |text: Option<&str>, not_downloaded: bool| super::transcript::Segments {
            segments: text
                .map(|text| {
                    vec![super::transcript::TranscriptSegment {
                        start_secs: 0.0,
                        text: text.to_owned(),
                        speaker: super::transcript::Speaker::Mic,
                    }]
                })
                .unwrap_or_default(),
            not_downloaded,
        };
        let nothing = || (summary(None, false), transcript(None, false));

        // 読めた本文に当たる（大小を無視する）。議事録・文字起こしのどちらでも。
        let (s, t) = (
            summary(Some("Release next week"), false),
            transcript(None, false),
        );
        assert_eq!(outcome_of(&s, &t, "release"), O::Matched);
        let (s, t) = (
            summary(None, false),
            transcript(Some("Release next week"), false),
        );
        assert_eq!(outcome_of(&s, &t, "release"), O::Matched);

        // 何も無ければ「当たらなかった」。**読めなかったとは言わない**。
        let (s, t) = nothing();
        assert_eq!(outcome_of(&s, &t, "release"), O::Missed);

        // **片方でも実体が無ければ、当たらなかったときに必ずそう言う**。
        let (s, t) = (summary(None, true), transcript(None, false));
        assert_eq!(outcome_of(&s, &t, "release"), O::NotDownloaded);
        let (s, t) = (summary(None, false), transcript(None, true));
        assert_eq!(outcome_of(&s, &t, "release"), O::NotDownloaded);

        // 当たったなら、読めなかった本文が残っていても当たり（`search_outcome` の割り切り）。
        let (s, t) = (
            summary(Some("Release next week"), false),
            transcript(None, true),
        );
        assert_eq!(outcome_of(&s, &t, "release"), O::Matched);
    }

    /// 失敗の理由 → 文言を**全種別で固定する**。
    ///
    /// 網羅 match が守るのは「種別を足したら割れる」ことだけで、**既存の文の書き換えは何も
    /// 検知しない**。ここは画面にそのまま出る文なので、値で押さえる（`docs/rules/testing.md`）。
    /// **表をテスト側で `match` にしない**——実装を写すだけになって、意味が無くなる。
    ///
    /// **パスが混ざらないことはここでは見ない**。この関数は渡された名前をそのまま並べるだけで、
    /// 保証を作っているのは名前を作る側（`transcribe::audio_display_name` /
    /// `transcribe::job_model_label`）。そちらのテストが対で押さえる。
    #[test]
    fn failure_text_is_fixed_for_every_kind() {
        use shoki_core::{SummarizeFailure as S, TranscribeFailure as T};

        let transcribe_cases = [
            (
                T::ModelDownload,
                "The transcription model could not be downloaded.",
            ),
            (T::ModelMissing, "The transcription model file is missing."),
            (
                T::ModelUnreadable,
                "The transcription model file could not be opened.",
            ),
            (T::ModelLoad, "The transcription model could not be loaded."),
            (
                T::Files {
                    failed: vec![FailedSource::new("mic.mp3", KeptFromSource::Nothing)],
                    kept_other_sources: false,
                },
                "mic.mp3 could not be transcribed.",
            ),
            (
                // **件数で文の形は変えない**（`docs/rules/messages.md`）ので、音源ごとに 1 文。
                T::Files {
                    failed: vec![
                        FailedSource::new("mic.mp3", KeptFromSource::Nothing),
                        FailedSource::new("system.mp3", KeptFromSource::Nothing),
                    ],
                    kept_other_sources: false,
                },
                "mic.mp3 could not be transcribed. system.mp3 could not be transcribed.",
            ),
            (
                // 途中まで読めた音源は、どこまで読めたかと、残っていることを言う（#164）。
                T::Files {
                    failed: vec![FailedSource::new(
                        "mic.mp3",
                        KeptFromSource::Upto(Duration::from_secs(3852)),
                    )],
                    kept_other_sources: false,
                },
                "mic.mp3 could not be read past 1:04:12. Everything that was read is kept.",
            ),
            (
                // 抜けもある音源は、**位置を言わない**（#176）。残せた長さは読み飛ばした
                // ぶん前へ詰まっていて、音声の位置ではない。
                T::Files {
                    failed: vec![FailedSource::new("mic.mp3", KeptFromSource::SomeWithGaps)],
                    kept_other_sources: false,
                },
                "mic.mp3 could not be read to the end, and parts of what was read are missing. \
                 Everything that was read is kept.",
            ),
            (
                // 失敗した音源から何も残らなくても、もう 1 本が最後まで行っていれば読める。
                T::Files {
                    failed: vec![FailedSource::new("system.mp3", KeptFromSource::Nothing)],
                    kept_other_sources: true,
                },
                "system.mp3 could not be transcribed. Everything that was read is kept.",
            ),
            (
                T::Panicked,
                "Transcribing this recording stopped unexpectedly.",
            ),
        ];
        for (reason, expected) in &transcribe_cases {
            assert_eq!(&transcribe_failure_text(reason), expected);
        }

        let summarize_cases = [
            (
                S::ModelPrepare,
                "The meeting notes model could not be prepared.",
            ),
            (
                S::ModelRun,
                "The model could not finish. It may need more free memory than this Mac has right \
                 now — closing other apps, or choosing a smaller model, can let it run.",
            ),
            (S::EmptyOutput, "The model returned nothing to write."),
            (S::Save, "The notes could not be saved."),
            (S::Panicked, "Writing notes stopped unexpectedly."),
        ];
        for (reason, expected) in &summarize_cases {
            assert_eq!(&summarize_failure_text(reason), expected);
        }
    }

    /// 状態行の文言は、**空表示と同じ値から出す**（#175 / #176）。一覧と共用の状態 enum は
    /// 録音との食い違いを持てないので、状態 enum から出すと同じペインの中で
    /// 「Transcribed」と「This transcript stops partway」が並ぶ。
    #[test]
    fn the_status_line_says_the_same_thing_as_the_empty_state() {
        let not_whole = |shortfall| TranscriptPane::NotWhole { shortfall }.status_text();
        assert_eq!(
            not_whole(TranscriptShortfall::StopsPartway),
            "Transcribed in part"
        );
        // **抜けは届いていないことと区別して言う**（#176）。同じ文言に畳むと、事実と違う
        // 「一部しか文字起こしできていない」を、最後まで読めた録音に出すことになる。
        assert_eq!(
            not_whole(TranscriptShortfall::HasGaps),
            "Transcribed with gaps"
        );
        // 両方のときは、届いていないほうを先に言う。
        assert_eq!(
            not_whole(TranscriptShortfall::StopsPartwayWithGaps),
            "Transcribed in part"
        );
        for shortfall in [
            TranscriptShortfall::StopsPartway,
            TranscriptShortfall::HasGaps,
            TranscriptShortfall::StopsPartwayWithGaps,
        ] {
            assert_ne!(
                not_whole(shortfall),
                TranscriptPane::Done.status_text(),
                "a transcript that does not match the recording must not read as a finished one"
            );
        }
        // 残りは状態 enum の表をそのまま使う（増やしたのはこの 1 つだけ）。
        for pane in [
            TranscriptPane::Done,
            TranscriptPane::NotTranscribed { auto_on: false },
            TranscriptPane::Stopping {
                model: "Medium".to_owned(),
            },
            TranscriptPane::Failed {
                reason: TranscribeFailure::Panicked,
            },
        ] {
            assert_eq!(pane.status_text(), transcript_status_text(pane.status()));
        }
    }

    /// 議事録側の選び方。**入力（文字起こし）の様子で 3 つに割れる条件はここでしか決まらない**。
    #[test]
    fn summary_pane_of_reads_the_state_of_its_input() {
        use shoki_core::TranscriptInput as I;

        let queued = Some(summarize::SummarizeState::Queued { position: 2 });

        // ワーカーの状態があれば、`summary.md` の有無より優先する。
        assert_eq!(
            summary_pane_of(queued, true, I::Ready, false),
            SummaryPane::Queued { position: 2 }
        );

        // 文字起こしが無ければ「まだ書けない」。ただし**既にある議事録は読ませる**ので、
        // 有無の判定はそちらが先。
        assert_eq!(
            summary_pane_of(None, false, I::Missing, false),
            SummaryPane::Blocked
        );
        assert_eq!(
            summary_pane_of(None, true, I::Missing, false),
            SummaryPane::Done
        );
        // 文字起こしがあれば「まだ書いていない」。自動の状態が pane まで届く。
        assert_eq!(
            summary_pane_of(None, false, I::Ready, true),
            SummaryPane::NotSummarized { auto_on: true }
        );
        // 続けて書く依頼の待ち時間（#165）。**「まだ書けない」とは言い分ける**——待っていれば
        // 始まるので、押す手を出さない。
        assert_eq!(
            summary_pane_of(None, false, I::Running, false),
            SummaryPane::WaitingForTranscript
        );
        assert!(
            SummaryPane::WaitingForTranscript
                .message()
                .actions
                .is_empty()
        );
        // 入力が失敗したら議事録は始まらない。そう言って、やり直す手を出す。
        assert_eq!(
            summary_pane_of(None, false, I::Failed, false),
            SummaryPane::TranscriptFailed
        );
        // 入力が録音と食い違うときは、そう言う（#175 / #176）。**止めはしない**ので、書く手は出す。
        assert_eq!(
            summary_pane_of(None, false, I::NotWhole, false),
            SummaryPane::NotesFromPartialTranscript
        );
        let partial_input = SummaryPane::NotesFromPartialTranscript;
        // **どう食い違っているかは言い分けない**（#176）。途中で終わっていても中が抜けていても
        // 議事録にとっては同じなので、内訳を持たない言い方であること。
        assert!(
            partial_input
                .message()
                .body
                .contains("missing parts of this recording")
        );
        assert_eq!(
            partial_input
                .message()
                .actions
                .iter()
                .map(|action| (action.kind, action.primary))
                .collect::<Vec<_>>(),
            vec![
                (PaneActionKind::WriteNotes, true),
                (PaneActionKind::OpenTranscription, false),
            ]
        );
        assert_eq!(partial_input.status(), SummaryStatus::NotSummarized);
        assert_eq!(
            SummaryPane::TranscriptFailed
                .message()
                .actions
                .iter()
                .map(|action| action.kind)
                .collect::<Vec<_>>(),
            vec![
                PaneActionKind::TranscribeThenNotes,
                PaneActionKind::OpenTranscription,
            ]
        );
    }

    /// 走っているジョブがある間は、中身を作り直す操作を出さない（取り消しと窓は残す）。
    #[test]
    fn actions_are_dropped_while_a_job_is_running() {
        let all = vec![
            PaneAction {
                label: "Try again".into(),
                kind: PaneActionKind::WriteNotes,
                primary: true,
            },
            PaneAction {
                label: "Cancel".into(),
                kind: PaneActionKind::CancelNotes,
                primary: false,
            },
            PaneAction {
                label: "Open meeting notes".into(),
                kind: PaneActionKind::OpenNotes,
                primary: false,
            },
            // 止める操作は走っている間しか出ないので、ここで落とすと出す先が無くなる（#163）。
            PaneAction {
                label: "Stop".into(),
                kind: PaneActionKind::StopTranscription,
                primary: false,
            },
            // 続けて書く操作も**中身を作り直す**（#165）。いまの経路では走行中に並ばないが、
            // ゲートは 1 箇所なので、ここが緩むと並んだ日に静かに重ね投入できる。
            PaneAction {
                label: "Transcribe, then write notes".into(),
                kind: PaneActionKind::TranscribeThenNotes,
                primary: true,
            },
            // 途中結果を開く操作はディスクに触らないので残す（#164）。
            PaneAction {
                label: "Show partial".into(),
                kind: PaneActionKind::ShowPartialTranscript,
                primary: false,
            },
        ];
        assert_eq!(actions_allowed_while_busy(all.clone(), false), all);
        assert_eq!(
            actions_allowed_while_busy(all, true)
                .iter()
                .map(|action| action.kind)
                .collect::<Vec<_>>(),
            vec![
                PaneActionKind::CancelNotes,
                PaneActionKind::OpenNotes,
                PaneActionKind::StopTranscription,
                PaneActionKind::ShowPartialTranscript,
            ]
        );
    }

    /// 状態 enum は文言と**同じ値から**作る（別々に渡すと食い違う。`docs/rules/slint.md`）。
    #[test]
    fn transcript_pane_status_matches_the_variant() {
        assert_eq!(
            TranscriptPane::NotTranscribed { auto_on: true }.status(),
            TranscriptStatus::NotTranscribed
        );
        assert_eq!(
            TranscriptPane::Transcribing {
                model: "Medium".to_owned(),
                percent: None,
            }
            .status(),
            TranscriptStatus::Transcribing
        );
        assert_eq!(TranscriptPane::Done.status(), TranscriptStatus::Done);
        assert_eq!(
            TranscriptPane::Failed {
                reason: TranscribeFailure::Panicked,
            }
            .status(),
            TranscriptStatus::Failed
        );
    }

    /// 要約もワーカーの進行状況を優先し、無ければ `summary.md` の有無で解決する
    /// （文字起こし側と同じ契約）。
    ///
    /// **状態だけを出す別の関数は置かない**（#188）。同じ優先順位が 2 つになると、片方だけ
    /// 直した日に一覧と読む領域が違うことを言う——`summary_pane_of` の `status()` で見る。
    #[test]
    fn the_summary_pane_prefers_worker_status_over_the_file() {
        use crate::summarize::SummarizeStatus;

        let state = |status| match status {
            SummarizeStatus::Queued => Some(summarize::SummarizeState::Queued { position: 1 }),
            SummarizeStatus::Summarizing => Some(summarize::SummarizeState::Summarizing {
                model_label: "Qwen".to_owned(),
                elapsed: Duration::from_secs(40),
            }),
            SummarizeStatus::Done => Some(summarize::SummarizeState::Done),
            SummarizeStatus::Failed => Some(summarize::SummarizeState::Failed {
                reason: shoki_core::SummarizeFailure::ModelRun,
            }),
        };
        let status = |worker, has_summary| {
            super::summary_pane_of(
                worker,
                has_summary,
                shoki_core::TranscriptInput::Ready,
                false,
            )
            .status()
        };

        // 投入直後（キュー待ち）は生成中と区別する。取り消せるのはこの間だけ。
        assert_eq!(
            status(state(SummarizeStatus::Queued), false),
            SummaryStatus::Queued
        );
        // 再生成中は `summary.md` が残っていても「生成中」。
        assert_eq!(
            status(state(SummarizeStatus::Summarizing), true),
            SummaryStatus::Summarizing
        );
        assert_eq!(
            status(state(SummarizeStatus::Done), false),
            SummaryStatus::Done
        );
        // 失敗の記録は古い `summary.md` があっても優先する（失敗を隠さない）。
        assert_eq!(
            status(state(SummarizeStatus::Failed), true),
            SummaryStatus::Failed
        );
        // ワーカーの記録が無ければファイルの有無で解決する（起動前に生成した分など）。
        assert_eq!(status(None, true), SummaryStatus::Done);
        assert_eq!(status(None, false), SummaryStatus::NotSummarized);
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

    /// 議事録側の空表示（#154）。`Done` で行が空になるのは `summary.md` の欠落・破損・空の経路。
    #[test]
    fn summary_pane_message_covers_all_states() {
        let panes = [
            SummaryPane::Blocked,
            SummaryPane::NotSummarized { auto_on: false },
            SummaryPane::NotSummarized { auto_on: true },
            SummaryPane::Queued { position: 2 },
            SummaryPane::Summarizing {
                model: "Qwen2.5 3B Instruct".to_owned(),
                started_ago: "40 seconds".to_owned(),
            },
            SummaryPane::Done,
            SummaryPane::Failed {
                reason: SummarizeFailure::ModelRun,
            },
            SummaryPane::WaitingForTranscript,
            SummaryPane::TranscriptFailed,
            SummaryPane::NotesFromPartialTranscript,
        ];
        for pane in &panes {
            let message = pane.message();
            assert!(!message.heading.is_empty(), "{pane:?} needs a heading");
            assert!(!message.body.is_empty(), "{pane:?} needs a reason");
            assert!(
                message.actions.iter().filter(|a| a.primary).count() <= 1,
                "{pane:?} must not offer two primary actions"
            );
        }

        // **入力待ちは「書けない理由」を言う**。ここを「まだ書いていない」と同じ文にすると、
        // なぜ押せないのかが画面から分からなくなる。主操作は**議事録まで続ける**もの（#165）
        // ——ここまで来た人が欲しいのは議事録で、文字起こしはその途中でしかない。
        let blocked = SummaryPane::Blocked.message();
        assert!(blocked.body.contains("transcript"));
        assert_eq!(
            blocked
                .actions
                .iter()
                .map(|action| action.kind)
                .collect::<Vec<_>>(),
            vec![
                PaneActionKind::TranscribeThenNotes,
                PaneActionKind::OpenTranscription
            ]
        );
        assert_eq!(blocked.actions[0].label, "Transcribe, then write notes");

        // キュー待ちは順番まで出し、取り消しを添える。
        assert_eq!(
            SummaryPane::Queued { position: 2 }.message().heading,
            "Waiting to start — number 2 in the queue"
        );
        assert_eq!(
            SummaryPane::Queued { position: 2 }.message().actions[0].kind,
            PaneActionKind::CancelNotes
        );

        // 走っている間はモデルと経過を出し、操作は出さない。
        let running = SummaryPane::Summarizing {
            model: "Llama 8B".to_owned(),
            started_ago: "3 minutes".to_owned(),
        }
        .message();
        assert!(running.body.contains("Llama 8B"));
        assert!(running.body.contains("3 minutes"));
        assert!(running.actions.is_empty());

        // 失敗は理由と、そこから取れる 2 つの手を出す。
        let failed = SummaryPane::Failed {
            reason: SummarizeFailure::EmptyOutput,
        }
        .message();
        assert_eq!(failed.body, "The model returned nothing to write.");
        assert_eq!(
            failed
                .actions
                .iter()
                .map(|action| action.kind)
                .collect::<Vec<_>>(),
            vec![PaneActionKind::WriteNotes, PaneActionKind::OpenNotes]
        );
    }

    /// `Blocked` と `NotSummarized` はどちらも「未生成」に落ちる（説明だけが違う）。
    #[test]
    fn summary_pane_status_matches_the_variant() {
        assert_eq!(SummaryPane::Blocked.status(), SummaryStatus::NotSummarized);
        assert_eq!(
            SummaryPane::NotSummarized { auto_on: true }.status(),
            SummaryStatus::NotSummarized
        );
        assert_eq!(
            SummaryPane::Queued { position: 1 }.status(),
            SummaryStatus::Queued
        );
        assert_eq!(
            SummaryPane::Summarizing {
                model: "Llama 8B".to_owned(),
                started_ago: "1 second".to_owned(),
            }
            .status(),
            SummaryStatus::Summarizing
        );
        assert_eq!(SummaryPane::Done.status(), SummaryStatus::Done);
        assert_eq!(
            SummaryPane::Failed {
                reason: SummarizeFailure::Save,
            }
            .status(),
            SummaryStatus::Failed
        );
    }

    /// 経過は読める粒度へ丸める（100ms の tick で秒が動き続けないように、1 分以上は分だけ）。
    #[test]
    fn elapsed_text_rounds_to_a_readable_unit() {
        assert_eq!(elapsed_text(Duration::from_secs(0)), "0 seconds");
        assert_eq!(elapsed_text(Duration::from_secs(40)), "40 seconds");
        assert_eq!(elapsed_text(Duration::from_secs(59)), "59 seconds");
        // **単複を揃える**。そのまま `started 1 minute ago` として画面に出る。
        assert_eq!(elapsed_text(Duration::from_secs(1)), "1 second");
        assert_eq!(elapsed_text(Duration::from_secs(60)), "1 minute");
        assert_eq!(elapsed_text(Duration::from_secs(200)), "3 minutes");
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
