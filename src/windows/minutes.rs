//! 議事録設定ウィンドウの配線（#141）。骨格は `super::transcription` と同じ（説明はあちら）。
//!
//! 違うのは Behaviour の中身と、一覧に出す種別。それと**文字起こしへの従属の表し方**——
//! 自動文字起こしが OFF でもトグルは無効にせず、注意書きで伝える。手で頼む要約は自動文字起こしと
//! 独立に動くので、無効化は実装より強い制約になる。

use std::cell::RefCell;
use std::rc::Rc;

use slint::ComponentHandle as _;

use crate::config::Config;
use crate::windows::models::{
    self, ModelLists, ModelsRefresh, RefreshLists, RowAction, set_if_changed,
};
use crate::{
    AppWindow, MinutesWindow, StatusTone, model_download, summarize, summary_model, transcribe,
};

pub(crate) fn build(
    config: &Rc<RefCell<Config>>,
    lists: &ModelLists,
    downloader: &model_download::ModelDownloader,
    transcriber: &transcribe::TranscribeWorker,
    summarizer: &summarize::SummarizeWorker,
    refresh: RefreshLists,
) -> MinutesWindow {
    let window = MinutesWindow::new().expect("creating the meeting notes window succeeds");
    window.set_models(lists.minutes.rows.clone().into());
    window.set_empty_text(models::MODELS_EMPTY_TEXT.into());
    apply_settings(&window, &config.borrow());

    {
        let weak = window.as_weak();
        let config = Rc::clone(config);
        let refresh = Rc::clone(&refresh);
        window.on_toggle_auto_summarize(move |enabled| {
            let mut candidate = config.borrow().clone();
            candidate.auto_summarize = enabled;
            let saved = candidate.save().is_ok();
            if saved {
                *config.borrow_mut() = candidate;
            } else {
                eprintln!("Keeping the previous meeting notes setting because saving failed");
            }
            if let Some(window) = weak.upgrade() {
                apply_settings(&window, &config.borrow());
            }
            refresh(ModelsRefresh::AfterOperation(None));
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

fn wire_list(
    window: &MinutesWindow,
    config: &Rc<RefCell<Config>>,
    lists: &ModelLists,
    downloader: &model_download::ModelDownloader,
    transcriber: &transcribe::TranscribeWorker,
    summarizer: &summarize::SummarizeWorker,
    refresh: RefreshLists,
) {
    {
        let handles = lists.minutes.clone();
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
        let handles = lists.minutes.clone();
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
                &lists.minutes,
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

pub(crate) fn apply_settings(window: &MinutesWindow, config: &Config) {
    window.set_auto_summarize(config.auto_summarize);
    window.set_feature_note(feature_note(config).into());
    window.set_depends_note(depends_note(config).into());
}

fn feature_note(config: &Config) -> &'static str {
    if config.auto_summarize {
        "Reads a transcript and writes decisions and action items. Runs on this Mac."
    } else {
        "Off — transcripts stay as text only."
    }
}

/// 文字起こしへの従属の注意（**無効化の代わり**）。空なら出さない。
///
/// 出すのは「自動要約は ON なのに、その入力を作る自動文字起こしが OFF」のときだけ。両方 OFF なら
/// 待っているものが無いので、言うことがない。
fn depends_note(config: &Config) -> &'static str {
    if config.auto_summarize && !config.auto_transcribe {
        "Automatic transcription is off, so no transcript is produced on its own and nothing will \
         run automatically. Notes you ask for by hand still work — shoki transcribes that \
         recording first."
    } else {
        ""
    }
}

pub(crate) fn apply_door(
    ui: &AppWindow,
    config: &Config,
    downloader: &model_download::ModelDownloader,
) {
    ui.set_auto_summarize(config.auto_summarize);
    ui.set_minutes_state(if config.auto_summarize { "On" } else { "Off" }.into());
    set_if_changed(
        || ui.get_minutes_summary(),
        |text| ui.set_minutes_summary(text),
        door_summary(config, downloader),
    );
    let line = crate::summary_model_status_line(config, downloader);
    set_if_changed(
        || ui.get_minutes_status(),
        |text| ui.set_minutes_status(text),
        door_status(config, &line),
    );
    ui.set_minutes_tone(door_tone(config, &line));
}

fn door_summary(config: &Config, downloader: &model_download::ModelDownloader) -> String {
    let spec = summary_model::spec_or_default(&config.summary_model);
    format!(
        "{} ({}) · {}",
        spec.display_name,
        model_download::format_size(spec.size_bytes),
        models::downloaded_count_text(summary_model::CATALOG, downloader)
    )
}

/// 扉の状態行。**待っている理由を先に言う**——要約は ON なのに文字起こしが OFF なら、
/// 取得状況より先にそこを伝えないと「準備できている」と読める。
fn door_status(config: &Config, line: &crate::ModelStatusLine) -> String {
    if !config.auto_summarize {
        return "Transcripts stay as text until you ask".to_owned();
    }
    if !config.auto_transcribe {
        return "Waits for a transcript — automatic transcription is off".to_owned();
    }
    line.text.clone()
}

fn door_tone(config: &Config, line: &crate::ModelStatusLine) -> StatusTone {
    if !config.auto_summarize {
        return StatusTone::Neutral;
    }
    if !config.auto_transcribe {
        return StatusTone::Caution;
    }
    line.tone
}
