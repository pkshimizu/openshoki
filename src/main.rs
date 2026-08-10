//! shoki — メニューバー／タスクバーに常駐する録音アプリのエントリポイント。
//!
//! 起動時はウィンドウを表示せずトレイに常駐し、トレイメニューから設定ウィンドウ・Recordings
//! ウィンドウの表示/非表示とアプリ終了を行う。録音・文字起こし・議事録要約は各モジュール
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

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, SystemTime};

// VecModel の row_data / set_row_data（tick の行単位更新）に必要。
use slint::Model;

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
const WINDOW_WIDTH: f32 = 460.0;
const WINDOW_HEIGHT: f32 = 900.0;
/// 初回表示位置（画面左上からの暫定値）。中央寄せ等の調整は後続に回す。
const WINDOW_X: f32 = 240.0;
const WINDOW_Y: f32 = 160.0;

/// Recordings ウィンドウの初期ジオメトリ。幅・高さは `ui/recordings-window.slint` の
/// min/preferred と一致させること（片方だけ変えない）。設定ウィンドウと重ならない位置に出す。
const RECORDINGS_WIDTH: f32 = 720.0;
const RECORDINGS_HEIGHT: f32 = 540.0;
const RECORDINGS_X: f32 = 200.0;
const RECORDINGS_Y: f32 = 120.0;

/// モデル管理ウィンドウの初期ジオメトリ。幅・高さは `ui/models-window.slint` の min/preferred と
/// 一致させること（片方だけ変えない）。設定ウィンドウから開くので、それと重ならない位置に出す。
const MODELS_WIDTH: f32 = 560.0;
const MODELS_HEIGHT: f32 = 520.0;
const MODELS_X: f32 = 700.0;
const MODELS_Y: f32 = 200.0;

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
    ui.set_whisper_models(model_choices(whisper_model::CATALOG));
    // 議事録要約: トグルと、使う LLM の選択・取得状況。選択肢の組み立て・フォールバックは
    // whisper と同じ（選択肢には所要時間とメモリの目安を含める。数 GB のダウンロードと
    // 数十秒・数 GB の実行コストが選択で決まるため）。
    ui.set_auto_summarize(config.borrow().auto_summarize);
    ui.set_summary_models(model_choices(summary_model::CATALOG));
    // 選択位置と状態行は、モデル管理ウィンドウから選び直したときの追従と**同じ関数**で入れる
    // （両種別ぶんを 1 箇所にまとめ、種別や状態行の導出が増えたときに初期化だけ取り残されない
    // ようにする）。
    apply_model_selection_to_settings(&ui, &config.borrow(), &model_downloader);
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

    // 「議事録要約を自動生成」トグル: 永続化に成功してから反映する（自動文字起こしトグルと対称）。
    // モデルは内蔵だが、ここでは取得を始めない（数 GB あり、ON にしただけで落とし始めると
    // 帯域とディスクを黙って使う）。取得の契機は `model_downloads_on_select` の
    // doc コメントを参照。
    let config_for_summarize = Rc::clone(&config);
    let ui_for_summarize = ui.as_weak();
    ui.on_toggle_auto_summarize(move |enabled| {
        let Some(ui) = ui_for_summarize.upgrade() else {
            return;
        };
        let mut candidate = config_for_summarize.borrow().clone();
        candidate.auto_summarize = enabled;
        if let Err(err) = candidate.save() {
            eprintln!(
                "Not changing the auto-summarize setting because saving the settings failed: {err}"
            );
            ui.set_auto_summarize(config_for_summarize.borrow().auto_summarize);
            return;
        }
        *config_for_summarize.borrow_mut() = candidate;
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
    // 未取得ならバックグラウンドでダウンロードを開始する（進捗はタイマーが状態行へ反映する）。
    // 取得を始めない場合もある（`whisper_model_path` で上書き中。契機の正は
    // `model_downloads_on_select`）。永続化と取得開始そのものは `select_model` が持つ
    // （モデル管理ウィンドウの「Use」と**同じ経路**にして、どちらから選んでも同じ結果になる
    // ようにする）。
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
        if !select_model(
            model_download::ModelKind::Speech,
            spec,
            &config_for_model,
            &downloader_for_model,
        ) {
            ui.set_whisper_model_index(whisper_model::model_index(
                &config_for_model.borrow().whisper_model,
            ) as i32);
            return;
        }
        whisper_model_status_line(&config_for_model.borrow(), &downloader_for_model)
            .apply_whisper(&ui);
    });

    // 要約 LLM の変更: whisper モデルの変更と同じ流儀（インデックス→ID の変換、永続化成功後に
    // 反映、保存失敗時は表示を保存済みの値へ戻す）。取得を始める条件だけ whisper と違うが、
    // その分岐も `select_model` が持つ（理由は `model_downloads_on_select` の doc）。
    let config_for_summary_model = Rc::clone(&config);
    let ui_for_summary_model = ui.as_weak();
    let downloader_for_summary_model = model_downloader.clone();
    ui.on_change_summary_model(move |index| {
        let Some(ui) = ui_for_summary_model.upgrade() else {
            return;
        };
        // ComboBox は Rust が渡したカタログの範囲しか返さないが、防御的に既定へ丸める。
        let spec = usize::try_from(index)
            .ok()
            .and_then(|i| summary_model::CATALOG.get(i))
            .unwrap_or_else(|| summary_model::default_spec());
        if !select_model(
            model_download::ModelKind::Summary,
            spec,
            &config_for_summary_model,
            &downloader_for_summary_model,
        ) {
            ui.set_summary_model_index(summary_model::model_index(
                &config_for_summary_model.borrow().summary_model,
            ) as i32);
            return;
        }
        summary_model_status_line(
            &config_for_summary_model.borrow(),
            &downloader_for_summary_model,
        )
        .apply_summary(&ui);
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

    // 議事録要約のバックグラウンドワーカー。文字起こしワーカーが成功時に投入する（設定
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

    // セッション選択: 詳細を更新し、その音源を再生準備（停止状態でロード。Play で再生開始）。
    {
        let player = Rc::clone(&player);
        let sessions = Rc::clone(&sessions);
        let transcript_segments = Rc::clone(&transcript_segments);
        let transcriber = transcriber.clone();
        let summarizer = summarizer.clone();
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
            rec.set_detail_sources(session.source_summary().into());
            rec.set_has_transcript(session.has_transcript);
            // 文字起こしの状態テキストと Transcribe ボタンの活性を、ワーカーの進行状況＋
            // JSON の有無から設定する（以後の変化は tick が追従させる）。
            refresh_detail_transcript_status(&rec, &transcriber, session);
            // 議事録要約も同じ流儀で状態を設定し、`summary.md` を読み直して表示へ反映する。
            refresh_detail_summary_status(&rec, &summarizer, session);
            refresh_detail_summary_rows(&rec, &session.dir);
            // 文字起こしを読み込み、話者ラベル＋開始時刻付きのセグメント一覧を更新する
            // （空＝欠落・破損・未生成なら Slint 側が縮退表示する）。
            let segments = transcript::load_transcript(&session.dir);
            rec.set_segments(Rc::new(slint::VecModel::from(transcript_rows(&segments))).into());
            rec.set_current_segment(-1);
            *transcript_segments.borrow_mut() = segments;
            rec.set_playing(false);
            // 再生対象は事前生成の mix.mp3（両音源）か単一音源ファイル。両音源で mix.mp3 が
            // まだ無ければ再生不可（選択時にその場でミックスして UI を固めない）。
            let playable = session.is_playable();
            rec.set_playable(playable);
            let duration = {
                let mut player = player.borrow_mut();
                match (session.playback_path(), player.as_mut()) {
                    (Some(path), Some(p)) => match p.load(&path) {
                        Ok(()) => p.duration(),
                        Err(err) => {
                            eprintln!("Failed to load the recording for playback: {err}");
                            None
                        }
                    },
                    // 再生対象が無いセッション（両音源で mix.mp3 が未生成）でも前の音声は手放す
                    // （理由は `AudioPlayer::unload` の doc コメント参照）。
                    (None, Some(p)) => {
                        p.unload();
                        None
                    }
                    // 出力デバイスを開けない環境では再生ハンドルが無く、手放す対象も無い。
                    (_, None) => None,
                }
            };
            apply_playback_position(&rec, Duration::ZERO, duration);
            // 全体長が分からないと比率→秒の換算ができないため、その場合はシークバーを
            // 表示専用に縮退させる。
            rec.set_seekable(playable && duration.is_some());
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

    // 「Summarize」: 選択中セッションの議事録要約を手動で（再）生成する。設定 `auto_summarize`
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
            // 進行状況マップに残ったエントリを掃除する（削除済みセッションの記録を残さない）。
            transcriber.forget(&dir);
            summarizer.forget(&dir);
            clear_recordings_selection(&rec, &transcript_segments);
        });
    }

    // モデル管理ウィンドウ（#117 で新設し、#138 で取得・選択まで扱うようにした）。設定画面の
    // ボタンで開く。設定・Recordings と同じく起動時に生成して隠しておき、閉じても常駐を保つ。
    let models_ui = ModelsWindow::new()?;
    models_ui
        .window()
        .on_close_requested(|| slint::CloseRequestResponse::HideWindow);
    // 一覧の素材（インデックス→操作対象の解決に使う）と、UI が参照し続けるモデル。
    // 差し替えずに行単位で更新する（tick で差し替えるとクリックを取りこぼす）。
    let model_list = ModelListHandles {
        sources: Rc::new(RefCell::new(Vec::new())),
        override_files: Rc::new(RefCell::new(OverrideFiles::default())),
        rows: Rc::new(slint::VecModel::default()),
        downloaded_seen: Rc::new(RefCell::new(Vec::new())),
    };
    models_ui.set_models(model_list.rows.clone().into());
    // 行が 1 つも無いときの縮退表示。カタログの行は必ず並ぶので実際には出ないが、表示の穴を
    // 残さないために一度だけ入れておく（走査の失敗は通知で伝える。`MODELS_UNREADABLE_NOTICE`）。
    models_ui.set_empty_text(MODELS_EMPTY_TEXT.into());
    {
        let models_weak = models_ui.as_weak();
        let ui_weak = ui.as_weak();
        let list = model_list.clone();
        let downloader = model_downloader.clone();
        let transcriber_for_models = transcriber.clone();
        let summarizer_for_models = summarizer.clone();
        let config_for_models = Rc::clone(&config);
        // 3 つのハンドラは「対象の行を引く → 操作する → 一覧を作り直す」が共通なので、
        // 作り直しだけを共有のクロージャ（`refresh`）にして、操作ごとにハンドラを分ける。
        let refresh = move |models: &ModelsWindow, notice: Option<&'static str>| {
            refresh_models_window(
                models,
                &list,
                &downloader,
                &transcriber_for_models,
                &summarizer_for_models,
                &config_for_models.borrow(),
                ModelsRefresh::AfterOperation(notice),
            );
        };
        let refresh = Rc::new(refresh);

        // 「Use」: 使うモデルを選び直す（設定画面の ComboBox と同じ経路）。
        {
            let models_weak = models_weak.clone();
            let ui_weak = ui_weak.clone();
            let sources = Rc::clone(&model_list.sources);
            let downloader = model_downloader.clone();
            let config = Rc::clone(&config);
            let refresh = Rc::clone(&refresh);
            models_ui.on_use_model(move |index| {
                let Some(models) = models_weak.upgrade() else {
                    return;
                };
                // 境界チェックと要素取得を get(i) で一体にする（他ハンドラと同じパターン）。
                let Some(ModelRowSource::Catalog { kind, spec, .. }) = usize::try_from(index)
                    .ok()
                    .and_then(|i| sources.borrow().get(i).cloned())
                else {
                    return; // 見出し・カタログ外の行では Use を出していない。
                };
                let saved = select_model(kind, spec, &config, &downloader);
                if saved && let Some(ui) = ui_weak.upgrade() {
                    // 設定画面の ComboBox と状態行を追従させる（どちらから選んでも同じ結果）。
                    apply_model_selection_to_settings(&ui, &config.borrow(), &downloader);
                }
                refresh(&models, (!saved).then_some(MODEL_SELECT_FAILED_NOTICE));
            });
        }

        // 「Download」: 選択は変えずに取得だけ始める（未取得・失敗の行にだけ出る）。
        {
            let models_weak = models_weak.clone();
            let sources = Rc::clone(&model_list.sources);
            let downloader = model_downloader.clone();
            let refresh = Rc::clone(&refresh);
            models_ui.on_download_model(move |index| {
                let Some(models) = models_weak.upgrade() else {
                    return;
                };
                let Some(ModelRowSource::Catalog { spec, .. }) = usize::try_from(index)
                    .ok()
                    .and_then(|i| sources.borrow().get(i).cloned())
                else {
                    return;
                };
                // 取得済み・DL 中なら request_download 側が早期 return する。進捗は tick が拾う。
                downloader.request_download(spec);
                refresh(&models, None);
            });
        }

        // 「Delete」: 確認モーダルの確定から呼ばれる。
        {
            let models_weak = models_weak.clone();
            let list = model_list.clone();
            let downloader = model_downloader.clone();
            let transcriber = transcriber.clone();
            let summarizer = summarizer.clone();
            let config = Rc::clone(&config);
            let refresh = Rc::clone(&refresh);
            models_ui.on_delete_model(move |index| {
                let Some(models) = models_weak.upgrade() else {
                    return;
                };
                let Some(source) = usize::try_from(index)
                    .ok()
                    .and_then(|i| list.sources.borrow().get(i).cloned())
                else {
                    return;
                };
                let Some(target) = source.installed().cloned() else {
                    // Delete はディスクに実体がある行にしか出さないので通常は来ないが、
                    // 「押しても無反応」に見せないため通知とログを残す（#117 の方針）。
                    eprintln!("Skipping the model deletion because the file is no longer listed");
                    refresh(&models, delete_failure_notice(DeleteOutcome::Failed));
                    return;
                };
                let config = config.borrow();
                // **押された時点で使用中を再確認する**。一覧は tick が状態を追うが、tick と
                // クリックの間にジョブが始まることはありうる（限界は
                // `refresh_models_window` の doc）。取得中の拒否は基盤側が持つ。
                let override_files = list.override_files.borrow();
                let context = models_context(
                    &transcriber,
                    &summarizer,
                    &downloader,
                    &config,
                    &override_files,
                );
                let outcome = if row_facts(&source, &context).busy {
                    DeleteOutcome::InUse
                } else {
                    match downloader.delete(&target) {
                        Ok(()) => DeleteOutcome::Deleted,
                        Err(err) => {
                            // 文言にフルパスは含めない（`docs/rules/security.md`）。
                            eprintln!("Skipping the model deletion because {err}");
                            DeleteOutcome::Failed
                        }
                    }
                };
                refresh(&models, delete_failure_notice(outcome));
            });
        }
    }
    {
        // 設定画面の「Manage models…」。開くたびに一覧を作り直す（ディスク走査はここだけ）。
        let models_weak = models_ui.as_weak();
        let list = model_list.clone();
        let downloader_for_open = model_downloader.clone();
        let transcriber_for_open = transcriber.clone();
        let summarizer_for_open = summarizer.clone();
        let config_for_open = Rc::clone(&config);
        // 初回表示でジオメトリを確定させたか（`show_window` が `&mut bool` を取るので RefCell）。
        let models_geometry = RefCell::new(false);
        ui.on_open_models_window(move || {
            let Some(models) = models_weak.upgrade() else {
                return;
            };
            refresh_models_window(
                &models,
                &list,
                &downloader_for_open,
                &transcriber_for_open,
                &summarizer_for_open,
                &config_for_open.borrow(),
                ModelsRefresh::AfterOperation(None),
            );
            show_window(
                models.window(),
                &mut models_geometry.borrow_mut(),
                slint::LogicalPosition::new(MODELS_X, MODELS_Y),
                slint::LogicalSize::new(MODELS_WIDTH, MODELS_HEIGHT),
            );
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
                sessions_model: Rc::clone(&sessions_model),
                transcript_segments: Rc::clone(&transcript_segments),
                transcriber: transcriber.clone(),
                summarizer: summarizer.clone(),
            },
            ModelsHandles {
                ui: models_ui.as_weak(),
                list: model_list.clone(),
                transcriber: transcriber.clone(),
                summarizer: summarizer.clone(),
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
    sessions: Rc<RefCell<Vec<recordings::RecordingSession>>>,
    sessions_model: Rc<slint::VecModel<SessionRow>>,
    transcript_segments: Rc<RefCell<Vec<transcript::TranscriptSegment>>>,
    transcriber: transcribe::TranscribeWorker,
    /// 詳細ペインの要約状態を tick で追従させるために読む（#81）。
    summarizer: summarize::SummarizeWorker,
}

/// モデル管理ウィンドウを tick で追従させるために必要なハンドル一式（`RecordingsHandles` と
/// 同じ理由でまとめる）。
struct ModelsHandles {
    ui: slint::Weak<ModelsWindow>,
    /// 一覧の素材と UI のモデル（組で持つ理由は `ModelListHandles`）。
    list: ModelListHandles,
    /// 削除できるかの判定に読む（ジョブがある間は消させない）。
    transcriber: transcribe::TranscribeWorker,
    summarizer: summarize::SummarizeWorker,
    /// モデルの取得状況（一覧の行の状態と、設定画面の状態行が読む）。**設定画面の更新もこの
    /// ハンドルを使う**（tick が両方を更新するので、引数を 2 つに割らない）。
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

        // 設定ウィンドウが開いている間だけ、選択中モデルの取得状況（ダウンロード進捗等）を
        // 状態行へ反映する（閉じているときは更新しない。変化したときだけ set して無駄な
        // 再描画を避ける）。
        if let Some(ui) = ui.upgrade()
            && ui.window().is_visible()
        {
            // 文言が変わったときだけ 3 つまとめて set する（進捗は文言のパーセントと同じ
            // 粒度で動くので、文言を代表にしてよい）。
            let status = whisper_model_status_line(&config.borrow(), &models.downloader);
            if ui.get_whisper_model_status() != status.text.as_str() {
                status.apply_whisper(&ui);
            }
            let summary_status = summary_model_status_line(&config.borrow(), &models.downloader);
            if ui.get_summary_model_status() != summary_status.text.as_str() {
                summary_status.apply_summary(&ui);
            }
        }

        // モデル管理ウィンドウが開いている間だけ、行の状態（取得の進捗・完了・失敗、ジョブの
        // 開始・終了）を追従させる。**ディスクは走査しない**（状態は状態マップだけで分かる。
        // `docs/rules/performance.md`）。取得が完了した行だけは実サイズと合計を追いつかせたい
        // ので、**記録が増えたときに 1 回だけ**走査し直す（毎 tick 走査しないためのラッチ。
        // 「記録は取得済みだが実体が無い」を条件にすると、外部でファイルを消された場合などに
        // 条件が解消せず走査が止まらない）。
        if let Some(models_ui) = models.ui.upgrade()
            && models_ui.window().is_visible()
        {
            let config = config.borrow();
            let downloaded = downloaded_ids(&models.list.sources.borrow(), &models.downloader);
            // 確認モーダルが開いている間は素材を作り直さない（行の並びが変わると、モーダルが
            // 指している行がずれる）。次の tick で拾う。
            let rescan = downloaded != *models.list.downloaded_seen.borrow()
                && !models_ui.get_show_delete_confirm();
            if rescan {
                // 走査し直すと `downloaded_seen` も更新される（`refresh_models_window`）。
                refresh_models_window(
                    &models_ui,
                    &models.list,
                    &models.downloader,
                    &models.transcriber,
                    &models.summarizer,
                    &config,
                    ModelsRefresh::Rescan,
                );
            } else {
                refresh_model_rows(
                    &models_ui,
                    &models.list,
                    &models.downloader,
                    &models.transcriber,
                    &models.summarizer,
                    &config,
                    ModelsRefresh::Poll,
                );
            }
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

/// Recordings ウィンドウの選択・再生表示を未選択状態へ初期化する
/// （ウィンドウを開いたとき・セッション削除後に共用する）。
///
/// 表示中だった文字起こし・議事録も手放す: どちらも発話由来の機微データで、詳細ペインが
/// 隠れている間もモデルとして持ち続ける理由が無い（削除したセッションの内容が残らないように。
/// `docs/rules/security.md`）。
fn clear_recordings_selection(
    rec: &RecordingsWindow,
    transcript_segments: &RefCell<Vec<transcript::TranscriptSegment>>,
) {
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
    let rows: Vec<SessionRow> = list
        .iter()
        .map(|session| SessionRow {
            datetime: session.display_datetime.clone().into(),
            has_mic: session.has_mic,
            has_system: session.has_system,
            transcript_status: transcript_display_status(
                handles.transcriber.status_of(&session.dir),
                session.has_transcript,
            ),
        })
        .collect();
    handles.sessions_model.set_vec(rows);
    // 開くたびに未選択・停止表示へ初期化する。
    clear_recordings_selection(rec, &handles.transcript_segments);
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

/// 手動（Recordings ウィンドウの Summarize）の議事録要約の依頼を組み立てる。設定値
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

/// 文字起こしに添える議事録要約の依頼を組み立てる。設定 OFF なら `None`（要約は走らない）。
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
/// 状態テキスト（`transcript_status_text`）が文形式なのに対し、こちらは他の空状態ラベル
/// （"No Recordings Yet" 等）と同じ Title Case の見出し形式にする（デザイン準拠）。
/// `Done` でセグメントが空になるのは JSON の欠落・破損時で、従来どおり未実施と同じ表示に落とす。
fn transcript_placeholder_text(display_status: TranscriptStatus) -> &'static str {
    match display_status {
        TranscriptStatus::Transcribing => TRANSCRIBING_LABEL,
        TranscriptStatus::Failed => "Transcription Failed",
        TranscriptStatus::NotTranscribed | TranscriptStatus::Done => "Not Transcribed Yet",
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

/// 議事録要約の表示状態（`ui/recordings-window.slint` の `SummaryStatus`）を合成する。
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
const SUMMARIZING_LABEL: &str = "Summarizing…";

/// 「キュー待ち」の表示ラベル。生成中と区別できる語にする: この間はまだ CPU を使っておらず、
/// 取り消せる（`SummarizeWorker::cancel`）。
///
/// 状態行（文形式）と縮退表示（Title Case）で大小が違うので、`SUMMARIZING_LABEL` のように
/// 1 つを共有できない（1 語のラベルは偶然どちらの流儀にも合っていた）。2 つに分ける。
const SUMMARY_QUEUED_LABEL: &str = "Waiting to summarize…";
const SUMMARY_QUEUED_PLACEHOLDER: &str = "Waiting to Summarize…";

/// 議事録要約の表示状態 → 詳細ペインの状態テキスト。
fn summary_status_text(display_status: SummaryStatus) -> &'static str {
    match display_status {
        SummaryStatus::NotSummarized => "Not summarized",
        SummaryStatus::Queued => SUMMARY_QUEUED_LABEL,
        SummaryStatus::Summarizing => SUMMARIZING_LABEL,
        SummaryStatus::Done => "Summarized",
        SummaryStatus::Failed => "Summarization failed",
    }
}

/// 議事録要約の表示状態 → Summary タブの縮退表示（行が無いとき）のラベル。状態テキストが
/// 文形式なのに対し、こちらは他の空状態ラベルと同じ Title Case にする
/// （`transcript_placeholder_text` と対称）。`Done` で行が空になるのは `summary.md` の欠落・
/// 破損・空のときで、未生成と同じ表示に落とす。
fn summary_placeholder_text(display_status: SummaryStatus) -> &'static str {
    match display_status {
        SummaryStatus::Queued => SUMMARY_QUEUED_PLACEHOLDER,
        SummaryStatus::Summarizing => SUMMARIZING_LABEL,
        SummaryStatus::Failed => "Summarization Failed",
        SummaryStatus::NotSummarized | SummaryStatus::Done => "Not Summarized Yet",
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

/// 詳細ペインの議事録要約の表示（状態テキスト・状態依存の配色・縮退ラベル）を反映する
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

/// 設定画面の ComboBox に並べる選択肢（`名前 — サイズ — 説明`）。whisper・要約 LLM で共用し、
/// 並び順はカタログのまま（選択位置はカタログ内インデックスで表す）。
///
/// 要約 LLM の説明行はこの文字列を Slint 側で選択位置から引いて出す（ComboBox の行は箱幅で
/// 省略されるため。`ui/app-window.slint` の `summary-models`）。
fn model_choices(catalog: &[model_download::ModelSpec]) -> slint::ModelRc<slint::SharedString> {
    Rc::new(slint::VecModel::<slint::SharedString>::from(
        catalog
            .iter()
            .map(|spec| {
                slint::SharedString::from(format!(
                    "{} — {} — {}",
                    spec.display_name,
                    model_download::format_size(spec.size_bytes),
                    spec.description
                ))
            })
            .collect::<Vec<_>>(),
    ))
    .into()
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
/// 文字起こし側に「自動文字起こし OFF なら取得しない」というゲートは**置かない**（既存挙動のまま）。
/// 設定画面の ComboBox は自動文字起こしが OFF だと無効なのでそこからは選択が起きず、モデル管理
/// ウィンドウの「Use」で選ぶのは先行取得の意図が明らかなため。要約側に `auto_summarize` のゲートが
/// あるのは、要約 LLM が whisper より大きく（最大 4.4 GB）、生成時に `ensure_model` が取得する
/// 経路が別にあるから。
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

/// 文字起こしに使う whisper モデルの取得状況を、設定画面の状態行テキストにする。
///
/// どのモデルかは ComboBox が示すので、ここは状態だけを出す。ただし上書き中は選んでも取得せず
/// そのファイルが使われるので、共用の「downloads automatically」だと表示と挙動が食い違う。
/// 取得の契機の正は `model_downloads_on_select`。
fn whisper_model_status_line(
    config: &Config,
    downloader: &model_download::ModelDownloader,
) -> ModelStatusLine {
    if model_path_override(model_download::ModelKind::Speech, config).is_some() {
        // 壊れてはいないが、選択が使われない状態なので caution（失敗ではない）。
        return ModelStatusLine::plain(MODEL_OVERRIDDEN_STATUS.to_owned(), StatusTone::Caution);
    }
    model_status_line(
        whisper_model::spec_or_default(&config.whisper_model),
        downloader,
    )
}

/// 議事録要約に使う LLM の取得状況を、設定画面の状態行テキストにする。
///
/// どのモデルかは ComboBox が示すので、ここは状態だけを出す。ただし取得の契機は whisper より
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
    if model_path_override(model_download::ModelKind::Summary, config).is_some() {
        return ModelStatusLine::plain(MODEL_OVERRIDDEN_STATUS.to_owned(), StatusTone::Caution);
    }
    let spec = summary_model::spec_or_default(&config.summary_model);
    if !model_downloads_on_select(model_download::ModelKind::Summary, config)
        && downloader.status_of(spec) == model_download::DownloadStatus::NotDownloaded
    {
        return ModelStatusLine::plain(
            format!(
                "Not downloaded — downloads when minutes are generated ({})",
                model_download::format_size(spec.size_bytes)
            ),
            StatusTone::Neutral,
        );
    }
    model_status_line(spec, downloader)
}

/// 受信済みバイトから進捗のパーセントを出す（設定画面の状態行と一覧の行で共用）。
///
/// `total` は Content-Length または既知サイズで常に正だが、防御的にゼロ除算を避ける。
/// Content-Length が実サイズより小さい異常時も 100% を超えて表示しない。
fn download_percent(received: u64, total: u64) -> u64 {
    (received.saturating_mul(100) / total.max(1)).min(100)
}

/// モデルの取得状況を、設定画面の状態行テキストにする（whisper / 要約 LLM で共用）。
/// 設定画面の状態行 1 本ぶん。文言だけでなく**意味（`tone`）と進捗**も一緒に運ぶ。
///
/// 色の対応表は Slint 側（`Style.tone-ink` / `Style.tone-mark`）に 1 つだけ置き、こちらは
/// 「どの意味か」を決める（`docs/rules/slint.md` の「状態→UI の対応表を三項連鎖にしない」）。
/// `progress` は 0.0〜1.0 で、取得中でなければ負にしてバーを出さない。
struct ModelStatusLine {
    text: String,
    tone: StatusTone,
    progress: f32,
}

impl ModelStatusLine {
    /// 進捗を持たない状態行。
    fn plain(text: String, tone: StatusTone) -> Self {
        Self {
            text,
            tone,
            progress: -1.0,
        }
    }

    /// 設定画面へ流し込む（3 つを必ずまとめて set する。片方だけ古い値が残らないように）。
    fn apply_whisper(&self, ui: &AppWindow) {
        ui.set_whisper_model_status(self.text.as_str().into());
        ui.set_whisper_model_tone(self.tone);
        ui.set_whisper_model_progress(self.progress);
    }

    fn apply_summary(&self, ui: &AppWindow) {
        ui.set_summary_model_status(self.text.as_str().into());
        ui.set_summary_model_tone(self.tone);
        ui.set_summary_model_progress(self.progress);
    }
}

/// モデルの取得状況を、設定画面の状態行にする（whisper / 要約 LLM で共用）。
///
/// **網羅 match** なので、`DownloadStatus` にバリアントを足したら文言と意味を決めるまで
/// コンパイルが通らない。
fn model_status_line(
    spec: &'static model_download::ModelSpec,
    downloader: &model_download::ModelDownloader,
) -> ModelStatusLine {
    match downloader.status_of(spec) {
        model_download::DownloadStatus::NotDownloaded => ModelStatusLine::plain(
            format!(
                // 自動取得の契機は複数ある（設定画面で選択した時点、または次の文字起こし・要約時）。
                // 共用の文言なので、どれかに限定した書き方にしない。
                "Not downloaded — downloads automatically ({})",
                model_download::format_size(spec.size_bytes)
            ),
            StatusTone::Neutral,
        ),
        model_download::DownloadStatus::Downloading { received, total } => ModelStatusLine {
            text: format!("Downloading… {}%", download_percent(received, total)),
            tone: StatusTone::Active,
            // 分母は Content-Length か既知サイズで常に正だが、防御的にゼロ除算を避ける。
            progress: received as f32 / total.max(1) as f32,
        },
        model_download::DownloadStatus::Downloaded => {
            ModelStatusLine::plain("Downloaded".to_owned(), StatusTone::Done)
        }
        model_download::DownloadStatus::Failed(reason) => {
            ModelStatusLine::plain(format!("Download failed: {reason}"), StatusTone::Danger)
        }
    }
}

/// 一覧を作り直す理由。**tick の作り直しがモーダルと通知を消さないよう**、意図を型で渡す
/// （`docs/rules/slint.md` の「『失敗したら表示を更新しない』は、ポーリング tick の上書きまで
/// 考える」。#117 は tick で作り直さない前提だったので同じ経路で済んでいた）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelsRefresh {
    /// ユーザー操作（開く・使う・取得・削除）の直後。**行の並びが変わる**ので、古い添字を
    /// 指したままにしないよう確認モーダルを畳み、通知を差し替える。
    AfterOperation(Option<&'static str>),
    /// tick が取得の完了を拾って素材を作り直す。ユーザー操作ではないので、**通知は保持**する
    /// （直前の失敗の理由を黙って消さない）。モーダルは開いている間そもそも走査しないので、
    /// ここへ来るときは閉じている。
    Rescan,
    /// tick のポーリング（走査なし）。並びは変わらないので、モーダル・添字・通知には触らない。
    Poll,
}

/// 通知をどう扱うか（`Option<Option<..>>` の 2 層にしないための型）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoticeUpdate {
    /// いま出ている通知をそのまま残す。
    Keep,
    /// この値へ差し替える（`None` は消す）。
    Set(Option<&'static str>),
}

impl ModelsRefresh {
    /// 確認モーダルと対象の添字を畳むか（ユーザー操作で並びが変わる経路だけ）。
    fn resets_modal(self) -> bool {
        matches!(self, Self::AfterOperation(_))
    }

    fn notice(self) -> NoticeUpdate {
        match self {
            Self::AfterOperation(notice) => NoticeUpdate::Set(notice),
            Self::Rescan | Self::Poll => NoticeUpdate::Keep,
        }
    }
}

/// モデル管理ウィンドウの中身を作り直す。**ディスクを走査する経路はここだけ**
/// （`docs/rules/performance.md`。100ms tick に走査を載せない）。呼ぶのは (1) ユーザー操作の直後
/// （開く・使う・取得・削除。`AfterOperation`）と (2) tick が取得の完了を拾ったとき（`Rescan`）。
///
/// 走査に失敗したら**通知**で伝える（カタログの行は必ず並ぶので、空表示では気づけない。
/// このとき行のサイズと状態はディスクの実体を反映しないので、その旨も文言に含める）。
fn refresh_models_window(
    models_ui: &ModelsWindow,
    handles: &ModelListHandles,
    downloader: &model_download::ModelDownloader,
    transcriber: &transcribe::TranscribeWorker,
    summarizer: &summarize::SummarizeWorker,
    config: &Config,
    cause: ModelsRefresh,
) {
    let (installed, scan_notice) = match model_download::installed_models() {
        Ok(found) => (found, None),
        Err(err) => {
            // 走査できないのを「1 つも無い」と混ぜない（実際は数 GB あるのに全行が未取得に
            // 見える）。フルパスはログにも表示にも出さない。
            eprintln!(
                "Showing no installed models because the models folder could not be listed: {err}"
            );
            (Vec::new(), Some(MODELS_UNREADABLE_NOTICE))
        }
    };
    reseed_model_sources(handles, installed, downloader, config);
    let cause = refresh_cause(cause, scan_notice);
    refresh_model_rows(
        models_ui,
        handles,
        downloader,
        transcriber,
        summarizer,
        config,
        cause,
    );
}

/// 走査の失敗を通知へ載せた作り直しの理由。**走査の失敗は操作の結果より先に伝える**（行の中身が
/// 信用できないため）。通知を保持する `Rescan` でも、これだけは差し替える。
fn refresh_cause(cause: ModelsRefresh, scan_notice: Option<&'static str>) -> ModelsRefresh {
    match (cause, scan_notice) {
        (_, Some(scan)) => ModelsRefresh::AfterOperation(Some(scan)),
        (cause, None) => cause,
    }
}

/// 走査の結果を素材へ入れ、**一緒に更新すべきもの**（tick のラッチと上書き先の解決）も同時に
/// 書く。3 つを別々に更新すると、片方だけ古い状態が生まれる（ラッチが古いと毎 tick 走査し直し、
/// 上書き先が古いと守るべき行を守らない）。
fn reseed_model_sources(
    handles: &ModelListHandles,
    installed: Vec<model_download::InstalledModel>,
    downloader: &model_download::ModelDownloader,
    config: &Config,
) {
    *handles.sources.borrow_mut() = model_row_sources(installed);
    *handles.downloaded_seen.borrow_mut() = downloaded_ids(&handles.sources.borrow(), downloader);
    *handles.override_files.borrow_mut() = OverrideFiles {
        speech: model_download::override_filename(config.whisper_model_path.as_deref()),
        summary: model_download::override_filename(config.summary_model_path.as_deref()),
    };
}

/// 行だけを組み直す（**ディスクを走査しない**。上書き先の解決も走査時に済ませてある）。開いている間の tick から呼び、取得の進捗・
/// 完了・失敗と、ジョブの開始・終了を表示へ反映する。
///
/// 変わった行だけ差し替える（`VecModel` を毎 tick 差し替えると全行の要素が再生成され、
/// ホバー・押下中の状態が飛んでクリックを取りこぼす。既存の一覧 tick と同じ流儀）。
fn refresh_model_rows(
    models_ui: &ModelsWindow,
    handles: &ModelListHandles,
    downloader: &model_download::ModelDownloader,
    transcriber: &transcribe::TranscribeWorker,
    summarizer: &summarize::SummarizeWorker,
    config: &Config,
    cause: ModelsRefresh,
) {
    let sources = handles.sources.borrow();
    let override_files = handles.override_files.borrow();
    let context = models_context(transcriber, summarizer, downloader, config, &override_files);
    let rows = model_rows(&sources, &context);

    if cause.resets_modal() {
        // 並びが変わるので、モーダルが古い行を指したままにしない。
        models_ui.set_show_delete_confirm(false);
        models_ui.set_delete_index(0);
    }
    if let NoticeUpdate::Set(notice) = cause.notice() {
        let notice = notice.unwrap_or_default();
        if models_ui.get_notice() != notice {
            models_ui.set_notice(notice.into());
        }
    }
    let total = models_total_text(&sources);
    if models_ui.get_total_text() != total.as_str() {
        models_ui.set_total_text(total.into());
    }
    apply_model_rows(&handles.rows, rows);
}

/// 行の反映の仕方。**全差し替えは行数が変わるときだけ**にする（毎回差し替えると全行の要素が
/// 再生成され、ホバー・押下中の状態が飛んでクリックを取りこぼす）。
#[derive(Debug, Clone, PartialEq, Eq)]
enum RowUpdate {
    /// モデルごと差し替える（行数が変わった＝素材を作り直した）。
    ReplaceAll,
    /// この添字の行だけ `set_row_data` する（空なら何もしない）。
    Changed(Vec<usize>),
}

/// いまの行と組み直した行を比べて、反映の仕方を決める（純粋関数）。
fn rows_to_update(current: &[ModelRow], next: &[ModelRow]) -> RowUpdate {
    if current.len() != next.len() {
        return RowUpdate::ReplaceAll;
    }
    RowUpdate::Changed(
        current
            .iter()
            .zip(next.iter())
            .enumerate()
            .filter_map(|(index, (current, next))| (current != next).then_some(index))
            .collect(),
    )
}

/// 組み直した行を UI のモデルへ反映する（判断は `rows_to_update`）。
fn apply_model_rows(model: &Rc<slint::VecModel<ModelRow>>, rows: Vec<ModelRow>) {
    use slint::Model as _;
    let current: Vec<ModelRow> = model.iter().collect();
    match rows_to_update(&current, &rows) {
        RowUpdate::ReplaceAll => model.set_vec(rows),
        RowUpdate::Changed(changed) => {
            // 添字で引かず、行を消費しながら該当だけ入れ替える（範囲外パニックの余地を残さない）。
            for (index, row) in rows.into_iter().enumerate() {
                if changed.contains(&index) {
                    model.set_row_data(index, row);
                }
            }
        }
    }
}

/// モデル管理ウィンドウの一覧を組むためのハンドル（素材と UI のモデル）。素材と行は同じ順序で
/// 1 対 1 なので、必ず組で持つ（別々に持つと**別のモデルを操作する**事故になる）。
/// `config.toml` のモデルパス上書きが `models/` 直下を指すときのファイル名（種別ごと）。
///
/// 上書きは config の手編集でしか変わらないので、**走査と同じタイミングで 1 回だけ解決**して持つ
/// （行ごと・tick ごとに `canonicalize` を叩かないため。`model_download::override_filename`）。
#[derive(Debug, Clone, Default)]
struct OverrideFiles {
    speech: Option<String>,
    summary: Option<String>,
}

#[derive(Clone)]
struct ModelListHandles {
    /// 一覧の行の素材（走査した時点のもの。tick は状態だけ組み直す）。
    sources: Rc<RefCell<Vec<ModelRowSource>>>,
    /// 上書き先のファイル名（走査と同じタイミングで解決する）。
    override_files: Rc<RefCell<OverrideFiles>>,
    /// UI が参照し続けるモデル（差し替えずに行単位で更新する）。
    rows: Rc<slint::VecModel<ModelRow>>,
    /// 直前に走査したときに「取得済みとして記録されていた」ID（tick が走査し直す契機の判定。
    /// `downloaded_ids`）。
    downloaded_seen: Rc<RefCell<Vec<&'static str>>>,
}

/// 一覧の 1 行の素材。**カタログ全件**（未取得を含む）と、`models/` にあるカタログ外のファイルを
/// 種別ごとに並べたもの（#138。#117 の「ディスクにあるものだけ」から広げた）。
///
/// 行の並びが UI のインデックスと 1 対 1 なので、ここで作った順序を UI へ渡すまで変えない
/// （並べ替えると**別のモデルを消す**）。
#[derive(Debug, Clone)]
enum ModelRowSource {
    /// 種別の見出し（ボタンを持たない行）。
    Heading(&'static str),
    /// カタログのモデル。`installed` はディスクに在るときのその実体。
    Catalog {
        kind: model_download::ModelKind,
        spec: &'static model_download::ModelSpec,
        installed: Option<model_download::InstalledModel>,
    },
    /// `models/` に在るがカタログに無いファイル（カタログ差し替え後の旧ファイルなど）。
    Extra(model_download::InstalledModel),
}

impl ModelRowSource {
    /// ディスクに在るならその実体（削除の対象。見出しと未取得の行は `None`）。
    fn installed(&self) -> Option<&model_download::InstalledModel> {
        match self {
            Self::Heading(_) => None,
            Self::Catalog { installed, .. } => installed.as_ref(),
            Self::Extra(installed) => Some(installed),
        }
    }
}

/// 種別の見出しの文言（**網羅 match**。種別を足したら見出しを書くまでコンパイルが通らない）。
fn kind_heading(kind: model_download::ModelKind) -> &'static str {
    match kind {
        model_download::ModelKind::Speech => "Transcription — Whisper",
        model_download::ModelKind::Summary => "Meeting minutes — LLM",
    }
}

/// 行の素材を組む。**カタログの登録簿の順**（種別ごとに見出し → カタログの並び）で、最後に
/// カタログ外のファイルを大きい順で置く。
fn model_row_sources(installed: Vec<model_download::InstalledModel>) -> Vec<ModelRowSource> {
    let mut sources: Vec<ModelRowSource> = Vec::new();
    for (kind, catalog, _) in model_download::REGISTERED_CATALOGS {
        sources.push(ModelRowSource::Heading(kind_heading(*kind)));
        for spec in catalog.iter() {
            sources.push(ModelRowSource::Catalog {
                kind: *kind,
                spec,
                installed: installed
                    .iter()
                    .find(|model| model.filename == spec.filename)
                    .cloned(),
            });
        }
    }
    // カタログに無いファイル（掃除できるように出す。#117）。`installed_models` が大きい順に
    // 返すので、その順を保つ。
    let mut extras = installed
        .into_iter()
        .filter(|model| model.catalog_id.is_none())
        .peekable();
    if extras.peek().is_some() {
        sources.push(ModelRowSource::Heading(EXTRA_FILES_HEADING));
        sources.extend(extras.map(ModelRowSource::Extra));
    }
    sources
}

/// カタログ外のファイルの見出し。
const EXTRA_FILES_HEADING: &str = "Other files in the models folder";

/// 行の状態を決めるための周辺状況（ワーカーと設定への照会を 1 回ずつに畳んでから渡す）。
struct ModelsContext<'a> {
    /// 文字起こしのジョブが在るか（キュー待ちを含む。`TranscribeWorker::has_pending_jobs`）。
    speech_busy: bool,
    /// 要約のジョブが在るか（同上）。
    summary_busy: bool,
    /// 設定でいま選ばれている ID。
    selected_speech: &'a str,
    selected_summary: &'a str,
    /// 設定のモデルパス上書きが `models/` 直下を指すなら、そのファイル名（**走査と同じ
    /// タイミングで解決したもの**。`OverrideFiles`）。
    speech_override_file: Option<&'a str>,
    summary_override_file: Option<&'a str>,
    /// その種別のモデルパスを上書きしているか（上書き中はカタログの選択が使われないので、
    /// 「使う」「取得する」を出さない）。
    speech_overridden: bool,
    summary_overridden: bool,
    downloader: &'a model_download::ModelDownloader,
}

fn models_context<'a>(
    transcriber: &transcribe::TranscribeWorker,
    summarizer: &summarize::SummarizeWorker,
    downloader: &'a model_download::ModelDownloader,
    config: &'a Config,
    override_files: &'a OverrideFiles,
) -> ModelsContext<'a> {
    ModelsContext {
        // 種別ごとに 1 回だけ照会する（行ごとにワーカーのロックを取らない）。
        speech_busy: transcriber.has_pending_jobs(),
        summary_busy: summarizer.has_pending_jobs(),
        selected_speech: whisper_model::spec_or_default(&config.whisper_model).id,
        selected_summary: summary_model::spec_or_default(&config.summary_model).id,
        speech_override_file: override_files.speech.as_deref(),
        summary_override_file: override_files.summary.as_deref(),
        speech_overridden: config.whisper_model_path.is_some(),
        summary_overridden: config.summary_model_path.is_some(),
        downloader,
    }
}

/// その種別のジョブが在るか。**網羅 match**にしてあるので、種別を足したら扱いを書くまで
/// コンパイルが通らない（`_ => false` で「消せる側」へ静かに落ちるのを防ぐ）。
fn kind_is_busy(context: &ModelsContext, kind: model_download::ModelKind) -> bool {
    match kind {
        model_download::ModelKind::Speech => context.speech_busy,
        model_download::ModelKind::Summary => context.summary_busy,
    }
}

/// 行の使用状況（表示と可否の分岐に使う。取得の状態＝`ModelStatus` とは別の軸）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowUsage {
    /// カタログの行で、選ばれていない。
    Idle,
    /// 設定でいま選ばれている。
    Selected,
    /// `config.toml` のモデルパス上書きが**この行のファイル**を指している。消しても再取得され
    /// ない（上書き中は `ensure_model` を通らない）。
    InConfig,
    /// `config.toml` がこの**種別**のモデルパスを上書きしている（この行のファイルではない）。
    /// カタログの選択は使われないので、選んでも取得しても意味が無い。
    Overridden,
    /// カタログに無いファイル。表示名も種別も分からず、消したらアプリでは戻せない。
    Unknown,
}

/// その種別のモデルパスが `config.toml` で上書きされているか（**網羅 match**。種別を足したら
/// 扱いを書くまでコンパイルが通らない）。
fn kind_overridden(context: &ModelsContext, kind: model_download::ModelKind) -> bool {
    match kind {
        model_download::ModelKind::Speech => context.speech_overridden,
        model_download::ModelKind::Summary => context.summary_overridden,
    }
}

/// 行ごとに求める事実（純粋関数。ワーカー・設定への照会は `ModelsContext` に畳んである）。
struct RowFacts {
    usage: RowUsage,
    /// その行のファイルを読むジョブが在るか（削除させない条件）。
    busy: bool,
}

fn row_facts(source: &ModelRowSource, context: &ModelsContext) -> RowFacts {
    let filename = match source {
        // 見出しはファイルを持たない（`""` をパスに合成しないよう、ここで打ち切る）。
        ModelRowSource::Heading(_) => {
            return RowFacts {
                usage: RowUsage::Idle,
                busy: false,
            };
        }
        ModelRowSource::Catalog { spec, .. } => spec.filename,
        ModelRowSource::Extra(installed) => installed.filename.as_str(),
    };
    let speech_override = context.speech_override_file == Some(filename);
    let summary_override = context.summary_override_file == Some(filename);
    // 関係する種別を**すべて**見る（同じファイルを 2 つの上書きが指していることもありうるので、
    // 先に一致した 1 つで打ち切らない）。
    let kind = match source {
        ModelRowSource::Catalog { kind, .. } => Some(*kind),
        _ => None,
    };
    // 上書き中の種別では、ジョブはカタログのファイルを開かない（`model_override` を使う）。
    // その行を「使用中で消せない」にすると、確実に使われていない数 GB を掃除できなくなる。
    let busy = kind
        .is_some_and(|kind| kind_is_busy(context, kind) && !kind_overridden(context, kind))
        || (speech_override && kind_is_busy(context, model_download::ModelKind::Speech))
        || (summary_override && kind_is_busy(context, model_download::ModelKind::Summary));
    let selected = match source {
        ModelRowSource::Catalog { kind, spec, .. } => match kind {
            model_download::ModelKind::Speech => spec.id == context.selected_speech,
            model_download::ModelKind::Summary => spec.id == context.selected_summary,
        },
        _ => false,
    };
    // この行のファイルが上書き先 → カタログの内外を問わず InConfig（消しても戻せない）。
    // そうでなくても種別が上書き中なら Overridden（選んでも取得しても使われない）。
    let kind_is_overridden = kind.is_some_and(|kind| kind_overridden(context, kind));
    let usage = if speech_override || summary_override {
        RowUsage::InConfig
    } else if kind_is_overridden {
        RowUsage::Overridden
    } else if matches!(source, ModelRowSource::Extra(_)) {
        RowUsage::Unknown
    } else if selected {
        RowUsage::Selected
    } else {
        RowUsage::Idle
    };
    RowFacts { usage, busy }
}

/// 行の取得の状態。**「取得済み」はディスクに実体があるときだけ**（`has_file` が正）で、記録は
/// 取得中・失敗の判別にだけ使う。
///
/// 記録（`Downloaded`）を優先しないのは、実体が無いのに「取得済み」と言うと削除できる行として
/// 出てしまい、確認モーダルが**無い容量の解放を約束**したうえで押しても何も起きないため。取得の
/// 完了直後はディスク走査の結果が古いが、そこは tick が「記録が増えた」ことを見て 1 回だけ走査し
/// 直して追いつかせる（`downloaded_ids`）。
fn model_status(has_file: bool, recorded: Option<&model_download::DownloadStatus>) -> ModelStatus {
    match (recorded, has_file) {
        (Some(model_download::DownloadStatus::Downloading { .. }), _) => ModelStatus::Downloading,
        // 失敗の記録は再試行までメモリに残る。ファイルが在るならそれは前回の成果物なので、
        // 「取得済み」を優先する（消せる状態として見せる）。
        (_, true) => ModelStatus::Installed,
        (Some(model_download::DownloadStatus::Failed(_)), false) => ModelStatus::Failed,
        (
            Some(
                model_download::DownloadStatus::Downloaded
                | model_download::DownloadStatus::NotDownloaded,
            )
            | None,
            false,
        ) => ModelStatus::NotDownloaded,
    }
}

/// 取得の状態の文言（**網羅 match**）。進捗は `Downloading`、理由は `Failed` のときだけ意味を持つ。
///
/// 失敗の理由まで出すのは、**取得の入口がこのウィンドウにもできた**ため（#138）。設定画面の
/// 状態行は選択中のモデルしか出さないので、ここに出さないと非選択モデルの失敗理由がどこにも
/// 出ない（`.app` では stderr も見えない）。理由は `insufficient_space_reason` などが作る文で、
/// パスを含まない（`docs/rules/security.md`）。
fn model_status_part(status: ModelStatus, percent: u64, reason: Option<&str>) -> String {
    match status {
        ModelStatus::NotDownloaded => "Not downloaded".to_owned(),
        ModelStatus::Downloading => format!("Downloading… {percent}%"),
        ModelStatus::Installed => "Downloaded".to_owned(),
        ModelStatus::Failed => match reason {
            Some(reason) => format!("Download failed: {reason}"),
            None => "Download failed".to_owned(),
        },
    }
}

/// 使用状況の文言（**網羅 match**。`Idle` は付け足す語が無い）。
fn model_usage_part(usage: RowUsage) -> Option<&'static str> {
    match usage {
        RowUsage::Idle => None,
        RowUsage::Selected => Some("selected in Settings"),
        RowUsage::InConfig => Some("set in config.toml"),
        RowUsage::Overridden => Some("not used because config.toml sets the model file"),
        RowUsage::Unknown => Some("not in the model catalog"),
    }
}

/// 削除できない理由の文言（ボタンが淡色になるだけでは理由が分からないので文字で出す）。
const MODEL_BUSY_PART: &str = "cannot be deleted while it is in use";

/// 行の状態テキスト。**3 つの表を `—` でつなぐ**（取得の状態・使用状況・使用中）。
/// 組み合わせを 1 つの表にすると状態 × 状況で行数が掛け算になるため、軸ごとに分ける。
fn model_row_status_text(
    status: ModelStatus,
    percent: u64,
    reason: Option<&str>,
    facts: &RowFacts,
) -> String {
    let mut parts = vec![model_status_part(status, percent, reason)];
    parts.extend(model_usage_part(facts.usage).map(ToOwned::to_owned));
    if facts.busy {
        parts.push(MODEL_BUSY_PART.to_owned());
    }
    parts.join(" — ")
}

/// 「使う」を出せるか。カタログの行で、いま選ばれておらず、その種別が `config.toml` で上書き
/// されていないとき（上書き中はカタログの選択が使われないので、押せても何も変わらない）。
fn can_use_row(source: &ModelRowSource, facts: &RowFacts) -> bool {
    matches!(source, ModelRowSource::Catalog { .. }) && facts.usage == RowUsage::Idle
}

/// 「取得する」を出せるか。ディスクに実体が無いカタログの行で、**その種別の上書き先が別のファイル
/// でない**とき。
///
/// 除くのは `Overridden`（＝上書き先が別のファイル）だけ。上書き中はカタログのモデルが使われない
/// ので、数 GB 落としても無駄になる。逆に `InConfig`（＝上書きがこの行のファイルを指している）は
/// **落とすことが動かす唯一の手段**なので出す（上書き中は `ensure_model` を通らず自動取得もされ
/// ない）。
///
/// **状態から導けない**ので Rust 側で決めて渡す（上書きの有無は状態の軸に含まれない）。
fn can_download_row(status: ModelStatus, source: &ModelRowSource, facts: &RowFacts) -> bool {
    matches!(source, ModelRowSource::Catalog { .. })
        && matches!(status, ModelStatus::NotDownloaded | ModelStatus::Failed)
        && facts.usage != RowUsage::Overridden
}

/// 削除できるか。**素材にファイルの実体がある**（＝消すものがある）かつその種別のジョブが無いとき。
///
/// `ModelStatus` ではなく素材を見るのは、状態は表示のための軸で「消せるか」の正ではないため。
/// **取得中に消させない**のは (1) `model_status` が `Downloading` を最優先にするので Slint 側が
/// Delete を出さないこと、(2) 最後の砦として `ModelDownloader::delete` が拒否すること——の 2 段で
/// （実体が既にある状態での再取得中は、ここは `true` を返しうる）。
///
/// **限界**: 押された時点の再確認（ワーカーのロック）と削除（`ModelDownloader` のロック）は別の
/// ロックなので、その間に投入されたジョブは拾えない（畳むにはワーカーをまたぐロックが要る）。
/// そうなってもカタログのモデルなら `ensure_model` が再取得するだけで、失うのは時間。
/// `config.toml` の上書き先だった場合はそのジョブが失敗する（確認モーダルがその旨を出している）。
fn can_delete_row(source: &ModelRowSource, facts: &RowFacts) -> bool {
    source.installed().is_some() && !facts.busy
}

/// 確認モーダルの説明テキスト。解放される容量と、**ゴミ箱へは入らない**こと、そして
/// 再取得できるかを出す（4.4GB の再取得は分オーダーかかるので、押す前に分かるようにする）。
fn model_delete_detail(usage: RowUsage, size_bytes: u64) -> String {
    let freed = format!(
        "This frees {}. The file is deleted permanently — it does not go to the Trash.",
        model_download::format_size(size_bytes)
    );
    match usage {
        RowUsage::Idle | RowUsage::Selected => {
            format!("{freed} It downloads again the next time it is needed.")
        }
        // 上書き中はカタログのモデルを取得しないので、上書きを外すまで戻ってこない。
        RowUsage::Overridden => {
            format!("{freed} It downloads again once config.toml no longer sets the model file.")
        }
        // 上書き中は `ensure_model` を通らないので、消すと設定を直すまでそのジョブが失敗する。
        RowUsage::InConfig => {
            format!("{freed} config.toml points at this file, so the app cannot download it again.")
        }
        // カタログ外は URL も SHA-256 も無いので、消したらアプリでは戻せない。
        RowUsage::Unknown => format!("{freed} The app cannot download this file again."),
    }
}

/// 行が 1 つも無いときに一覧の中央へ出す文言。**カタログの行は必ず並ぶので実際には出ない**
/// （表示の穴を残さないために置いてある）。走査の失敗は通知（`MODELS_UNREADABLE_NOTICE`）で
/// 伝える——空表示に混ぜると「まだ何も無い」と嘘を言うことになる。
const MODELS_EMPTY_TEXT: &str = "No models available";

/// 走査そのものに失敗したときの通知（カタログの行は必ず並ぶので、空表示では気づけない。
/// 行のサイズ・状態がディスクの実体を反映しないことも伝える）。
const MODELS_UNREADABLE_NOTICE: &str =
    "Could not list the models folder — sizes and states may be out of date.";

/// 押された時点で使用中だったときの通知（一覧の状態テキストと同じ事実を指すので、語を揃える）。
const MODEL_IN_USE_NOTICE: &str = "This model is in use right now — it was not deleted.";

/// 削除できなかった理由（`ModelsWindow::notice`）。理由まで出すのは「押しても無反応」に見せない
/// ため。使用中は UI で押させないのが基本なので、ここへ来るのは一覧が古かったときだけ。
const MODEL_DELETE_FAILED_NOTICE: &str = "Could not delete this model — see the log for details.";

/// 選択を保存できなかったときの通知（設定の永続化に失敗した場合）。
const MODEL_SELECT_FAILED_NOTICE: &str =
    "Could not change the model — the settings could not be saved (see the log).";

/// 削除の結果。bool 2 つで持つと `(busy, !failed)` のような**ありえない組み合わせ**を作れるので、
/// 3 値の enum にする（`docs/review-perspectives/rust-anti-patterns.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeleteOutcome {
    /// 消えた。
    Deleted,
    /// 押された時点で使用中だった（消していない）。
    InUse,
    /// 基盤側が拒否した・I/O で失敗した（理由はログ）。
    Failed,
}

/// 削除の結果から通知を決める（網羅 match なので、結果を足したら文言を書くまで通らない）。
fn delete_failure_notice(outcome: DeleteOutcome) -> Option<&'static str> {
    match outcome {
        DeleteOutcome::Deleted => None,
        DeleteOutcome::InUse => Some(MODEL_IN_USE_NOTICE),
        DeleteOutcome::Failed => Some(MODEL_DELETE_FAILED_NOTICE),
    }
}

/// 一覧の末尾に出す合計テキスト（**取得済みのぶんだけ**。未取得の行を足して「使っている容量」を
/// 膨らませない）。取得済みが無ければ空。
fn models_total_text(sources: &[ModelRowSource]) -> String {
    let installed: Vec<&model_download::InstalledModel> = sources
        .iter()
        .filter_map(ModelRowSource::installed)
        .collect();
    if installed.is_empty() {
        return String::new();
    }
    let total: u64 = installed.iter().map(|model| model.size_bytes).sum();
    format!(
        "{} {} — {}",
        installed.len(),
        if installed.len() == 1 {
            "model"
        } else {
            "models"
        },
        model_download::format_size(total)
    )
}

/// 一覧の行をまとめて組む。**`sources` と戻り値は同じ順**（UI のインデックスが操作の対象を
/// 指すので、ここで並べ替えない）。
fn model_rows(sources: &[ModelRowSource], context: &ModelsContext) -> Vec<ModelRow> {
    sources
        .iter()
        .map(|source| match source {
            ModelRowSource::Heading(title) => heading_row(title),
            _ => model_row(source, context),
        })
        .collect()
}

/// 種別の区切り行（操作を持たない）。
fn heading_row(title: &'static str) -> ModelRow {
    ModelRow {
        is_heading: true,
        name: title.into(),
        ..ModelRow::default()
    }
}

/// 1 行を組む（見出し以外）。表示名・説明・サイズはカタログとディスクの実体から、状態と可否は
/// `RowFacts` から決める。
fn model_row(source: &ModelRowSource, context: &ModelsContext) -> ModelRow {
    let facts = row_facts(source, context);
    let installed = source.installed();
    let recorded = match source {
        ModelRowSource::Catalog { spec, .. } => context.downloader.recorded_status(spec.id),
        // カタログ外のファイルは取得の記録を持たない（ダウンロードの宛先はカタログのみ）。
        _ => None,
    };
    let status = model_status(installed.is_some(), recorded.as_ref());
    let percent = match recorded {
        Some(model_download::DownloadStatus::Downloading { received, total }) => {
            download_percent(received, total)
        }
        _ => 0,
    };
    let failure_reason = match &recorded {
        Some(model_download::DownloadStatus::Failed(reason)) => Some(reason.as_str()),
        _ => None,
    };
    // サイズは、ディスクに在れば実ファイルの長さ（壊れた途中ファイルの実サイズを見せたい）、
    // 無ければカタログの値（取得前に大きさが分かるように）。
    let size_bytes = match (installed, source) {
        (Some(installed), _) => installed.size_bytes,
        (None, ModelRowSource::Catalog { spec, .. }) => spec.size_bytes,
        (None, _) => 0,
    };
    let (name, detail) = match source {
        ModelRowSource::Catalog { spec, .. } => (spec.display_name.to_owned(), spec.description),
        // カタログ外は表示名がファイル名になるので、2 行目は出さない（同じ文字列を 2 回並べない）。
        ModelRowSource::Extra(installed) => (installed.filename.clone(), ""),
        // 見出しは `heading_row` が組むので、ここには来ない。
        ModelRowSource::Heading(title) => ((*title).to_owned(), ""),
    };
    ModelRow {
        is_heading: false,
        name: name.into(),
        detail: detail.into(),
        size: model_download::format_size(size_bytes).into(),
        status_text: model_row_status_text(status, percent, failure_reason, &facts).into(),
        delete_detail: model_delete_detail(facts.usage, size_bytes).into(),
        status,
        can_use: can_use_row(source, &facts),
        can_download: can_download_row(status, source, &facts),
        can_delete: can_delete_row(source, &facts),
    }
}

/// 記録が「取得済み」になっているカタログ ID（素材の並び順）。
///
/// tick はディスクを走査しないので、取得が完了しても行のサイズと合計は追いつかない。この集合が
/// **前の tick から変わったとき**だけ 1 回走査し直す（`build_menu_event_handler` のラッチ）。
/// 「記録は取得済みなのに実体が無い」を条件にすると、外部でファイルを消された・走査に失敗した
/// といった**解消しない不一致**で毎 tick 走査が走り続ける。
fn downloaded_ids(
    sources: &[ModelRowSource],
    downloader: &model_download::ModelDownloader,
) -> Vec<&'static str> {
    sources
        .iter()
        .filter_map(|source| match source {
            ModelRowSource::Catalog { spec, .. } => matches!(
                downloader.recorded_status(spec.id),
                Some(model_download::DownloadStatus::Downloaded)
            )
            .then_some(spec.id),
            _ => None,
        })
        .collect()
}

/// 選び直しで取得を打ち切るモデルの ID（打ち切らないなら `None`）。
///
/// 同じモデルを選び直したときに打ち切らないためのガード。モデル管理ウィンドウの「Use」は
/// **選択中の行でも押せる**ので、ここを外すと「押したら自分のダウンロードが止まって数 GB を
/// 捨てる」ことになる（`request_download` が拾い直すので止まりっぱなしにはならないが、
/// 受信済みのぶんは戻らない）。
fn model_to_cancel_on_select<'a>(
    previous_id: &'a str,
    selected: &'static model_download::ModelSpec,
) -> Option<&'a str> {
    (previous_id != selected.id).then_some(previous_id)
}

/// 使うモデルを選び直して設定へ永続化する（設定画面の ComboBox とモデル管理ウィンドウの
/// 「Use」が**同じ経路**を通る）。成功したら `true`。
///
/// 選び直しで不要になった**前のモデルの取得は打ち切る**（#124。`cancel_download`）。
///
/// 取得を始めるかは `model_downloads_on_select` が決める（種別で条件が違う）。保存に失敗したら
/// 設定は変えない。
fn select_model(
    kind: model_download::ModelKind,
    spec: &'static model_download::ModelSpec,
    config: &Rc<RefCell<Config>>,
    downloader: &model_download::ModelDownloader,
) -> bool {
    let mut candidate = config.borrow().clone();
    // 上書きと同時に、直前に選んでいた ID を取り出す（打ち切る対象はこれ 1 つだけ。種別の全
    // モデルを止めると、管理ウィンドウの「Download」で別のモデルを明示的に落としている最中に
    // 選び直しただけでそれが消える。`ModelDownloader::cancel_download` の doc）。
    // **1 つの match にまとめる**のは、控える側と上書きする側で違うフィールドを触る事故を
    // 構文で塞ぐため。
    let superseded_id = match kind {
        model_download::ModelKind::Speech => {
            std::mem::replace(&mut candidate.whisper_model, spec.id.to_owned())
        }
        model_download::ModelKind::Summary => {
            std::mem::replace(&mut candidate.summary_model, spec.id.to_owned())
        }
    };
    if let Err(err) = candidate.save() {
        // どの種別の話か分かるようにする（3 つの入口＝両方の ComboBox とモデル管理ウィンドウの
        // 「Use」が同じ関数を通るので、種別が無いと調査で効かない）。
        eprintln!(
            "Not changing the {} model because saving the settings failed: {err}",
            spec.kind
        );
        return false;
    }
    // 取得の可否は保存する値で決める（移動する前に読む）。取得済み・DL 中は
    // request_download 側が早期 return する。
    let downloads_now = model_downloads_on_select(kind, &candidate);
    *config.borrow_mut() = candidate;
    // 選び直したので、前に選んでいたモデルの取得はもう要らない（#124）。ここでやるのは
    // フラグを立てることだけで、担当スレッドが気づくのは次のチャンクを読む手前。
    // **新しい取得を頼む前に**立てるのが要点で、空き容量の事前確認は打ち切り済みの取得を
    // 数えないので（`in_flight_remaining_bytes`）、新しいほうが要らない容量を要求しなくなる。
    //
    // 同じモデルを選び直したときは打ち切らない（自分の取得を止めて数 GB を捨てることになる）。
    if let Some(id) = model_to_cancel_on_select(&superseded_id, spec) {
        downloader.cancel_download(id);
    }
    if downloads_now {
        downloader.request_download(spec);
    }
    true
}

/// 設定画面の ComboBox の選択位置と状態行を、いまの設定に合わせて更新する。
///
/// 起動時の初期化と、モデル管理ウィンドウから選び直したときの追従が**同じ経路**を通る（状態行の
/// 導出が種別ごとに増えても、初期化だけ古い経路に取り残されないようにするため）。
fn apply_model_selection_to_settings(
    ui: &AppWindow,
    config: &Config,
    downloader: &model_download::ModelDownloader,
) {
    ui.set_whisper_model_index(whisper_model::model_index(&config.whisper_model) as i32);
    ui.set_summary_model_index(summary_model::model_index(&config.summary_model) as i32);
    ui.set_whisper_model_overridden(
        model_path_override(model_download::ModelKind::Speech, config).is_some(),
    );
    ui.set_summary_model_overridden(
        model_path_override(model_download::ModelKind::Summary, config).is_some(),
    );
    whisper_model_status_line(config, downloader).apply_whisper(ui);
    summary_model_status_line(config, downloader).apply_summary(ui);
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
    use super::{
        ModelStatus, StatusTone, SummaryStatus, TranscriptStatus, app_version_text,
        breathing_level, model_choices, model_downloads_on_select, model_status_line,
        model_to_cancel_on_select, playback_progress, seek_position_from_ratio,
        summary_display_status, summary_model_status_line, summary_placeholder_text, summary_rows,
        summary_status_text, transcript_display_status, transcript_placeholder_text,
        transcript_status_text, whisper_model_status_line,
    };
    use crate::transcribe::TranscribeStatus;
    use std::time::Duration;

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
            "Not Transcribed Yet"
        );
        assert_eq!(
            transcript_placeholder_text(TranscriptStatus::Transcribing),
            "Transcribing…"
        );
        assert_eq!(
            transcript_placeholder_text(TranscriptStatus::Done),
            "Not Transcribed Yet"
        );
        assert_eq!(
            transcript_placeholder_text(TranscriptStatus::Failed),
            "Transcription Failed"
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
            "Not summarized"
        );
        assert_eq!(
            summary_status_text(SummaryStatus::Queued),
            "Waiting to summarize…"
        );
        assert_eq!(
            summary_status_text(SummaryStatus::Summarizing),
            "Summarizing…"
        );
        assert_eq!(summary_status_text(SummaryStatus::Done), "Summarized");
        assert_eq!(
            summary_status_text(SummaryStatus::Failed),
            "Summarization failed"
        );
    }

    /// 縮退表示ラベル。Done で行が空になるのは `summary.md` の欠落・破損・空の経路で、
    /// 未生成と同じラベルに落とす。
    #[test]
    fn summary_placeholder_text_covers_all_states() {
        assert_eq!(
            summary_placeholder_text(SummaryStatus::NotSummarized),
            "Not Summarized Yet"
        );
        assert_eq!(
            summary_placeholder_text(SummaryStatus::Queued),
            "Waiting to Summarize…"
        );
        assert_eq!(
            summary_placeholder_text(SummaryStatus::Summarizing),
            "Summarizing…"
        );
        assert_eq!(
            summary_placeholder_text(SummaryStatus::Done),
            "Not Summarized Yet"
        );
        assert_eq!(
            summary_placeholder_text(SummaryStatus::Failed),
            "Summarization Failed"
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
            model_status_line(spec, &downloader).text,
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
            model_status_line(spec, &downloader).text,
            "Downloading… 100%"
        );

        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::Downloaded);
        assert_eq!(model_status_line(spec, &downloader).text, "Downloaded");

        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Failed("boom".into()),
        );
        assert_eq!(
            model_status_line(spec, &downloader).text,
            "Download failed: boom"
        );

        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::NotDownloaded);
        assert_eq!(
            model_status_line(spec, &downloader).text,
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
        let line = model_status_line(spec, &downloader);
        assert_eq!(line.tone, StatusTone::Neutral);
        assert!(line.progress < 0.0, "no bar before the download starts");

        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Downloading {
                received: 25,
                total: 100,
            },
        );
        let line = model_status_line(spec, &downloader);
        assert_eq!(line.tone, StatusTone::Active);
        assert!((line.progress - 0.25).abs() < f32::EPSILON);

        downloader.set_status_for_test(spec, crate::model_download::DownloadStatus::Downloaded);
        let line = model_status_line(spec, &downloader);
        assert_eq!(line.tone, StatusTone::Done);
        assert!(line.progress < 0.0);

        downloader.set_status_for_test(
            spec,
            crate::model_download::DownloadStatus::Failed("boom".into()),
        );
        assert_eq!(
            model_status_line(spec, &downloader).tone,
            StatusTone::Danger
        );

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

    /// 選び直しで打ち切るのは「別のモデルに変わったとき」だけ。同じモデルを選び直しても
    /// （モデル管理ウィンドウの「Use」は選択中の行でも押せる）自分の取得は止めない。
    #[test]
    fn model_to_cancel_on_select_skips_an_unchanged_selection() {
        let tiny = crate::whisper_model::spec_for("tiny").expect("tiny is in the catalog");
        assert_eq!(model_to_cancel_on_select("small", tiny), Some("small"));
        assert_eq!(model_to_cancel_on_select("tiny", tiny), None);
        // カタログ外の手編集値から選び直した場合も、その ID の取得を打ち切る対象にする
        // （走っていなければ `cancel_download` が false を返すだけ）。
        assert_eq!(
            model_to_cancel_on_select("no-such-model", tiny),
            Some("no-such-model")
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

        // 上書き中は取得状況によらず同じ文言（要約側と同じ表現にする）。
        let overridden = crate::config::Config {
            whisper_model_path: Some(std::path::PathBuf::from("/tmp/ggml-small.bin")),
            ..choosing_tiny
        };
        assert_eq!(
            whisper_model_status_line(&overridden, &downloader).text,
            "Using the model file set in config.toml"
        );
        downloader.set_status_for_test(tiny, crate::model_download::DownloadStatus::Downloaded);
        assert_eq!(
            whisper_model_status_line(&overridden, &downloader).text,
            "Using the model file set in config.toml"
        );
    }

    /// カタログ外のファイル 1 件（`models/` に在るが登録簿に無い）。
    fn extra_file(filename: &str, size: u64) -> crate::model_download::InstalledModel {
        crate::model_download::InstalledModel {
            filename: filename.to_owned(),
            size_bytes: size,
            kind: None,
            catalog_id: None,
        }
    }

    /// カタログの spec がディスクに在る状態の `InstalledModel`。
    fn installed_spec(
        kind: crate::model_download::ModelKind,
        spec: &'static crate::model_download::ModelSpec,
        size: u64,
    ) -> crate::model_download::InstalledModel {
        crate::model_download::InstalledModel {
            filename: spec.filename.to_owned(),
            size_bytes: size,
            kind: Some(kind),
            catalog_id: Some(spec.id),
        }
    }

    fn speech_spec() -> &'static crate::model_download::ModelSpec {
        crate::whisper_model::default_spec()
    }

    fn summary_spec() -> &'static crate::model_download::ModelSpec {
        crate::summary_model::default_spec()
    }

    /// 素材は**カタログ全件**（未取得を含む）を種別ごとに並べ、最後にカタログ外のファイルを置く。
    /// 見出しは種別の区切りとして行になる（`docs/rules/slint.md` の `SummaryRow` と同じ流儀）。
    #[test]
    fn model_row_sources_list_every_catalog_entry_with_headings() {
        let sources = super::model_row_sources(vec![
            installed_spec(crate::model_download::ModelKind::Speech, speech_spec(), 100),
            extra_file("left-over.bin", 20),
        ]);

        // 見出しは登録簿の種別ごとに 1 つ＋カタログ外のぶん。
        let headings: Vec<&str> = sources
            .iter()
            .filter_map(|source| match source {
                super::ModelRowSource::Heading(title) => Some(*title),
                _ => None,
            })
            .collect();
        assert_eq!(
            headings,
            vec![
                super::kind_heading(crate::model_download::ModelKind::Speech),
                super::kind_heading(crate::model_download::ModelKind::Summary),
                super::EXTRA_FILES_HEADING,
            ]
        );

        // カタログ全件が並ぶ（未取得も）。件数は登録簿から数える。
        let catalog_rows = sources
            .iter()
            .filter(|source| matches!(source, super::ModelRowSource::Catalog { .. }))
            .count();
        let expected: usize = crate::model_download::REGISTERED_CATALOGS
            .iter()
            .map(|(_, catalog, _)| catalog.len())
            .sum();
        assert_eq!(catalog_rows, expected);

        // ディスクに在るものだけ `installed` が入る。
        let installed_names: Vec<&str> = sources
            .iter()
            .filter_map(super::ModelRowSource::installed)
            .map(|model| model.filename.as_str())
            .collect();
        assert_eq!(
            installed_names,
            vec![speech_spec().filename, "left-over.bin"]
        );
    }

    /// カタログ外のファイルが無ければ、その見出しも出さない（空の区切りを残さない）。
    #[test]
    fn model_row_sources_skip_the_extra_heading_when_there_are_none() {
        let sources = super::model_row_sources(Vec::new());
        assert!(
            !sources.iter().any(|source| matches!(
                source,
                super::ModelRowSource::Heading(super::EXTRA_FILES_HEADING)
            )),
            "the extra heading must not appear without extra files"
        );
    }

    fn context<'a>(
        downloader: &'a crate::model_download::ModelDownloader,
        speech_busy: bool,
        summary_busy: bool,
    ) -> super::ModelsContext<'a> {
        super::ModelsContext {
            speech_busy,
            summary_busy,
            selected_speech: crate::whisper_model::DEFAULT_MODEL_ID,
            selected_summary: crate::summary_model::DEFAULT_MODEL_ID,
            speech_override_file: None,
            summary_override_file: None,
            speech_overridden: false,
            summary_overridden: false,
            downloader,
        }
    }

    /// 行の状態は**取得の軸**（`ModelStatus`）と**使用の軸**（`RowUsage`）に分かれる。
    /// 「取得済み」はディスクに実体があるときだけで、記録は取得中・失敗の判別に使う。
    #[test]
    fn model_status_says_installed_only_with_a_file() {
        use crate::model_download::DownloadStatus;
        assert_eq!(super::model_status(false, None), ModelStatus::NotDownloaded);
        assert_eq!(super::model_status(true, None), ModelStatus::Installed);
        assert_eq!(
            super::model_status(
                false,
                Some(&DownloadStatus::Downloading {
                    received: 1,
                    total: 2
                })
            ),
            ModelStatus::Downloading
        );
        // 記録が取得済みでも、ディスクに実体が無ければ「取得済み」とは言わない（言うと
        // 削除できる行として出てしまい、確認モーダルが無い容量の解放を約束する）。
        assert_eq!(
            super::model_status(false, Some(&DownloadStatus::Downloaded)),
            ModelStatus::NotDownloaded
        );
        assert_eq!(
            super::model_status(false, Some(&DownloadStatus::Failed("boom".to_owned()))),
            ModelStatus::Failed
        );
        // 失敗の記録が残っていても、ファイルが在るなら前回の成果物として消せる状態にする。
        assert_eq!(
            super::model_status(true, Some(&DownloadStatus::Failed("boom".to_owned()))),
            ModelStatus::Installed
        );
    }

    /// 使用状況は「選択中・config 上書き・カタログ外・それ以外」。上書きは**選択より先**に見る
    /// （上書き中はカタログの選択が使われないため）。
    #[test]
    fn row_facts_tell_selection_config_and_busy_apart() {
        let downloader = crate::model_download::ModelDownloader::new();
        let selected = super::ModelRowSource::Catalog {
            kind: crate::model_download::ModelKind::Speech,
            spec: speech_spec(),
            installed: None,
        };
        // 既定 ID を選択中にしてあるので、この行は「選択中」。
        assert_eq!(
            super::row_facts(&selected, &context(&downloader, false, false)).usage,
            super::RowUsage::Selected
        );
        // その種別のジョブがあれば busy（削除させない）。
        assert!(super::row_facts(&selected, &context(&downloader, true, false)).busy);
        assert!(!super::row_facts(&selected, &context(&downloader, false, true)).busy);

        // カタログ外は Unknown。
        let extra = super::ModelRowSource::Extra(extra_file("left-over.bin", 10));
        assert_eq!(
            super::row_facts(&extra, &context(&downloader, true, true)).usage,
            super::RowUsage::Unknown
        );
        assert!(
            !super::row_facts(&extra, &context(&downloader, true, true)).busy,
            "a file no job reads must not be treated as busy"
        );
    }

    /// `config.toml` の上書きが**この行のファイル**を指しているときの扱い。選択中より先に見て
    /// `InConfig` にし、**その種別のジョブがある間は消させない**（ジョブが読んでいるファイル）。
    #[test]
    fn an_override_target_is_in_config_and_protected_while_jobs_run() {
        let downloader = crate::model_download::ModelDownloader::new();
        let filename = speech_spec().filename;
        let source = super::ModelRowSource::Catalog {
            kind: crate::model_download::ModelKind::Speech,
            spec: speech_spec(),
            installed: Some(installed_spec(
                crate::model_download::ModelKind::Speech,
                speech_spec(),
                10,
            )),
        };

        let mut idle = context(&downloader, false, false);
        idle.speech_override_file = Some(filename);
        idle.speech_overridden = true;
        let facts = super::row_facts(&source, &idle);
        assert_eq!(
            facts.usage,
            super::RowUsage::InConfig,
            "the override target is reported as such, not as the Settings selection"
        );
        assert!(!facts.busy, "no jobs are running");
        assert!(super::can_delete_row(&source, &facts));
        // 上書き先は落とすことが動かす唯一の手段なので、取得は出す。
        assert!(super::can_download_row(
            ModelStatus::NotDownloaded,
            &source,
            &facts
        ));

        let mut busy = context(&downloader, true, false);
        busy.speech_override_file = Some(filename);
        busy.speech_overridden = true;
        let busy_facts = super::row_facts(&source, &busy);
        assert!(
            busy_facts.busy,
            "the file a running job reads must not be deletable"
        );
        assert!(!super::can_delete_row(&source, &busy_facts));
    }

    /// 走査の失敗は、tick 由来の作り直し（通知を保持する `Rescan`）でも通知へ載せる
    /// （行のサイズ・状態がディスクを反映しないので、黙っていると気づけない）。
    #[test]
    fn refresh_cause_reports_a_failed_scan_even_from_the_tick() {
        assert_eq!(
            super::refresh_cause(super::ModelsRefresh::Rescan, Some("scan failed")),
            super::ModelsRefresh::AfterOperation(Some("scan failed"))
        );
        assert_eq!(
            super::refresh_cause(super::ModelsRefresh::Rescan, None),
            super::ModelsRefresh::Rescan,
            "a successful rescan keeps the notice"
        );
        // 走査の失敗は操作の結果より先（行の中身が信用できない）。
        assert_eq!(
            super::refresh_cause(
                super::ModelsRefresh::AfterOperation(Some("delete failed")),
                Some("scan failed")
            ),
            super::ModelsRefresh::AfterOperation(Some("scan failed"))
        );
    }

    /// 走査したら**ラッチと上書き先の解決も一緒に**更新する（別々に書くと、ラッチが古くて毎 tick
    /// 走査し直す／上書き先が古くて守るべき行を守らない、という食い違いが生まれる）。
    #[test]
    fn reseeding_the_sources_updates_the_latch() {
        let downloader = crate::model_download::ModelDownloader::new();
        use std::cell::RefCell;
        use std::rc::Rc;
        let handles = super::ModelListHandles {
            sources: Rc::new(RefCell::new(Vec::new())),
            override_files: Rc::new(RefCell::new(super::OverrideFiles::default())),
            rows: Rc::new(slint::VecModel::default()),
            downloaded_seen: Rc::new(RefCell::new(Vec::new())),
        };
        downloader.set_status_for_test(
            speech_spec(),
            crate::model_download::DownloadStatus::Downloaded,
        );

        super::reseed_model_sources(
            &handles,
            vec![installed_spec(
                crate::model_download::ModelKind::Speech,
                speech_spec(),
                10,
            )],
            &downloader,
            &crate::config::Config::default(),
        );
        assert_eq!(
            *handles.downloaded_seen.borrow(),
            super::downloaded_ids(&handles.sources.borrow(), &downloader),
            "the latch must match the sources it was seeded from"
        );
        assert!(!handles.sources.borrow().is_empty());
    }

    /// 「使う」を出すのは、カタログの行で選ばれていないときだけ（見出し・カタログ外・選択中・
    /// config 上書き中は出さない）。
    #[test]
    fn can_use_row_only_offers_unselected_catalog_rows() {
        let idle = super::ModelRowSource::Catalog {
            kind: crate::model_download::ModelKind::Speech,
            spec: speech_spec(),
            installed: None,
        };
        let facts = |usage| super::RowFacts { usage, busy: false };
        assert!(super::can_use_row(&idle, &facts(super::RowUsage::Idle)));
        assert!(!super::can_use_row(
            &idle,
            &facts(super::RowUsage::Selected)
        ));
        assert!(!super::can_use_row(
            &idle,
            &facts(super::RowUsage::InConfig)
        ));
        // カタログ外・見出しはそもそも選べない。
        let extra = super::ModelRowSource::Extra(extra_file("left-over.bin", 10));
        assert!(!super::can_use_row(
            &extra,
            &facts(super::RowUsage::Unknown)
        ));
        let heading = super::ModelRowSource::Heading("Transcription");
        assert!(!super::can_use_row(&heading, &facts(super::RowUsage::Idle)));
    }

    /// 削除できるのはディスクに在って、その種別のジョブが無いときだけ。
    #[test]
    fn can_delete_row_requires_a_file_and_no_jobs() {
        let idle = super::RowFacts {
            usage: super::RowUsage::Idle,
            busy: false,
        };
        let busy = super::RowFacts {
            usage: super::RowUsage::Idle,
            busy: true,
        };
        // 素材に実体があるかで決まる（状態ではない: 記録が取得済みでもファイルが無ければ
        // 消すものが無い）。
        let with_file = super::ModelRowSource::Catalog {
            kind: crate::model_download::ModelKind::Speech,
            spec: speech_spec(),
            installed: Some(installed_spec(
                crate::model_download::ModelKind::Speech,
                speech_spec(),
                10,
            )),
        };
        let without_file = super::ModelRowSource::Catalog {
            kind: crate::model_download::ModelKind::Speech,
            spec: speech_spec(),
            installed: None,
        };
        assert!(super::can_delete_row(&with_file, &idle));
        assert!(!super::can_delete_row(&with_file, &busy));
        assert!(!super::can_delete_row(&without_file, &idle));
        assert!(!super::can_delete_row(
            &super::ModelRowSource::Heading("Transcription"),
            &idle
        ));
    }

    /// 取得の状態の文言（全バリアント）。進捗は `Downloading` のときだけ出る。
    #[test]
    fn model_status_part_covers_all_states() {
        assert_eq!(
            super::model_status_part(ModelStatus::NotDownloaded, 0, None),
            "Not downloaded"
        );
        assert_eq!(
            super::model_status_part(ModelStatus::Downloading, 42, None),
            "Downloading… 42%"
        );
        assert_eq!(
            super::model_status_part(ModelStatus::Installed, 0, None),
            "Downloaded"
        );
        // 取得の入口がこのウィンドウにもできたので、**失敗の理由まで行に出す**（設定画面の
        // 状態行は選択中のモデルしか出さない）。
        assert_eq!(
            super::model_status_part(ModelStatus::Failed, 0, Some("not enough free disk space")),
            "Download failed: not enough free disk space"
        );
        assert_eq!(
            super::model_status_part(ModelStatus::Failed, 0, None),
            "Download failed"
        );
    }

    /// 使用状況の文言（全バリアント）。`Idle` は付け足す語が無い。
    #[test]
    fn model_usage_part_covers_all_states() {
        assert_eq!(super::model_usage_part(super::RowUsage::Idle), None);
        assert_eq!(
            super::model_usage_part(super::RowUsage::Selected),
            Some("selected in Settings")
        );
        assert_eq!(
            super::model_usage_part(super::RowUsage::InConfig),
            Some("set in config.toml")
        );
        assert_eq!(
            super::model_usage_part(super::RowUsage::Overridden),
            Some("not used because config.toml sets the model file")
        );
        assert_eq!(
            super::model_usage_part(super::RowUsage::Unknown),
            Some("not in the model catalog")
        );
    }

    /// 状態テキストは 3 つの軸を `—` でつなぐ（削除できない理由もここに出る）。
    #[test]
    fn model_row_status_text_joins_the_axes() {
        let idle = super::RowFacts {
            usage: super::RowUsage::Idle,
            busy: false,
        };
        assert_eq!(
            super::model_row_status_text(ModelStatus::NotDownloaded, 0, None, &idle),
            "Not downloaded"
        );
        let selected_busy = super::RowFacts {
            usage: super::RowUsage::Selected,
            busy: true,
        };
        assert_eq!(
            super::model_row_status_text(ModelStatus::Installed, 0, None, &selected_busy),
            "Downloaded — selected in Settings — cannot be deleted while it is in use"
        );
        let in_config = super::RowFacts {
            usage: super::RowUsage::InConfig,
            busy: false,
        };
        assert_eq!(
            super::model_row_status_text(ModelStatus::Downloading, 7, None, &in_config),
            "Downloading… 7% — set in config.toml"
        );
    }

    /// 行の並びは素材の順のまま（UI のインデックスが操作対象を指すので、ここで並べ替えると
    /// **別のモデルを消す・別のモデルを選ぶ**）。見出しは操作を持たない。
    #[test]
    fn model_rows_keep_the_order_of_the_sources() {
        let downloader = crate::model_download::ModelDownloader::new();
        let sources = super::model_row_sources(vec![
            installed_spec(
                crate::model_download::ModelKind::Summary,
                summary_spec(),
                4_000_000_000,
            ),
            extra_file("left-over.bin", 20),
        ]);
        let rows = super::model_rows(&sources, &context(&downloader, false, false));

        assert_eq!(rows.len(), sources.len());
        for (row, source) in rows.iter().zip(sources.iter()) {
            match source {
                super::ModelRowSource::Heading(title) => {
                    assert!(row.is_heading, "{title} should be a heading row");
                    assert_eq!(row.name, *title);
                    assert!(!row.can_use && !row.can_delete);
                }
                super::ModelRowSource::Catalog { spec, .. } => {
                    assert!(!row.is_heading);
                    assert_eq!(row.name, spec.display_name);
                    assert_eq!(row.detail, spec.description);
                }
                super::ModelRowSource::Extra(installed) => {
                    assert!(!row.is_heading);
                    assert_eq!(row.name, installed.filename.as_str());
                    assert_eq!(row.detail, "", "an unknown file has no description");
                }
            }
        }

        // 取得済みの行は削除でき、未取得の行は取得できる（状態から Slint 側が出し分ける）。
        let installed_row = rows
            .iter()
            .find(|row| row.name == summary_spec().display_name)
            .expect("the installed summary model should have a row");
        assert_eq!(installed_row.status, ModelStatus::Installed);
        assert!(installed_row.can_delete);
        let not_downloaded = rows
            .iter()
            .find(|row| row.name == speech_spec().display_name)
            .expect("the speech model should have a row");
        assert_eq!(not_downloaded.status, ModelStatus::NotDownloaded);
        assert!(!not_downloaded.can_delete);
    }

    /// 確認モーダルの説明は、解放される容量と「ゴミ箱に入らない」ことを必ず言う。再取得できるかは
    /// 使用状況で変わるので、そこだけ文言を分ける。
    #[test]
    fn model_delete_detail_tells_the_freed_space_and_whether_it_returns() {
        let catalog = super::model_delete_detail(super::RowUsage::Selected, 1_624_555_275);
        assert_eq!(
            catalog,
            "This frees 1.5 GB. The file is deleted permanently — it does not go to the Trash. \
             It downloads again the next time it is needed."
        );
        let unknown = super::model_delete_detail(super::RowUsage::Unknown, 77_691_713);
        assert_eq!(
            unknown,
            "This frees 74 MB. The file is deleted permanently — it does not go to the Trash. \
             The app cannot download this file again."
        );
        // config が指しているファイルは、カタログに載っていても再取得されない。
        let pointed_at = super::model_delete_detail(super::RowUsage::InConfig, 77_691_713);
        assert_eq!(
            pointed_at,
            "This frees 74 MB. The file is deleted permanently — it does not go to the Trash. \
             config.toml points at this file, so the app cannot download it again."
        );
    }

    /// 削除の結果ごとの通知（全バリアント）。
    #[test]
    fn delete_failure_notice_covers_all_outcomes() {
        assert_eq!(
            super::delete_failure_notice(super::DeleteOutcome::InUse),
            Some(super::MODEL_IN_USE_NOTICE)
        );
        assert_eq!(
            super::delete_failure_notice(super::DeleteOutcome::Failed),
            Some(super::MODEL_DELETE_FAILED_NOTICE)
        );
        assert_eq!(
            super::delete_failure_notice(super::DeleteOutcome::Deleted),
            None
        );
    }

    /// 合計は**取得済みのぶんだけ**（未取得の行を足して「使っている容量」を膨らませない）。
    #[test]
    fn models_total_text_counts_only_installed_models() {
        // カタログ全件が並んでいても、ディスクに無ければ合計に入らない。
        let none_installed = super::model_row_sources(Vec::new());
        assert_eq!(super::models_total_text(&none_installed), "");

        let one = super::model_row_sources(vec![installed_spec(
            crate::model_download::ModelKind::Speech,
            speech_spec(),
            77_691_713,
        )]);
        assert_eq!(super::models_total_text(&one), "1 model — 74 MB");

        let two = super::model_row_sources(vec![
            installed_spec(
                crate::model_download::ModelKind::Speech,
                speech_spec(),
                1_624_555_275,
            ),
            extra_file("left-over.bin", 1_624_555_275),
        ]);
        assert_eq!(super::models_total_text(&two), "2 models — 3.0 GB");
    }

    /// tick の作り直しは**モーダルと通知に触らない**（触ると確認モーダルが 100ms で閉じ、
    /// 削除が完走できず、失敗の理由も読めない）。操作の直後だけ畳む。
    #[test]
    fn only_an_operation_resets_the_modal_and_the_notice() {
        let after = super::ModelsRefresh::AfterOperation(Some("boom"));
        assert!(after.resets_modal());
        assert_eq!(after.notice(), super::NoticeUpdate::Set(Some("boom")));
        let cleared = super::ModelsRefresh::AfterOperation(None);
        assert!(cleared.resets_modal());
        assert_eq!(
            cleared.notice(),
            super::NoticeUpdate::Set(None),
            "an operation clears the notice"
        );

        let poll = super::ModelsRefresh::Poll;
        assert!(!poll.resets_modal(), "the tick must not close the modal");
        assert_eq!(
            poll.notice(),
            super::NoticeUpdate::Keep,
            "the tick must not touch the notice"
        );
        // 取得の完了で走査し直す経路も tick 由来なので、直前の失敗の理由を消さない。
        let rescan = super::ModelsRefresh::Rescan;
        assert!(!rescan.resets_modal());
        assert_eq!(rescan.notice(), super::NoticeUpdate::Keep);
    }

    /// 行の反映は**変わった行だけ**（全差し替えするとホバー・押下中の状態が飛んでクリックを
    /// 取りこぼす）。全差し替えにするのは行数が変わるときだけ。
    #[test]
    fn rows_to_update_replaces_all_only_when_the_count_changes() {
        let row = |name: &str| super::ModelRow {
            name: name.into(),
            ..super::ModelRow::default()
        };

        // 行数が変わる（素材を作り直した）。
        assert_eq!(
            super::rows_to_update(&[], &[row("a")]),
            super::RowUpdate::ReplaceAll
        );
        assert_eq!(
            super::rows_to_update(&[row("a"), row("b")], &[row("a")]),
            super::RowUpdate::ReplaceAll
        );
        // 同じ行数なら変わった添字だけ（tick はこちらを通る）。
        assert_eq!(
            super::rows_to_update(&[row("a"), row("b")], &[row("a"), row("c")]),
            super::RowUpdate::Changed(vec![1])
        );
        // 何も変わらなければ触らない。
        assert_eq!(
            super::rows_to_update(&[row("a"), row("b")], &[row("a"), row("b")]),
            super::RowUpdate::Changed(Vec::new())
        );
    }

    /// 反映そのものも見る（判断どおりにモデルが収束すること）。
    #[test]
    fn apply_model_rows_converges_to_the_new_rows() {
        use slint::Model as _;
        let model: std::rc::Rc<slint::VecModel<super::ModelRow>> =
            std::rc::Rc::new(slint::VecModel::default());
        let row = |name: &str| super::ModelRow {
            name: name.into(),
            ..super::ModelRow::default()
        };

        super::apply_model_rows(&model, vec![row("a"), row("b")]);
        assert_eq!(model.row_count(), 2);
        super::apply_model_rows(&model, vec![row("a"), row("c")]);
        assert_eq!(model.row_data(1).expect("the row exists").name, "c");
        super::apply_model_rows(&model, vec![row("a")]);
        assert_eq!(model.row_count(), 1);
    }

    /// 走査し直す契機は「**記録が取得済みになった ID の集合が変わったとき**」。
    /// 「記録は取得済みなのに実体が無い」を条件にすると、解消しない不一致で走査が止まらない。
    #[test]
    fn downloaded_ids_track_the_recorded_completions() {
        let downloader = crate::model_download::ModelDownloader::new();
        let sources = super::model_row_sources(vec![extra_file("left-over.bin", 10)]);
        assert!(
            super::downloaded_ids(&sources, &downloader).is_empty(),
            "nothing is recorded as downloaded yet"
        );

        // 取得中はまだ数えない（完了で初めて走査し直す）。
        downloader.set_status_for_test(
            speech_spec(),
            crate::model_download::DownloadStatus::Downloading {
                received: 1,
                total: 2,
            },
        );
        assert!(super::downloaded_ids(&sources, &downloader).is_empty());

        downloader.set_status_for_test(
            speech_spec(),
            crate::model_download::DownloadStatus::Downloaded,
        );
        assert_eq!(
            super::downloaded_ids(&sources, &downloader),
            vec![speech_spec().id]
        );
        // カタログ外のファイルは取得の記録を持たないので数に入らない。
        assert!(
            !super::downloaded_ids(&sources, &downloader).contains(&"left-over.bin"),
            "an unknown file has no download record"
        );
    }

    /// `config.toml` がその種別のモデルパスを上書きしている間は、カタログの選択が使われないので
    /// 「使う」も「取得する」も出さない（数 GB 落としても使われない）。
    #[test]
    fn an_overridden_kind_offers_neither_use_nor_download() {
        let downloader = crate::model_download::ModelDownloader::new();
        let mut overridden = context(&downloader, false, false);
        overridden.speech_overridden = true;
        let mut busy_overridden = context(&downloader, true, false);
        busy_overridden.speech_overridden = true;
        let source = super::ModelRowSource::Catalog {
            kind: crate::model_download::ModelKind::Speech,
            spec: speech_spec(),
            installed: None,
        };

        let facts = super::row_facts(&source, &overridden);
        assert_eq!(facts.usage, super::RowUsage::Overridden);
        assert!(!super::can_use_row(&source, &facts));
        assert!(!super::can_download_row(
            ModelStatus::NotDownloaded,
            &source,
            &facts
        ));

        // 上書き中の種別では、ジョブはカタログのファイルを開かないので「使用中」にしない
        // （そうしないと、確実に使われていない数 GB を掃除できなくなる）。
        assert!(
            !super::row_facts(&source, &busy_overridden).busy,
            "an overridden kind does not read the catalog file"
        );
        // 上書きされていない種別は今までどおり選べる・落とせる。
        let summary = super::ModelRowSource::Catalog {
            kind: crate::model_download::ModelKind::Summary,
            spec: summary_spec(),
            installed: None,
        };
        let summary_facts = super::row_facts(&summary, &overridden);
        assert_eq!(summary_facts.usage, super::RowUsage::Selected);
        assert!(super::can_download_row(
            ModelStatus::NotDownloaded,
            &summary,
            &summary_facts
        ));
    }

    /// 「取得する」を出すのは、ディスクに実体が無いカタログの行だけ。
    #[test]
    fn can_download_row_only_offers_catalog_rows_without_a_file() {
        let facts = super::RowFacts {
            usage: super::RowUsage::Idle,
            busy: false,
        };
        let source = super::ModelRowSource::Catalog {
            kind: crate::model_download::ModelKind::Speech,
            spec: speech_spec(),
            installed: None,
        };
        assert!(super::can_download_row(
            ModelStatus::NotDownloaded,
            &source,
            &facts
        ));
        assert!(super::can_download_row(
            ModelStatus::Failed,
            &source,
            &facts
        ));
        assert!(!super::can_download_row(
            ModelStatus::Installed,
            &source,
            &facts
        ));
        assert!(!super::can_download_row(
            ModelStatus::Downloading,
            &source,
            &facts
        ));
        // カタログ外・見出しは取得できない（URL が無い）。
        let extra = super::ModelRowSource::Extra(extra_file("left-over.bin", 10));
        assert!(!super::can_download_row(
            ModelStatus::NotDownloaded,
            &extra,
            &facts
        ));
        // 上書きがこの行のファイルを指しているなら、落とすことが動かす唯一の手段なので出す。
        let in_config = super::RowFacts {
            usage: super::RowUsage::InConfig,
            busy: false,
        };
        assert!(super::can_download_row(
            ModelStatus::NotDownloaded,
            &source,
            &in_config
        ));
    }

    /// 見出しの文言（全種別）。種別を足したら網羅 match が更新を強制する。
    #[test]
    fn kind_heading_covers_all_kinds() {
        assert_eq!(
            super::kind_heading(crate::model_download::ModelKind::Speech),
            "Transcription — Whisper"
        );
        assert_eq!(
            super::kind_heading(crate::model_download::ModelKind::Summary),
            "Meeting minutes — LLM"
        );
    }

    /// 要約 LLM の状態行は取得状況を示す（どのモデルかは ComboBox が示す）。取得の契機が設定で
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
        assert_eq!(
            summary_model_status_line(&idle, &downloader).text,
            "Not downloaded — downloads when minutes are generated (4.4 GB)"
        );
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
        assert_eq!(
            summary_model_status_line(&overridden, &downloader).text,
            "Using the model file set in config.toml"
        );
    }

    /// ComboBox の選択肢は「名前 — サイズ — 説明」で、カタログの順・件数どおりに並ぶ。
    /// 要約 LLM の説明行はこの文字列を Slint 側で引くので、目安が入っていることもここで固定する。
    #[test]
    fn model_choices_follow_the_catalog_order() {
        use slint::Model;

        let choices = model_choices(crate::summary_model::CATALOG);
        assert_eq!(choices.row_count(), crate::summary_model::CATALOG.len());
        assert_eq!(
            choices
                .row_data(0)
                .expect("the catalog has at least one entry"),
            "Qwen2.5 3B Instruct — 2.0 GB — 25 s and 3.7 GB of memory for a 4-min meeting, but can invent details"
        );
        assert_eq!(
            choices.row_data(1).expect("the catalog has a second entry"),
            "Qwen2.5 7B Instruct — 4.4 GB — 54 s and 8.2 GB of memory for a 4-min meeting, more faithful"
        );
        // whisper でも同じ形（サイズは MB 表記になる）。
        let whisper = model_choices(crate::whisper_model::CATALOG);
        assert_eq!(
            whisper.row_data(0).expect("the catalog has a first entry"),
            "Tiny — 74 MB — fastest, lowest accuracy"
        );
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
