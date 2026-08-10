//! モデル一覧の合成・可否判定・走査・通知・選択（#140 で `main.rs` から切り出した）。
//!
//! 一覧は**複数のウィンドウへ載せる**（`#141` で文字起こし用と議事録用に分ける）。描画は
//! `ui/model-list.slint` の `ModelList` が持ち、そこへ詰める素材をここが作る。
//!
//! この境目の要点は 2 つ:
//!
//! - **ディスクの走査（`model_download::installed_models()`）を呼ぶのはここの
//!   `refresh_models_window` だけ**（`docs/rules/performance.md`。100ms tick に走査を載せない）。
//! - **操作の可否は Rust の純粋関数が決める**（`can_use_row` / `can_download_row` /
//!   `can_delete_row`）。Slint 側で `ModelStatus` から導出させない（`docs/rules/slint.md`）——
//!   状態だけでは決まらず、ジョブ・`config.toml` の上書き・カタログの有無が絡むため。

use std::cell::RefCell;
use std::rc::Rc;

use crate::config::Config;
use crate::{
    ModelRow, ModelStatus, ModelsWindow, download_percent, model_download,
    model_downloads_on_select, summarize, summary_model, transcribe, whisper_model,
};

/// 一覧を作り直す理由。**tick の作り直しがモーダルと通知を消さないよう**、意図を型で渡す
/// （`docs/rules/slint.md` の「『失敗したら表示を更新しない』は、ポーリング tick の上書きまで
/// 考える」。#117 は tick で作り直さない前提だったので同じ経路で済んでいた）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelsRefresh {
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
pub(crate) enum NoticeUpdate {
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
pub(crate) fn refresh_models_window(
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
pub(crate) fn refresh_cause(
    cause: ModelsRefresh,
    scan_notice: Option<&'static str>,
) -> ModelsRefresh {
    match (cause, scan_notice) {
        (_, Some(scan)) => ModelsRefresh::AfterOperation(Some(scan)),
        (cause, None) => cause,
    }
}

/// 走査の結果を素材へ入れ、**一緒に更新すべきもの**（tick のラッチと上書き先の解決）も同時に
/// 書く。3 つを別々に更新すると、片方だけ古い状態が生まれる（ラッチが古いと毎 tick 走査し直し、
/// 上書き先が古いと守るべき行を守らない）。
pub(crate) fn reseed_model_sources(
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
pub(crate) fn refresh_model_rows(
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
    let rows = model_rows(&sources, &context, handles.kinds);

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
pub(crate) enum RowUpdate {
    /// モデルごと差し替える（行数が変わった＝素材を作り直した）。
    ReplaceAll,
    /// この添字の行だけ `set_row_data` する（空なら何もしない）。
    Changed(Vec<usize>),
}

/// いまの行と組み直した行を比べて、反映の仕方を決める（純粋関数）。
pub(crate) fn rows_to_update(current: &[ModelRow], next: &[ModelRow]) -> RowUpdate {
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
pub(crate) fn apply_model_rows(model: &Rc<slint::VecModel<ModelRow>>, rows: Vec<ModelRow>) {
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
pub(crate) struct OverrideFiles {
    pub(crate) speech: Option<String>,
    pub(crate) summary: Option<String>,
}

/// 一覧に出す種別。**登録簿と対で読む**——`model_download::REGISTERED_CATALOGS` に種別を
/// 足したらここにも足す（足さないとその種別が一覧から静かに消える）。網羅 match で強制できない
/// のは、これが「全部入り」を表す定数だから。
pub(crate) const ALL_MODEL_KINDS: &[model_download::ModelKind] = &[
    model_download::ModelKind::Speech,
    model_download::ModelKind::Summary,
];

#[derive(Clone)]
pub(crate) struct ModelListHandles {
    /// 一覧の行の素材（走査した時点のもの。tick は状態だけ組み直す）。
    pub(crate) sources: Rc<RefCell<Vec<ModelRowSource>>>,
    /// 上書き先のファイル名（走査と同じタイミングで解決する）。
    pub(crate) override_files: Rc<RefCell<OverrideFiles>>,
    /// この一覧が出す種別。**カタログ外のファイルはこれに関わらず末尾に出る**
    /// （`ModelRowSource::belongs_to`）。#141 でウィンドウを分けたとき、ここだけが変わる。
    pub(crate) kinds: &'static [model_download::ModelKind],
    /// UI が参照し続けるモデル（差し替えずに行単位で更新する）。
    pub(crate) rows: Rc<slint::VecModel<ModelRow>>,
    /// 直前に走査したときに「取得済みとして記録されていた」ID（tick が走査し直す契機の判定。
    /// `downloaded_ids`）。
    pub(crate) downloaded_seen: Rc<RefCell<Vec<&'static str>>>,
}

/// 一覧の 1 行の素材。**カタログ全件**（未取得を含む）と、`models/` にあるカタログ外のファイルを
/// 種別ごとに並べたもの（#138。#117 の「ディスクにあるものだけ」から広げた）。
///
/// 行の並びが UI のインデックスと 1 対 1 なので、ここで作った順序を UI へ渡すまで変えない
/// （並べ替えると**別のモデルを消す**）。
#[derive(Debug, Clone)]
pub(crate) enum ModelRowSource {
    /// 種別の見出し（ボタンを持たない行）。**どの種別の区切りかを持つ**——種別で絞ったとき、
    /// 見出しだけが取り残されないようにするため。`None` はカタログ外のファイルの見出しで、
    /// これは種別に属さないので絞り込みでも常に残る。
    Heading {
        kind: Option<model_download::ModelKind>,
        title: &'static str,
    },
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
    /// 一覧に出す種別に属するか。**カタログ外のファイル（とその見出し）は種別を持たないので
    /// 常に出す**——`models/` に置き去りのファイルは、どの種別の一覧から見ても掃除できる必要が
    /// ある（種別ごとに分けた一覧のどれにも出ないと、消す手段が消える）。
    ///
    /// 素材の並びは `model_row_sources` が作った順のままにする（`REGISTERED_CATALOGS` を
    /// 呼び出し側で並べ直さない、という `src/model_download.rs` の約束）。絞るだけなので、
    /// カタログ外の行は末尾に残る。
    pub(crate) fn belongs_to(&self, kinds: &[model_download::ModelKind]) -> bool {
        match self {
            Self::Heading { kind: None, .. } | Self::Extra(_) => true,
            Self::Heading {
                kind: Some(kind), ..
            }
            | Self::Catalog { kind, .. } => kinds.contains(kind),
        }
    }

    /// ディスクに在るならその実体（削除の対象。見出しと未取得の行は `None`）。
    pub(crate) fn installed(&self) -> Option<&model_download::InstalledModel> {
        match self {
            Self::Heading { .. } => None,
            Self::Catalog { installed, .. } => installed.as_ref(),
            Self::Extra(installed) => Some(installed),
        }
    }
}

/// 種別の見出しの文言（**網羅 match**。種別を足したら見出しを書くまでコンパイルが通らない）。
pub(crate) fn kind_heading(kind: model_download::ModelKind) -> &'static str {
    match kind {
        model_download::ModelKind::Speech => "Transcription — Whisper",
        model_download::ModelKind::Summary => "Meeting notes — LLM",
    }
}

/// 行の素材を組む。**カタログの登録簿の順**（種別ごとに見出し → カタログの並び）で、最後に
/// カタログ外のファイルを大きい順で置く。
pub(crate) fn model_row_sources(
    installed: Vec<model_download::InstalledModel>,
) -> Vec<ModelRowSource> {
    let mut sources: Vec<ModelRowSource> = Vec::new();
    for (kind, catalog, _) in model_download::REGISTERED_CATALOGS {
        sources.push(ModelRowSource::Heading {
            kind: Some(*kind),
            title: kind_heading(*kind),
        });
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
        sources.push(ModelRowSource::Heading {
            kind: None,
            title: EXTRA_FILES_HEADING,
        });
        sources.extend(extras.map(ModelRowSource::Extra));
    }
    sources
}

/// カタログ外のファイルの見出し。
pub(crate) const EXTRA_FILES_HEADING: &str = "Other files in the models folder";

/// 行の状態を決めるための周辺状況（ワーカーと設定への照会を 1 回ずつに畳んでから渡す）。
pub(crate) struct ModelsContext<'a> {
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

pub(crate) fn models_context<'a>(
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
pub(crate) fn kind_is_busy(context: &ModelsContext, kind: model_download::ModelKind) -> bool {
    match kind {
        model_download::ModelKind::Speech => context.speech_busy,
        model_download::ModelKind::Summary => context.summary_busy,
    }
}

/// 行の使用状況（表示と可否の分岐に使う。取得の状態＝`ModelStatus` とは別の軸）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowUsage {
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
pub(crate) fn kind_overridden(context: &ModelsContext, kind: model_download::ModelKind) -> bool {
    match kind {
        model_download::ModelKind::Speech => context.speech_overridden,
        model_download::ModelKind::Summary => context.summary_overridden,
    }
}

/// 行ごとに求める事実（純粋関数。ワーカー・設定への照会は `ModelsContext` に畳んである）。
pub(crate) struct RowFacts {
    pub(crate) usage: RowUsage,
    /// その行のファイルを読むジョブが在るか（削除させない条件）。
    pub(crate) busy: bool,
}

pub(crate) fn row_facts(source: &ModelRowSource, context: &ModelsContext) -> RowFacts {
    let filename = match source {
        // 見出しはファイルを持たない（`""` をパスに合成しないよう、ここで打ち切る）。
        ModelRowSource::Heading { .. } => {
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
pub(crate) fn model_status(
    has_file: bool,
    recorded: Option<&model_download::DownloadStatus>,
) -> ModelStatus {
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
pub(crate) fn model_status_part(status: ModelStatus, percent: u64, reason: Option<&str>) -> String {
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
pub(crate) fn model_usage_part(usage: RowUsage) -> Option<&'static str> {
    match usage {
        RowUsage::Idle => None,
        RowUsage::Selected => Some("selected in Settings"),
        RowUsage::InConfig => Some("set in config.toml"),
        RowUsage::Overridden => Some("not used because config.toml sets the model file"),
        RowUsage::Unknown => Some("not in the model catalog"),
    }
}

/// 削除できない理由の文言（ボタンが淡色になるだけでは理由が分からないので文字で出す）。
pub(crate) const MODEL_BUSY_PART: &str = "cannot be deleted while it is in use";

/// 行の状態テキスト。**3 つの表を `—` でつなぐ**（取得の状態・使用状況・使用中）。
/// 組み合わせを 1 つの表にすると状態 × 状況で行数が掛け算になるため、軸ごとに分ける。
pub(crate) fn model_row_status_text(
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
pub(crate) fn can_use_row(source: &ModelRowSource, facts: &RowFacts) -> bool {
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
pub(crate) fn can_download_row(
    status: ModelStatus,
    source: &ModelRowSource,
    facts: &RowFacts,
) -> bool {
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
pub(crate) fn can_delete_row(source: &ModelRowSource, facts: &RowFacts) -> bool {
    source.installed().is_some() && !facts.busy
}

/// 確認モーダルの説明テキスト。解放される容量と、**ゴミ箱へは入らない**こと、そして
/// 再取得できるかを出す（4.4GB の再取得は分オーダーかかるので、押す前に分かるようにする）。
pub(crate) fn model_delete_detail(usage: RowUsage, size_bytes: u64) -> String {
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
pub(crate) const MODELS_EMPTY_TEXT: &str = "No models available";

/// 走査そのものに失敗したときの通知（カタログの行は必ず並ぶので、空表示では気づけない。
/// 行のサイズ・状態がディスクの実体を反映しないことも伝える）。
pub(crate) const MODELS_UNREADABLE_NOTICE: &str =
    "Could not list the models folder — sizes and states may be out of date.";

/// 押された時点で使用中だったときの通知（一覧の状態テキストと同じ事実を指すので、語を揃える）。
pub(crate) const MODEL_IN_USE_NOTICE: &str = "This model is in use right now — it was not deleted.";

/// 削除できなかった理由（`ModelsWindow::notice`）。理由まで出すのは「押しても無反応」に見せない
/// ため。使用中は UI で押させないのが基本なので、ここへ来るのは一覧が古かったときだけ。
pub(crate) const MODEL_DELETE_FAILED_NOTICE: &str =
    "Could not delete this model — see the log for details.";

/// 選択を保存できなかったときの通知（設定の永続化に失敗した場合）。
pub(crate) const MODEL_SELECT_FAILED_NOTICE: &str =
    "Could not change the model — the settings could not be saved (see the log).";

/// 削除の結果。bool 2 つで持つと `(busy, !failed)` のような**ありえない組み合わせ**を作れるので、
/// 3 値の enum にする（`docs/review-perspectives/rust-anti-patterns.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeleteOutcome {
    /// 消えた。
    Deleted,
    /// 押された時点で使用中だった（消していない）。
    InUse,
    /// 基盤側が拒否した・I/O で失敗した（理由はログ）。
    Failed,
}

/// 削除の結果から通知を決める（網羅 match なので、結果を足したら文言を書くまで通らない）。
pub(crate) fn delete_failure_notice(outcome: DeleteOutcome) -> Option<&'static str> {
    match outcome {
        DeleteOutcome::Deleted => None,
        DeleteOutcome::InUse => Some(MODEL_IN_USE_NOTICE),
        DeleteOutcome::Failed => Some(MODEL_DELETE_FAILED_NOTICE),
    }
}

/// 一覧の末尾に出す合計テキスト（**取得済みのぶんだけ**。未取得の行を足して「使っている容量」を
/// 膨らませない）。取得済みが無ければ空。
pub(crate) fn models_total_text(sources: &[ModelRowSource]) -> String {
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
pub(crate) fn model_rows(
    sources: &[ModelRowSource],
    context: &ModelsContext,
    kinds: &[model_download::ModelKind],
) -> Vec<ModelRow> {
    sources
        .iter()
        .filter(|source| source.belongs_to(kinds))
        .map(|source| match source {
            ModelRowSource::Heading { title, .. } => heading_row(title),
            _ => model_row(source, context),
        })
        .collect()
}

/// 種別の区切り行（操作を持たない）。
pub(crate) fn heading_row(title: &'static str) -> ModelRow {
    ModelRow {
        is_heading: true,
        name: title.into(),
        ..ModelRow::default()
    }
}

/// 1 行を組む（見出し以外）。表示名・説明・サイズはカタログとディスクの実体から、状態と可否は
/// `RowFacts` から決める。
pub(crate) fn model_row(source: &ModelRowSource, context: &ModelsContext) -> ModelRow {
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
        ModelRowSource::Heading { title, .. } => ((*title).to_owned(), ""),
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
pub(crate) fn downloaded_ids(
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
pub(crate) fn model_to_cancel_on_select<'a>(
    previous_id: &'a str,
    selected: &'static model_download::ModelSpec,
) -> Option<&'a str> {
    (previous_id != selected.id).then_some(previous_id)
}

/// 使うモデルを選び直して設定へ永続化する（設定画面の Select とモデル管理ウィンドウの
/// 「Use」が**同じ経路**を通る）。成功したら `true`。
///
/// 選び直しで不要になった**前のモデルの取得は打ち切る**（#124。`cancel_download`）。
///
/// 取得を始めるかは `model_downloads_on_select` が決める（種別で条件が違う）。保存に失敗したら
/// 設定は変えない。
pub(crate) fn select_model(
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
        // どの種別の話か分かるようにする（3 つの入口＝両方の Select とモデル管理ウィンドウの
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

#[cfg(test)]
mod tests {
    use crate::ModelStatus;

    /// 選び直しで打ち切るのは「別のモデルに変わったとき」だけ。同じモデルを選び直しても
    /// （モデル管理ウィンドウの「Use」は選択中の行でも押せる）自分の取得は止めない。
    #[test]
    fn model_to_cancel_on_select_skips_an_unchanged_selection() {
        let tiny = crate::whisper_model::spec_for("tiny").expect("tiny is in the catalog");
        assert_eq!(
            super::model_to_cancel_on_select("small", tiny),
            Some("small")
        );
        assert_eq!(super::model_to_cancel_on_select("tiny", tiny), None);
        // カタログ外の手編集値から選び直した場合も、その ID の取得を打ち切る対象にする
        // （走っていなければ `cancel_download` が false を返すだけ）。
        assert_eq!(
            super::model_to_cancel_on_select("no-such-model", tiny),
            Some("no-such-model")
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
                super::ModelRowSource::Heading { title, .. } => Some(*title),
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
                super::ModelRowSource::Heading {
                    kind: None,
                    title: super::EXTRA_FILES_HEADING,
                }
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
            kinds: super::ALL_MODEL_KINDS,
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
        let heading = super::ModelRowSource::Heading {
            kind: Some(crate::model_download::ModelKind::Speech),
            title: "Transcription",
        };
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
            &super::ModelRowSource::Heading {
                kind: Some(crate::model_download::ModelKind::Speech),
                title: "Transcription",
            },
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

    /// 種別で絞ると、**その種別の見出しと行だけ**が残る。カタログ外のファイル（と見出し）は
    /// 種別に属さないので常に末尾に残る——どの一覧から見ても掃除できないと、置き去りの
    /// ファイルを消す手段が無くなる。
    #[test]
    fn model_rows_can_be_narrowed_to_one_kind() {
        let downloader = crate::model_download::ModelDownloader::new();
        let sources = super::model_row_sources(vec![extra_file("left-over.bin", 20)]);
        let speech_only = super::model_rows(
            &sources,
            &context(&downloader, false, false),
            &[crate::model_download::ModelKind::Speech],
        );

        let names: Vec<&str> = speech_only.iter().map(|row| row.name.as_str()).collect();
        assert!(
            names.contains(&super::kind_heading(
                crate::model_download::ModelKind::Speech
            )),
            "the speech heading must stay: {names:?}"
        );
        assert!(
            !names.contains(&super::kind_heading(
                crate::model_download::ModelKind::Summary
            )),
            "the summary heading must go with its rows: {names:?}"
        );
        assert!(
            !names.contains(&summary_spec().display_name),
            "a summary model must not appear in a speech-only list: {names:?}"
        );
        assert!(
            names.contains(&speech_spec().display_name),
            "the speech catalog must still be listed: {names:?}"
        );

        // カタログ外は末尾に、見出しごと残る。
        assert_eq!(
            names.iter().rev().take(2).collect::<Vec<_>>(),
            vec![&"left-over.bin", &super::EXTRA_FILES_HEADING],
            "unknown files stay at the end regardless of the kind: {names:?}"
        );

        // 全種別を渡したときは絞る前と同じ（この issue の呼び出しは出力を変えない）。
        let all = super::model_rows(
            &sources,
            &context(&downloader, false, false),
            super::ALL_MODEL_KINDS,
        );
        assert_eq!(all.len(), sources.len());
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
        let rows = super::model_rows(
            &sources,
            &context(&downloader, false, false),
            super::ALL_MODEL_KINDS,
        );

        assert_eq!(rows.len(), sources.len());
        for (row, source) in rows.iter().zip(sources.iter()) {
            match source {
                super::ModelRowSource::Heading { title, .. } => {
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
            "Meeting notes — LLM"
        );
    }
}
