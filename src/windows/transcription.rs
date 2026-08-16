//! 文字起こし設定ウィンドウの配線（#141）。
//!
//! **1 つの機能を端から端まで持つ**: 自動実行のスイッチ・認識言語と、その機能が使うモデルの
//! 一覧（選択・取得・削除）。使うモデルを選ぶ場所は一覧の `Use` だけで、設定画面には選択 UI を
//! 置かない（同じ設定を 2 つの UI が持っていたのを解消するのがこの分割の目的）。
//!
//! 一覧そのものの合成は `super::models`。ここが持つのはこのウィンドウ固有の設定と、設定画面の
//! 「扉」に出す文言。

use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle as _;

use crate::config::Config;
use crate::windows::models::{
    self, ModelLists, ModelsRefresh, RefreshLists, RowAction, set_if_changed,
};
use crate::{
    AppWindow, StatusTone, TranscriptionWindow, config, model_download, summarize, transcribe,
    whisper_model,
};

/// ウィンドウを作り、設定とコールバックを配線する。
///
/// `refresh` は「両方の一覧を作り直して、生きているウィンドウへ反映する」処理（`main` が持つ）。
/// **操作したウィンドウと巻き込まれたウィンドウで理由を変える**ため、呼び出し側へ委ねてある。
pub(crate) fn build(
    config: &Rc<RefCell<Config>>,
    lists: &ModelLists,
    downloader: &model_download::ModelDownloader,
    transcriber: &transcribe::TranscribeWorker,
    summarizer: &summarize::SummarizeWorker,
    refresh: RefreshLists,
) -> TranscriptionWindow {
    let window = TranscriptionWindow::new().expect("creating the transcription window succeeds");
    window.set_models(lists.transcription.rows.clone().into());
    // 行が 1 つも無いときの縮退表示（カタログの行は必ず並ぶので実際には出ない。走査の失敗は
    // 通知で伝える）。
    window.set_empty_text(models::MODELS_EMPTY_TEXT.into());
    window.set_languages(
        Rc::new(slint::VecModel::<slint::SharedString>::from(
            config::TRANSCRIBE_LANGUAGES
                .iter()
                .map(|(_, display)| slint::SharedString::from(*display))
                .collect::<Vec<_>>(),
        ))
        .into(),
    );
    apply_settings(&window, &config.borrow());

    // 自動文字起こしの ON/OFF。保存に失敗したら表示を旧値へ戻す（`docs/rules/slint.md`）。
    {
        let weak = window.as_weak();
        let config = Rc::clone(config);
        let refresh = Rc::clone(&refresh);
        window.on_toggle_auto_transcribe(move |enabled| {
            let mut candidate = config.borrow().clone();
            candidate.auto_transcribe = enabled;
            let saved = candidate.save().is_ok();
            if saved {
                *config.borrow_mut() = candidate;
            } else {
                eprintln!("Keeping the previous transcription setting because saving failed");
            }
            if let Some(window) = weak.upgrade() {
                // 保存できなければ旧値へ戻す。できたときも、状態の文言を組み直すために通す。
                apply_settings(&window, &config.borrow());
            }
            // 一覧のバッジ（`In use` / `Selected`）が機能の ON/OFF で変わるので作り直す。
            refresh(ModelsRefresh::AfterOperation(None));
        });
    }

    // 認識言語。カタログ内の添字で受け、コードへ変換して永続化する。
    {
        let weak = window.as_weak();
        let config = Rc::clone(config);
        window.on_change_language(move |index| {
            let mut candidate = config.borrow().clone();
            // Select は Rust が渡したカタログの範囲しか返さないが、防御的に既定（先頭）へ丸める。
            let (code, _) = config::TRANSCRIBE_LANGUAGES
                .get(usize::try_from(index).unwrap_or(0))
                .unwrap_or(&config::TRANSCRIBE_LANGUAGES[0]);
            candidate.transcribe_language = (*code).to_owned();
            if candidate.save().is_ok() {
                *config.borrow_mut() = candidate;
            } else {
                eprintln!("Keeping the previous transcription language because saving failed");
            }
            if let Some(window) = weak.upgrade() {
                apply_settings(&window, &config.borrow());
            }
        });
    }

    wire_list(
        &window,
        config,
        lists,
        downloader,
        transcriber,
        summarizer,
        refresh,
    );
    window
}

/// 一覧の 3 操作を配線する。**素材を引くのは自分のウィンドウのハンドルだけ**
/// （`lists.transcription`）。
fn wire_list(
    window: &TranscriptionWindow,
    config: &Rc<RefCell<Config>>,
    lists: &ModelLists,
    downloader: &model_download::ModelDownloader,
    transcriber: &transcribe::TranscribeWorker,
    summarizer: &summarize::SummarizeWorker,
    refresh: RefreshLists,
) {
    {
        let handles = lists.transcription.clone();
        let config = Rc::clone(config);
        let downloader = downloader.clone();
        let refresh = Rc::clone(&refresh);
        window.on_use_model(move |index| {
            if let RowAction::Done(notice) =
                models::use_model_at(&handles, index, &config, &downloader)
            {
                refresh(ModelsRefresh::AfterOperation(notice));
            }
        });
    }
    {
        let handles = lists.transcription.clone();
        let downloader = downloader.clone();
        let refresh = Rc::clone(&refresh);
        window.on_download_model(move |index| {
            if let RowAction::Done(notice) = models::download_model_at(&handles, index, &downloader)
            {
                refresh(ModelsRefresh::AfterOperation(notice));
            }
        });
    }
    {
        let lists = lists.clone();
        let config = Rc::clone(config);
        let downloader = downloader.clone();
        let transcriber = transcriber.clone();
        let summarizer = summarizer.clone();
        window.on_delete_model(move |index| {
            if let RowAction::Done(notice) = models::delete_model_at(
                &lists,
                &lists.transcription,
                index,
                &config,
                &downloader,
                &transcriber,
                &summarizer,
            ) {
                refresh(ModelsRefresh::AfterOperation(notice));
            }
        });
    }
}

/// 設定の値と、見出しの下の 1 行を表示へ入れる（**初期化と更新が同じ経路を通る**）。
pub(crate) fn apply_settings(window: &TranscriptionWindow, config: &Config) {
    window.set_auto_transcribe(config.auto_transcribe);
    window
        .set_language_index(config::transcribe_language_index(&config.transcribe_language) as i32);
    window.set_feature_note(feature_note(config).into());
}

/// 見出しの下の 1 行。**その機能がいま何をするか**を書く（ON/OFF で言うことが変わる）。
fn feature_note(config: &Config) -> &'static str {
    if config.auto_transcribe {
        "Turns a finished recording into text on this Mac. Nothing is uploaded."
    } else {
        "Off — recordings stay as audio only."
    }
}

/// 設定画面の「扉」に出す文言を入れる。
///
/// 扉が要るのは**中を開かずに現状が分かること**なので、3 つに分ける: いま ON か（`state`）、
/// どう構成されているか（`summary`）、いま何が起きているか（`status`）。
pub(crate) fn apply_door(
    ui: &AppWindow,
    config: &Config,
    downloader: &model_download::ModelDownloader,
) {
    ui.set_auto_transcribe(config.auto_transcribe);
    ui.set_transcription_state(if config.auto_transcribe { "On" } else { "Off" }.into());
    set_if_changed(
        || ui.get_transcription_summary(),
        |text| ui.set_transcription_summary(text),
        door_summary(config, downloader),
    );
    let line = crate::whisper_model_status_line(config, downloader);
    set_if_changed(
        || ui.get_transcription_status(),
        |text| ui.set_transcription_status(text),
        door_status(config, &line),
    );
    // OFF のときは取得状況ではなく「何が起きないか」を出しているので、色も事実を述べる側へ。
    ui.set_transcription_tone(if config.auto_transcribe {
        line.tone
    } else {
        StatusTone::Neutral
    });
}

/// 扉の構成行（`Japanese · Medium (1.5 GB) · 2 of 6 models downloaded`）。
fn door_summary(config: &Config, downloader: &model_download::ModelDownloader) -> String {
    let language = config::TRANSCRIBE_LANGUAGES
        .get(config::transcribe_language_index(
            &config.transcribe_language,
        ))
        .map_or("English", |(_, display)| *display);
    let spec = whisper_model::spec_or_default(&config.whisper_model);
    format!(
        "{language} · {} ({}) · {}",
        spec.display_name,
        model_download::format_size(spec.size_bytes),
        models::downloaded_count_text(whisper_model::CATALOG, downloader)
    )
}

/// 扉の状態行。OFF のときは取得状況ではなく**何が起きないか**を出す（動いていないのに
/// `Ready` と書くと、動いていると読める）。
fn door_status(config: &Config, line: &crate::ModelStatusLine) -> String {
    if !config.auto_transcribe {
        return "Recordings stay as audio until you ask".to_owned();
    }
    line.text.clone()
}
