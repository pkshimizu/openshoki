//! モデル一覧の合成・可否判定・走査・通知・選択（#140 で `main.rs` から切り出した）。
//!
//! 一覧は**複数のウィンドウへ載せる**（#141 で文字起こし用と議事録用に分ける）。描画は
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
    ModelRow, ModelStatus, StatusTone, download_percent, model_download, model_downloads_on_select,
    summarize, summary_model, transcribe, whisper_model,
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
    /// **他方のウィンドウでの操作に巻き込まれた**。並びが変わるのでモーダルは畳むが、通知は
    /// 触らない（触っていない側に他人の失敗を出さない／出ていた通知を黙って消さない）。
    AfterOperationElsewhere,
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
    /// 確認モーダルと対象の添字を畳むか（**ユーザー操作で並びが変わる経路だけ**）。
    ///
    /// 畳まないまま並びが変わると、モーダルが指す添字が別の行を指し、**表示と違うモデルを
    /// 消す**。`Rescan` も素材を作り直すが畳まない——**モーダルが開いている間は `Rescan` を
    /// 呼ばない**のが呼び出し側の約束だから（ガードは `main` の tick。表示中のウィンドウの
    /// モーダルだけを見る）。
    fn resets_modal(self) -> bool {
        matches!(
            self,
            Self::AfterOperation(_) | Self::AfterOperationElsewhere
        )
    }

    /// 巻き込まれた側の理由。操作でなければそのまま（`Rescan` / `Poll` は元から通知に触らない）。
    pub(crate) fn elsewhere(self) -> Self {
        match self {
            Self::AfterOperation(_) => Self::AfterOperationElsewhere,
            other => other,
        }
    }

    fn notice(self) -> NoticeUpdate {
        match self {
            Self::AfterOperation(notice) => NoticeUpdate::Set(notice),
            Self::AfterOperationElsewhere | Self::Rescan | Self::Poll => NoticeUpdate::Keep,
        }
    }
}

/// **ディスクを走査して、すべての一覧の素材を作り直す**。走査する経路はここだけ
/// （`docs/rules/performance.md`。100ms tick に走査を載せない）。呼ぶのは (1) ユーザー操作の
/// 直後（開く・使う・取得・削除）と (2) tick が取得の完了を拾ったとき。
///
/// **ビューを受け取らない**のが要点。ウィンドウが閉じていても素材とラッチは揃える必要があり、
/// ビューと束ねると「片方の `Weak` が upgrade できないと走査ごと消える」経路が生まれる。
/// 表示への反映は `apply_rows` が、生きているビューにだけ行う。
///
/// 走査に失敗したら**通知**を返す（カタログの行は必ず並ぶので、空表示では気づけない。
/// このとき行のサイズと状態はディスクの実体を反映しないので、その旨も文言に含める）。
pub(crate) fn rescan_model_lists(
    lists: &ModelLists,
    downloader: &model_download::ModelDownloader,
    config: &Config,
) -> Option<&'static str> {
    // **ラッチは走査の前に読む**。走査中に完了した取得をラッチだけが拾うと、その変化は
    // 「もう処理した」ことになって次の tick でも拾われず、行が古いまま固定される。
    let latch = downloaded_ids(downloader);

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

    // **すべての一覧を同じスナップショットから作り直す**。1 つだけ作り直せる関数を公開しないのは、
    // 片方だけ古くなる経路（とくに tick が追いつけないカタログ外ファイルの削除）を塞ぐため。
    for handles in lists.all() {
        *handles.sources.borrow_mut() = model_row_sources(installed.clone(), handles.kinds);
    }
    *lists.shared.downloaded_seen.borrow_mut() = latch;
    *lists.shared.override_files.borrow_mut() = OverrideFiles {
        speech: model_download::override_filename(config.whisper_model_path.as_deref()),
        summary: model_download::override_filename(config.summary_model_path.as_deref()),
    };
    scan_notice
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

/// 行だけを組み直す（**ディスクを走査しない**。上書き先の解決も走査時に済ませてある）。
/// 開いている間の tick から呼び、取得の進捗・完了・失敗と、ジョブの開始・終了を表示へ反映する。
///
/// 変わった行だけ差し替える（`VecModel` を毎 tick 差し替えると全行の要素が再生成され、
/// ホバー・押下中の状態が飛んでクリックを取りこぼす。既存の一覧 tick と同じ流儀）。
pub(crate) fn apply_rows(
    view: &dyn ModelListView,
    handles: &ModelListHandles,
    workers: &ModelWorkers,
    config: &Config,
    cause: ModelsRefresh,
) {
    let sources = handles.sources.borrow();
    // 共有状態は**値で受ける**（`Ref` を持ち回すと、この先で作り直しを呼んだときに
    // `BorrowMutError` で常駐プロセスごと落ちる。`ModelScanShared` の doc）。
    let override_files = workers.lists.shared.override_files.borrow().clone();
    let context = models_context(
        &workers.transcriber,
        &workers.summarizer,
        &workers.downloader,
        config,
        &override_files,
    );
    let rows = model_rows(&sources, &context);

    if cause.resets_modal() {
        // 並びが変わるので、モーダルが古い行を指したままにしない。
        view.set_show_delete_confirm(false);
        view.set_delete_index(0);
    }
    if let NoticeUpdate::Set(notice) = cause.notice() {
        let notice = notice.unwrap_or_default();
        if view.notice() != notice {
            view.set_notice(notice.into());
        }
    }
    let total = models_total_text(&sources);
    if view.total_text() != total.as_str() {
        view.set_total_text(total.into());
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

/// `config.toml` のモデルパス上書きが `models/` 直下を指すときのファイル名（種別ごと）。
///
/// 上書きは config の手編集でしか変わらないので、**走査と同じタイミングで 1 回だけ解決**して持つ
/// （行ごと・tick ごとに `canonicalize` を叩かないため。`model_download::override_filename`）。
#[derive(Debug, Clone, Default)]
pub(crate) struct OverrideFiles {
    speech: Option<String>,
    summary: Option<String>,
}

/// 1 つの一覧（＝1 つの機能ウィンドウ）の素材と UI のモデル。素材と行は同じ順序で 1 対 1 なので、
/// 必ず組で持つ（別々に持つと**別のモデルを操作する**事故になる。UI が渡す添字はこの前提で
/// 素材を引く）。
#[derive(Clone)]
pub(crate) struct ModelListHandles {
    /// この一覧が出す種別。素材を作る `model_row_sources` がこれで絞る（**カタログ外の
    /// ファイルは種別に関わらず末尾に出る**——どちらの一覧からも掃除できる必要があるため）。
    pub(crate) kinds: &'static [model_download::ModelKind],
    /// 一覧の行の素材（走査した時点のもの。tick は状態だけ組み直す）。
    pub(crate) sources: Rc<RefCell<Vec<ModelRowSource>>>,
    /// UI が参照し続けるモデル（差し替えずに行単位で更新する）。
    pub(crate) rows: Rc<slint::VecModel<ModelRow>>,
}

impl ModelListHandles {
    fn new(kinds: &'static [model_download::ModelKind]) -> Self {
        Self {
            kinds,
            sources: Rc::new(RefCell::new(Vec::new())),
            rows: Rc::new(slint::VecModel::default()),
        }
    }
}

/// 2 つの一覧が**共有する**、走査に由来する状態。
///
/// **読み出しは値で返す**（`Ref` を持ち回さない）。作り直しの経路はここを `borrow_mut` するので、
/// 判定のために借りたまま呼ぶと `BorrowMutError` で**常駐プロセスごと落ちる**
/// （`docs/rules/coding-conventions.md`）。
struct ModelScanShared {
    /// 上書き先のファイル名（走査と同じタイミングで解決する）。
    override_files: RefCell<OverrideFiles>,
    /// 直前に走査したときに「取得済みとして記録されていた」ID（tick が走査し直す契機の判定）。
    /// **全種別で 1 つ**——ウィンドウごとに持つと、先に評価された側がラッチを消費して、
    /// もう片方が永久に古いままになる。
    downloaded_seen: RefCell<Vec<&'static str>>,
}

/// 走査を共有する一覧の集まり。**ラッチを消費するのはここだけ**で、消費したら必ず全部の素材を
/// 同時に作り直す（`rescan_model_lists`）。1 つだけ作り直せる関数を公開しないのは、片方だけ
/// 古くなる経路を塞ぐため——とくに**カタログ外ファイルの削除は取得の記録を変えない**ので、
/// tick のラッチでは永久に追いつけない（消えた行が残り、削除できるように見え続ける）。
#[derive(Clone)]
pub(crate) struct ModelLists {
    shared: Rc<ModelScanShared>,
    pub(crate) transcription: ModelListHandles,
    pub(crate) minutes: ModelListHandles,
}

impl ModelLists {
    pub(crate) fn new() -> Self {
        Self {
            shared: Rc::new(ModelScanShared {
                override_files: RefCell::new(OverrideFiles::default()),
                downloaded_seen: RefCell::new(Vec::new()),
            }),
            transcription: ModelListHandles::new(SPEECH_KINDS),
            minutes: ModelListHandles::new(SUMMARY_KINDS),
        }
    }

    fn all(&self) -> [&ModelListHandles; 2] {
        [&self.transcription, &self.minutes]
    }

    /// 取得の記録が前の走査から変わったか（走査し直す契機）。**値で比べて `Ref` を残さない**。
    pub(crate) fn downloads_changed(&self, downloader: &model_download::ModelDownloader) -> bool {
        downloaded_ids(downloader) != *self.shared.downloaded_seen.borrow()
    }
}

/// 文字起こしウィンドウが出す種別。**2 つの一覧の和が登録簿を覆う**ことは
/// `every_registered_catalog_appears_in_a_window` が検査する（種別を足して両方に入れ忘れると、
/// そのモデルはどちらの一覧にも出ず、削除もできなくなる）。
pub(crate) const SPEECH_KINDS: &[model_download::ModelKind] = &[model_download::ModelKind::Speech];
/// 議事録ウィンドウが出す種別（理由は `SPEECH_KINDS`）。
pub(crate) const SUMMARY_KINDS: &[model_download::ModelKind] =
    &[model_download::ModelKind::Summary];

/// どのウィンドウの操作で作り直すか。**操作元と巻き込まれた側で理由が変わる**ので、
/// 呼び出し側が必ず自分を名乗る（名乗らないと、片方の通知がもう片方へ出る）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ListOrigin {
    Transcription,
    Minutes,
    /// どちらのウィンドウでもない（tick）。通知に触らない理由しか来ないので、区別は要らない。
    Tick,
}

/// 「作り直して、生きているウィンドウへ反映する」処理。
///
/// **2 つのウィンドウが互いを更新し合う**ので、実体は両方を作ってからでないと組めない。そこで
/// 各ウィンドウには「あとで入る箱」を渡し、`main` が最後に実体を入れる。
pub(crate) type RefreshLists = Rc<dyn Fn(ModelsRefresh, ListOrigin)>;
/// その箱（生成順の都合で後入れになる。`main` が 1 つだけ持つ）。
pub(crate) type RefreshSlot = Rc<RefCell<Option<RefreshLists>>>;

/// 一覧を組み直すのに要るもの一式。**引数を数えて分けるのではなく、意味の単位でまとめる**
/// （どれか 1 つだけ古いものを渡す事故を型で防ぐ）。
#[derive(Clone)]
pub(crate) struct ModelWorkers {
    pub(crate) lists: ModelLists,
    pub(crate) downloader: model_download::ModelDownloader,
    /// 削除できるかの判定に読む（ジョブがある間は消させない）。
    pub(crate) transcriber: transcribe::TranscribeWorker,
    pub(crate) summarizer: summarize::SummarizeWorker,
}

/// 一覧を載せているウィンドウ。**表示の更新だけ**を受け持つ（素材には触らせない——
/// ビューとハンドルの取り違えで「別の一覧の添字で素材を引く」事故を型で防ぐ）。
pub(crate) trait ModelListView {
    /// このビューが出す一覧。**ウィンドウの型が決める**ので、呼び出し側が選べない
    /// （選べると、議事録のウィンドウに文字起こしの一覧を渡しても通ってしまう）。
    fn handles<'a>(&self, lists: &'a ModelLists) -> &'a ModelListHandles;
    /// このビューで操作したときの出どころ（通知の宛先を決める）。同じ理由でビューが答える。
    fn origin(&self) -> ListOrigin;
    fn total_text(&self) -> slint::SharedString;
    fn set_total_text(&self, text: slint::SharedString);
    fn notice(&self) -> slint::SharedString;
    fn set_notice(&self, text: slint::SharedString);
    fn set_show_delete_confirm(&self, value: bool);
    fn set_delete_index(&self, value: i32);
}

/// 一覧の 3 操作（Use / Download / Delete）を、そのウィンドウの一覧へ配線する。
///
/// **どの一覧を触るかはウィンドウの型が答える**（`ModelListView::handles` / `origin`）。呼び出し側は
/// ウィンドウを渡すだけで一覧も通知の宛先も選べないので、**組を間違えられるのは
/// `impl_model_list_view!` の 1 行だけ**になる。型では防げない（`ModelListHandles` はどちらの
/// 一覧も同じ型）ので、そこは `each_window_owns_its_own_list` が検査する。
///
/// 間違えたときに起きるのは、**添字は正しいのに別のモデルを消す**こと——カタログ外の行は両方の
/// 一覧の末尾に出るので、行数が同じなら例外も出ない。
///
/// マクロなのは、Slint の生成型に共通のコールバック登録トレイトが無いため（`on_use_model` などは
/// 型ごとの固有メソッドで、ジェネリクスからは呼べない）。
macro_rules! wire_model_list {
    ($window:expr, $config:expr, $workers:expr, $refresh:expr) => {{
        let handles = models::ModelListView::handles(&$window, &$workers.lists).clone();
        let origin = models::ModelListView::origin(&$window);
        {
            let handles = handles.clone();
            let config = std::rc::Rc::clone($config);
            let downloader = $workers.downloader.clone();
            let refresh = std::rc::Rc::clone(&$refresh);
            $window.on_use_model(move |index| {
                if let RowAction::Done(notice) =
                    models::use_model_at(&handles, index, &config, &downloader)
                {
                    refresh(ModelsRefresh::AfterOperation(notice), origin);
                }
            });
        }
        {
            let handles = handles.clone();
            let downloader = $workers.downloader.clone();
            let refresh = std::rc::Rc::clone(&$refresh);
            $window.on_download_model(move |index| {
                if let RowAction::Done(notice) =
                    models::download_model_at(&handles, index, &downloader)
                {
                    refresh(ModelsRefresh::AfterOperation(notice), origin);
                }
            });
        }
        {
            let workers = $workers.clone();
            let config = std::rc::Rc::clone($config);
            let refresh = std::rc::Rc::clone(&$refresh);
            $window.on_delete_model(move |index| {
                if let RowAction::Done(notice) =
                    models::delete_model_at(&workers, &handles, index, &config)
                {
                    refresh(ModelsRefresh::AfterOperation(notice), origin);
                }
            });
        }
    }};
}
pub(crate) use wire_model_list;

/// Slint が生成したウィンドウ型へ `ModelListView` を実装する（委譲するだけ）。
macro_rules! impl_model_list_view {
    ($window:ty, $field:ident, $origin:ident) => {
        impl $crate::windows::models::ModelListView for $window {
            fn handles<'a>(
                &self,
                lists: &'a $crate::windows::models::ModelLists,
            ) -> &'a $crate::windows::models::ModelListHandles {
                &lists.$field
            }
            fn origin(&self) -> $crate::windows::models::ListOrigin {
                $crate::windows::models::ListOrigin::$origin
            }
            fn total_text(&self) -> slint::SharedString {
                <$window>::get_total_text(self)
            }
            fn set_total_text(&self, text: slint::SharedString) {
                <$window>::set_total_text(self, text);
            }
            fn notice(&self) -> slint::SharedString {
                <$window>::get_notice(self)
            }
            fn set_notice(&self, text: slint::SharedString) {
                <$window>::set_notice(self, text);
            }
            fn set_show_delete_confirm(&self, value: bool) {
                <$window>::set_show_delete_confirm(self, value);
            }
            fn set_delete_index(&self, value: i32) {
                <$window>::set_delete_index(self, value);
            }
        }
    };
}

impl_model_list_view!(crate::TranscriptionWindow, transcription, Transcription);
impl_model_list_view!(crate::MinutesWindow, minutes, Minutes);

/// 一覧の 1 行の素材。**カタログ全件**（未取得を含む）と、`models/` にあるカタログ外のファイルを
/// 種別ごとに並べたもの（#138。#117 の「ディスクにあるものだけ」から広げた）。
///
/// 行の並びが UI のインデックスと 1 対 1 なので、ここで作った順序を UI へ渡すまで変えない
/// （並べ替えると**別のモデルを消す**）。
#[derive(Debug, Clone)]
pub(crate) enum ModelRowSource {
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
    pub(crate) fn installed(&self) -> Option<&model_download::InstalledModel> {
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
        model_download::ModelKind::Summary => "Meeting notes — LLM",
    }
}

/// 行の素材を組む。**カタログの登録簿の順**（種別ごとに見出し → カタログの並び）で、最後に
/// カタログ外のファイルを大きい順で置く。
fn model_row_sources(
    installed: Vec<model_download::InstalledModel>,
    kinds: &[model_download::ModelKind],
) -> Vec<ModelRowSource> {
    let mut sources: Vec<ModelRowSource> = Vec::new();
    for (kind, catalog, _) in model_download::REGISTERED_CATALOGS {
        // 出さない種別は見出しごと落とす（見出しだけが残ると、中身の無い区切りになる）。
        if !kinds.contains(kind) {
            continue;
        }
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
    /// その機能の自動実行が ON か。**表示の語を変えるためだけに持つ**（可否には効かせない——
    /// 機能を OFF にした後こそ、取得済みモデルの掃除をしたい）。
    speech_on: bool,
    summary_on: bool,
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
        speech_on: config.auto_transcribe,
        summary_on: config.auto_summarize,
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

/// その種別の自動実行が ON か（**網羅 match**。種別を足したら扱いを書くまで通らない）。
fn kind_is_on(context: &ModelsContext, kind: model_download::ModelKind) -> bool {
    match kind {
        model_download::ModelKind::Speech => context.speech_on,
        model_download::ModelKind::Summary => context.summary_on,
    }
}

/// その種別を指す語（文言に混ぜる。`selected for transcription` のように使う）。
fn kind_noun(kind: model_download::ModelKind) -> &'static str {
    match kind {
        model_download::ModelKind::Speech => "transcription",
        model_download::ModelKind::Summary => "meeting notes",
    }
}

/// その種別の仕事を指す語（`a recording is being transcribed now` のように使う）。
fn kind_job_phrase(kind: model_download::ModelKind) -> &'static str {
    match kind {
        model_download::ModelKind::Speech => "a recording is being transcribed now",
        model_download::ModelKind::Summary => "a recording is being summarized now",
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
pub(crate) struct RowFacts {
    usage: RowUsage,
    /// その行のファイルを読むジョブが在るか（削除させない条件）。
    pub(crate) busy: bool,
}

pub(crate) fn row_facts(source: &ModelRowSource, context: &ModelsContext) -> RowFacts {
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
fn model_status_part(status: ModelStatus, reason: Option<&str>) -> String {
    match status {
        ModelStatus::NotDownloaded => "Not downloaded".to_owned(),
        // 進捗の数値は**別の列**が持つ（`progress_detail`）。ここに混ぜると桁が増えたときに
        // 右のボタンが動く。
        ModelStatus::Downloading => "Downloading…".to_owned(),
        ModelStatus::Installed => "Downloaded".to_owned(),
        ModelStatus::Failed => match reason {
            Some(reason) => format!("Download failed — {reason}"),
            None => "Download failed".to_owned(),
        },
    }
}

/// 使用状況の文言（**網羅 match**。`Idle` は付け足す語が無い）。
///
/// 選択中の行は、その機能が動いているかで言うことが変わる——OFF のまま「これで文字起こしする」と
/// 書くと、動いていないのに動いていると読める。
fn model_usage_part(
    usage: RowUsage,
    kind: Option<model_download::ModelKind>,
    context: &ModelsContext,
) -> Option<String> {
    match usage {
        RowUsage::Idle => None,
        RowUsage::Selected | RowUsage::InConfig => kind.map(|kind| {
            if kind_is_on(context, kind) {
                format!("selected for {}", kind_noun(kind))
            } else {
                "will be used once the switch above is on".to_owned()
            }
        }),
        RowUsage::Overridden => Some("not used because config.toml sets the model file".to_owned()),
        RowUsage::Unknown => Some(
            "not recognized, so shoki cannot tell which feature it belongs to. It is never used; \
             deleting it here only removes the file"
                .to_owned(),
        ),
    }
}

/// 削除できない理由の文言（ボタンが淡色になるだけでは理由が分からないので文字で出す）。
fn model_busy_part(kind: Option<model_download::ModelKind>) -> String {
    match kind {
        Some(kind) => format!(
            "{}, so it cannot be deleted until that finishes",
            kind_job_phrase(kind)
        ),
        // カタログ外のファイルは、それを開いている種別のジョブが在るときだけ busy になる
        // （どちらの種別かはここでは分からないので、種別を出さない言い方にする）。
        None => "it is in use right now, so it cannot be deleted until that finishes".to_owned(),
    }
}

/// 行の状態テキスト。**3 つの表を `·` でつなぐ**（取得の状態・使用状況・使用中）。
/// 組み合わせを 1 つの表にすると状態 × 状況で行数が掛け算になるため、軸ごとに分ける。
fn model_row_status_text(
    status: ModelStatus,
    reason: Option<&str>,
    facts: &RowFacts,
    kind: Option<model_download::ModelKind>,
    context: &ModelsContext,
) -> String {
    let mut parts = vec![model_status_part(status, reason)];
    parts.extend(model_usage_part(facts.usage, kind, context));
    if facts.busy {
        parts.push(model_busy_part(kind));
    }
    format!("{}.", parts.join(" · "))
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
pub(crate) const MODELS_EMPTY_TEXT: &str = "No models available";

/// 走査そのものに失敗したときの通知（カタログの行は必ず並ぶので、空表示では気づけない。
/// 行のサイズ・状態がディスクの実体を反映しないことも伝える）。
const MODELS_UNREADABLE_NOTICE: &str =
    "Could not list the models folder — sizes and states may be out of date.";

/// 押された時点で使用中だったときの通知（一覧の状態テキストと同じ事実を指すので、語を揃える）。
const MODEL_IN_USE_NOTICE: &str = "This model is in use right now — it was not deleted.";

/// 削除できなかった理由（一覧の `notice`）。理由まで出すのは「押しても無反応」に見せない
/// ため。使用中は UI で押させないのが基本なので、ここへ来るのは一覧が古かったときだけ。
const MODEL_DELETE_FAILED_NOTICE: &str = "Could not delete this model — see the log for details.";

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
    let kind = match source {
        ModelRowSource::Catalog { kind, .. } => Some(*kind),
        // カタログ外のファイルは種別を判定できない（だから両方の一覧に出す）。
        _ => None,
    };
    let transfer = match recorded {
        Some(model_download::DownloadStatus::Downloading { received, total }) => {
            Some((received, total))
        }
        _ => None,
    };
    let percent = transfer.map_or(0, |(received, total)| download_percent(received, total));
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
        status_text: model_row_status_text(status, failure_reason, &facts, kind, context).into(),
        tone: row_tone(status, &facts),
        // 取得中だけバーを出す（`-1` は「出さない」の約束。`StatusLine`）。
        progress: transfer.map_or(-1.0, |_| percent as f32 / 100.0),
        progress_detail: transfer
            .map(|(received, total)| progress_detail(received, total, percent))
            .unwrap_or_default()
            .into(),
        badge: row_badge(&facts, kind, context).into(),
        // カタログ外のファイル名だけ等幅で出す（拡張子やハッシュを読み取りやすく）。
        mono: matches!(source, ModelRowSource::Extra(_)),
        delete_detail: model_delete_detail(facts.usage, size_bytes).into(),
        status,
        can_use: can_use_row(source, &facts),
        can_download: can_download_row(status, source, &facts),
        can_delete: can_delete_row(source, &facts),
    }
}

/// 取得の進捗の内訳（`1.9 / 3.1 GB · 62%`）。**状態の文言とは別の列**に出すので、ここは
/// 数値だけを組む。
fn progress_detail(received: u64, total: u64, percent: u64) -> String {
    if total == 0 {
        // 全体長が分からない配信では割合を出せない（受信量だけ出す）。
        return format!("{} received", model_download::format_size(received));
    }
    format!(
        "{} / {} · {percent}%",
        model_download::format_size(received),
        model_download::format_size(total)
    )
}

/// 行の状態の意味（**網羅 match**。色の対応表は `Style` の 1 箇所）。
fn row_tone(status: ModelStatus, facts: &RowFacts) -> StatusTone {
    match status {
        ModelStatus::Failed => StatusTone::Danger,
        ModelStatus::Downloading => StatusTone::Active,
        // 上書きやカタログ外は「そのままでは意図どおり動かない」側（`StatusTone` の定義）。
        ModelStatus::Installed | ModelStatus::NotDownloaded => match facts.usage {
            RowUsage::Overridden | RowUsage::Unknown => StatusTone::Caution,
            RowUsage::Selected | RowUsage::InConfig => StatusTone::Done,
            RowUsage::Idle => StatusTone::Neutral,
        },
    }
}

/// 使用中の標（**その機能が動いているかで語を変える**）。空なら出さない。
///
/// `In use` は「いまこれで動いている」、`Selected` は「選ばれてはいるが、機能が OFF なので
/// まだ動いていない」。同じ状態に同じ語を使うと、OFF のまま待っている人が「動いている」と
/// 読んでしまう。
fn row_badge(
    facts: &RowFacts,
    kind: Option<model_download::ModelKind>,
    context: &ModelsContext,
) -> &'static str {
    match (facts.usage, kind) {
        (RowUsage::Selected | RowUsage::InConfig, Some(kind)) => {
            if kind_is_on(context, kind) {
                "In use"
            } else {
                "Selected"
            }
        }
        _ => "",
    }
}

/// 記録が「取得済み」になっているカタログ ID（**登録簿の並び順。全種別**）。
///
/// tick はディスクを走査しないので、取得が完了しても行のサイズと合計は追いつかない。この集合が
/// **前の走査から変わったとき**だけ 1 回走査し直す（`ModelLists::downloads_changed`）。
/// 「記録は取得済みなのに実体が無い」を条件にすると、外部でファイルを消された・走査に失敗した
/// といった**解消しない不一致**で毎 tick 走査が走り続ける。
///
/// 素材ではなく**登録簿**から作るのは、一覧を種別で絞ったあとも同じラッチを共有するため
/// （絞った素材から作ると、開いていない種別の取得完了を取りこぼす）。
fn downloaded_ids(downloader: &model_download::ModelDownloader) -> Vec<&'static str> {
    model_download::REGISTERED_CATALOGS
        .iter()
        .flat_map(|(_, catalog, _)| catalog.iter())
        .filter(|spec| {
            matches!(
                downloader.recorded_status(spec.id),
                Some(model_download::DownloadStatus::Downloaded)
            )
        })
        .map(|spec| spec.id)
        .collect()
}

/// 同じ値なら書かない（毎 tick の無駄な再描画を避ける）。**UI の現在値と比べる**ので、別経路が
/// 書き換えた後でも正しく追いつく（こちらで前回値を覚えると、その経路とずれる）。
pub(crate) fn set_if_changed(
    current: impl Fn() -> slint::SharedString,
    set: impl Fn(slint::SharedString),
    next: String,
) {
    if current() != next.as_str() {
        set(next.into());
    }
}

/// 扉に出す「取得済みの件数」（`2 of 6 models downloaded`）。**カタログの件数を分母にする**——
/// 「何個あって、いくつ落ちているか」が分かると、開かずに掃除の要否を判断できる。
pub(crate) fn downloaded_count_text(
    catalog: &'static [model_download::ModelSpec],
    downloader: &model_download::ModelDownloader,
) -> String {
    let downloaded = catalog
        .iter()
        .filter(|spec| {
            matches!(
                downloader.recorded_status(spec.id),
                Some(model_download::DownloadStatus::Downloaded)
            )
        })
        .count();
    format!("{downloaded} of {} models downloaded", catalog.len())
}

/// 一覧の行に対する操作の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RowAction {
    /// 何かした（`Some` は出す通知）。**素材を作り直す必要がある**。
    Done(Option<&'static str>),
    /// **その行にそのボタンを出していない**（見出し・カタログ外など）ので、押しようがない。
    /// 「押したのに無反応」に見える経路をここへ落とさないこと——失敗は `Done` で通知を出す。
    Ignored,
}

/// 添字からカタログの行を引く。**引くのは必ずそのウィンドウ自身の素材**（別の一覧の素材を
/// 引くと、添字は正しいのに別のモデルを操作する）。
fn catalog_at(handles: &ModelListHandles, index: i32) -> Option<ModelRowSource> {
    let source = usize::try_from(index)
        .ok()
        .and_then(|i| handles.sources.borrow().get(i).cloned())?;
    matches!(source, ModelRowSource::Catalog { .. }).then_some(source)
}

/// 「使う」: その行のモデルを使う設定にする。
pub(crate) fn use_model_at(
    handles: &ModelListHandles,
    index: i32,
    config: &Rc<RefCell<Config>>,
    downloader: &model_download::ModelDownloader,
) -> RowAction {
    let Some(ModelRowSource::Catalog { kind, spec, .. }) = catalog_at(handles, index) else {
        return RowAction::Ignored; // 見出し・カタログ外の行では Use を出していない。
    };
    let saved = select_model(kind, spec, config, downloader);
    RowAction::Done((!saved).then_some(MODEL_SELECT_FAILED_NOTICE))
}

/// 「取得する」: 選択は変えずに取得だけ始める（未取得・失敗の行にだけ出る）。
pub(crate) fn download_model_at(
    handles: &ModelListHandles,
    index: i32,
    downloader: &model_download::ModelDownloader,
) -> RowAction {
    let Some(ModelRowSource::Catalog { spec, .. }) = catalog_at(handles, index) else {
        return RowAction::Ignored;
    };
    // 取得済み・DL 中なら request_download 側が早期 return する。進捗は tick が拾う。
    downloader.request_download(spec);
    RowAction::Done(None)
}

/// 「削除」: 確認モーダルの確定から呼ばれる。
///
/// **押された時点で使用中を再確認する**。一覧は tick が状態を追うが、tick とクリックの間に
/// ジョブが始まることはありうる。取得中の拒否は基盤側が持つ。
pub(crate) fn delete_model_at(
    workers: &ModelWorkers,
    handles: &ModelListHandles,
    index: i32,
    config: &Rc<RefCell<Config>>,
) -> RowAction {
    let Some(source) = usize::try_from(index)
        .ok()
        .and_then(|i| handles.sources.borrow().get(i).cloned())
    else {
        // 添字が範囲外なのは、モーダルを開いている間に並びが変わったとき（本来は畳んでいる）。
        // **`Ignored` にしない**——確定を押したのに何も起きない、という見え方を残さない。
        eprintln!("Skipping the model deletion because the row is no longer listed");
        return RowAction::Done(delete_failure_notice(DeleteOutcome::Failed));
    };
    let Some(target) = source.installed().cloned() else {
        // Delete はディスクに実体がある行にしか出さないので通常は来ないが、「押しても無反応」に
        // 見せないため通知とログを残す（#117 の方針）。
        eprintln!("Skipping the model deletion because the file is no longer listed");
        return RowAction::Done(delete_failure_notice(DeleteOutcome::Failed));
    };
    // **判定はブロックに閉じて借用を先に返す**。呼び出し側はこの直後に作り直しへ進み、そこで
    // 共有状態を `borrow_mut` するので、`Ref` を持ったままだと `BorrowMutError` で
    // **アプリごと落ちる**（`ModelScanShared` の doc）。
    let busy = {
        let config = config.borrow();
        let override_files = workers.lists.shared.override_files.borrow().clone();
        let context = models_context(
            &workers.transcriber,
            &workers.summarizer,
            &workers.downloader,
            &config,
            &override_files,
        );
        row_facts(&source, &context).busy
    };
    let outcome = if busy {
        DeleteOutcome::InUse
    } else {
        match workers.downloader.delete(&target) {
            Ok(()) => DeleteOutcome::Deleted,
            Err(err) => {
                // 文言にフルパスは含めない（`docs/rules/security.md`）。
                eprintln!("Skipping the model deletion because {err}");
                DeleteOutcome::Failed
            }
        }
    };
    RowAction::Done(delete_failure_notice(outcome))
}

/// 選び直しで取得を打ち切るモデルの ID（打ち切らないなら `None`）。
///
/// 同じモデルを選び直したときに打ち切らないためのガード。一覧の「Use」は
/// **選択中の行でも押せる**ので、ここを外すと「押したら自分のダウンロードが止まって数 GB を
/// 捨てる」ことになる（`request_download` が拾い直すので止まりっぱなしにはならないが、
/// 受信済みのぶんは戻らない）。
fn model_to_cancel_on_select<'a>(
    previous_id: &'a str,
    selected: &'static model_download::ModelSpec,
) -> Option<&'a str> {
    (previous_id != selected.id).then_some(previous_id)
}

/// 使うモデルを選び直して設定へ永続化する。成功したら `true`。
///
/// **入口は一覧の「Use」だけ**（#141 で設定画面の選択 UI を廃した）。
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
        // どの種別の話か分かるようにする（入口は 2 つ＝それぞれの機能ウィンドウの
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

    /// テストで「全種別」を渡すための定数。**本番にこの並びは無い**（ウィンドウごとに片方だけを
    /// 出す）ので、ここで組む。
    const ALL_KINDS: &[crate::model_download::ModelKind] = &[
        crate::model_download::ModelKind::Speech,
        crate::model_download::ModelKind::Summary,
    ];

    /// 選び直しで打ち切るのは「別のモデルに変わったとき」だけ。同じモデルを選び直しても
    /// （一覧の「Use」は選択中の行でも押せる）自分の取得は止めない。
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
        let sources = super::model_row_sources(
            vec![
                installed_spec(crate::model_download::ModelKind::Speech, speech_spec(), 100),
                extra_file("left-over.bin", 20),
            ],
            ALL_KINDS,
        );

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
        let sources = super::model_row_sources(Vec::new(), ALL_KINDS);
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
            speech_on: true,
            summary_on: true,
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

    /// 走査したら**両方の一覧とラッチを同時に**作り直す。片方だけ作り直せる経路があると、
    /// カタログ外ファイルの削除がもう片方に反映されない（取得の記録が変わらないので、tick の
    /// ラッチでは永久に追いつけない）。
    #[test]
    fn a_rescan_reseeds_every_list_and_the_latch() {
        let downloader = crate::model_download::ModelDownloader::new();
        let lists = super::ModelLists::new();
        downloader.set_status_for_test(
            speech_spec(),
            crate::model_download::DownloadStatus::Downloaded,
        );
        assert!(
            lists.downloads_changed(&downloader),
            "the latch starts empty, so a recorded download counts as a change"
        );

        super::rescan_model_lists(&lists, &downloader, &crate::config::Config::default());

        assert!(
            !lists.downloads_changed(&downloader),
            "the rescan must consume the change, or the tick rescans every 100ms"
        );
        // 両方の一覧に素材が入る（片方だけ作り直していないこと）。
        assert!(
            !lists.transcription.sources.borrow().is_empty(),
            "the transcription list must be seeded"
        );
        assert!(
            !lists.minutes.sources.borrow().is_empty(),
            "the meeting notes list must be seeded too"
        );
        // それぞれ自分の種別だけを出す。
        let speech_names = super::model_rows(
            &lists.transcription.sources.borrow(),
            &context(&downloader, false, false),
        );
        assert!(
            speech_names
                .iter()
                .all(|row| row.name != summary_spec().display_name),
            "the transcription list must not carry summary models"
        );
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

    /// 取得の状態の文言（全バリアント）。**進捗の数値は含めない**（別の列が持つ）。
    #[test]
    fn model_status_part_covers_all_states() {
        assert_eq!(
            super::model_status_part(ModelStatus::NotDownloaded, None),
            "Not downloaded"
        );
        assert_eq!(
            super::model_status_part(ModelStatus::Downloading, None),
            "Downloading…"
        );
        assert_eq!(
            super::model_status_part(ModelStatus::Installed, None),
            "Downloaded"
        );
        // 取得の入口が一覧にもあるので、**失敗の理由まで行に出す**（扉の状態行は選択中の
        // モデルしか出さない）。
        assert_eq!(
            super::model_status_part(ModelStatus::Failed, Some("not enough free disk space")),
            "Download failed — not enough free disk space"
        );
        assert_eq!(
            super::model_status_part(ModelStatus::Failed, None),
            "Download failed"
        );
    }

    /// 使用状況の文言（全バリアント）。`Idle` は付け足す語が無い。
    ///
    /// 選択中の行は**その機能が動いているかで言うことが変わる**（OFF のまま「これで文字起こし
    /// する」と書くと、動いていないのに動いていると読める）。
    #[test]
    fn model_usage_part_covers_all_states() {
        let downloader = crate::model_download::ModelDownloader::new();
        let on = context(&downloader, false, false);
        let mut off = context(&downloader, false, false);
        off.speech_on = false;
        let speech = Some(crate::model_download::ModelKind::Speech);

        assert_eq!(
            super::model_usage_part(super::RowUsage::Idle, speech, &on),
            None
        );
        assert_eq!(
            super::model_usage_part(super::RowUsage::Selected, speech, &on),
            Some("selected for transcription".to_owned())
        );
        assert_eq!(
            super::model_usage_part(super::RowUsage::Selected, speech, &off),
            Some("will be used once the switch above is on".to_owned()),
            "a selected model must not claim to be running while the feature is off"
        );
        assert_eq!(
            super::model_usage_part(super::RowUsage::InConfig, speech, &on),
            Some("selected for transcription".to_owned())
        );
        assert_eq!(
            super::model_usage_part(super::RowUsage::Overridden, speech, &on),
            Some("not used because config.toml sets the model file".to_owned())
        );
        assert!(
            super::model_usage_part(super::RowUsage::Unknown, None, &on)
                .is_some_and(|text| text.starts_with("not recognized")),
            "an unknown file must say why it is not used"
        );
    }

    /// 状態テキストは 3 つの軸を `·` でつなぎ、文末にピリオドを置く（削除できない理由もここ）。
    #[test]
    fn model_row_status_text_joins_the_axes() {
        let downloader = crate::model_download::ModelDownloader::new();
        let context = context(&downloader, false, false);
        let speech = Some(crate::model_download::ModelKind::Speech);
        let idle = super::RowFacts {
            usage: super::RowUsage::Idle,
            busy: false,
        };
        assert_eq!(
            super::model_row_status_text(ModelStatus::NotDownloaded, None, &idle, speech, &context),
            "Not downloaded."
        );
        let selected_busy = super::RowFacts {
            usage: super::RowUsage::Selected,
            busy: true,
        };
        assert_eq!(
            super::model_row_status_text(
                ModelStatus::Installed,
                None,
                &selected_busy,
                speech,
                &context
            ),
            "Downloaded · selected for transcription · a recording is being transcribed now, so it \
             cannot be deleted until that finishes."
        );
        let in_config = super::RowFacts {
            usage: super::RowUsage::InConfig,
            busy: false,
        };
        assert_eq!(
            super::model_row_status_text(
                ModelStatus::Downloading,
                None,
                &in_config,
                speech,
                &context
            ),
            "Downloading… · selected for transcription."
        );
    }

    /// 取得中は**進捗の内訳を別の列**に出す（状態の文言に混ぜると、桁が増えたときに右のボタンが
    /// 動く）。全体長が分からない配信では割合を出さない。
    #[test]
    fn the_progress_detail_is_a_column_of_its_own() {
        // サイズの表記は `format_size` に従う（2 進接頭辞）。
        assert_eq!(
            super::progress_detail(1_900_000_000, 3_100_000_000, 62),
            "1.8 GB / 2.9 GB · 62%"
        );
        assert_eq!(
            super::progress_detail(120_000_000, 0, 0),
            "114 MB received",
            "without a total there is no percentage to show"
        );
    }

    /// 種別で絞ると、**その種別の見出しと行だけ**が残る。カタログ外のファイル（と見出し）は
    /// 種別に属さないので常に末尾に残る——どの一覧から見ても掃除できないと、置き去りの
    /// ファイルを消す手段が無くなる。
    ///
    /// 絞るのは**素材**で、行ではない。行だけを絞ると UI の添字が素材の添字とずれて、
    /// **別のモデルを消す・別のモデルを選ぶ**（下の 1 対 1 の検査がそこを押さえる）。
    #[test]
    fn narrowing_to_one_kind_drops_its_heading_and_rows() {
        let downloader = crate::model_download::ModelDownloader::new();
        let speech_only = super::model_row_sources(
            vec![extra_file("left-over.bin", 20)],
            &[crate::model_download::ModelKind::Speech],
        );
        let rows = super::model_rows(&speech_only, &context(&downloader, false, false));

        let names: Vec<&str> = rows.iter().map(|row| row.name.as_str()).collect();
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

        // **行と素材は同じ添字で同じものを指す**。UI が渡してくる添字はこの前提で素材を引く
        // （`main` の Use / Download / Delete）ので、ここがずれると別のモデルを操作する。
        assert_eq!(rows.len(), speech_only.len());
        for (index, row) in rows.iter().enumerate() {
            let expected = match &speech_only[index] {
                super::ModelRowSource::Heading(title) => (*title).to_owned(),
                super::ModelRowSource::Catalog { spec, .. } => spec.display_name.to_owned(),
                super::ModelRowSource::Extra(installed) => installed.filename.clone(),
            };
            assert_eq!(row.name, expected, "row {index} must match its source");
        }
    }

    /// **ウィンドウと一覧の組**（`impl_model_list_view!` の 1 行）が入れ違っていないか。
    ///
    /// 型では防げない（`ModelListHandles` はどちらの一覧も同じ型）。入れ違うと、文字起こしの
    /// 一覧で Del を押したときに要約 LLM が消える——添字は正しいので例外も出ない。
    #[test]
    #[cfg_attr(
        not(slint_debug_info),
        ignore = "needs Slint debug info (SLINT_EMIT_DEBUG_INFO=1)"
    )]
    fn each_window_owns_its_own_list() {
        use super::ModelListView as _;
        crate::init_test_backend();
        let lists = super::ModelLists::new();

        let transcription =
            crate::TranscriptionWindow::new().expect("create the transcription window");
        assert_eq!(
            transcription.handles(&lists).kinds,
            super::SPEECH_KINDS,
            "the transcription window must own the speech list"
        );
        assert_eq!(transcription.origin(), super::ListOrigin::Transcription);

        let minutes = crate::MinutesWindow::new().expect("create the meeting notes window");
        assert_eq!(
            minutes.handles(&lists).kinds,
            super::SUMMARY_KINDS,
            "the meeting notes window must own the summary list"
        );
        assert_eq!(minutes.origin(), super::ListOrigin::Minutes);
    }

    /// 通知の宛先は**操作元だけ**。議事録側で失敗した通知が文字起こし側に出てはいけないし、
    /// その逆もいけない（`ListOrigin` を渡し忘れると、片方の画面が無反応になる）。
    #[test]
    fn only_the_window_that_acted_gets_the_notice() {
        let failed = super::ModelsRefresh::AfterOperation(Some("boom"));
        // 操作元はそのまま、他方は通知に触らない理由へ落ちる。
        assert_eq!(failed.notice(), super::NoticeUpdate::Set(Some("boom")));
        assert_eq!(failed.elsewhere().notice(), super::NoticeUpdate::Keep);
        // 走査由来の理由は「操作」ではないので、`elsewhere` でも姿が変わらない。
        for cause in [super::ModelsRefresh::Rescan, super::ModelsRefresh::Poll] {
            assert_eq!(cause.elsewhere(), cause, "{cause:?} is not an operation");
        }
    }

    /// 巻き込まれた側は**モーダルを畳むが通知は触らない**。
    ///
    /// 畳まないと、並びが変わったあとのモーダルが別の行を指す（**表示と違うモデルを消す**）。
    /// 通知まで差し替えると、触っていない画面に他人の失敗が出るか、出ていた通知が黙って消える。
    #[test]
    fn a_refresh_elsewhere_folds_the_modal_but_keeps_the_notice() {
        let elsewhere = super::ModelsRefresh::AfterOperationElsewhere;
        assert!(
            elsewhere.resets_modal(),
            "the other window's rows change too, so its modal must fold"
        );
        assert_eq!(
            elsewhere.notice(),
            super::NoticeUpdate::Keep,
            "a window that was not operated must keep its own notice"
        );
        // 操作した側は通知を差し替える。
        assert_eq!(
            super::ModelsRefresh::AfterOperation(Some("boom")).notice(),
            super::NoticeUpdate::Set(Some("boom"))
        );
    }

    /// **ユーザー操作で作り直す理由はモーダルを畳む**。畳まないまま並びが変わると添字がずれる。
    #[test]
    fn every_operation_cause_folds_the_modal() {
        for cause in [
            super::ModelsRefresh::AfterOperation(None),
            super::ModelsRefresh::AfterOperationElsewhere,
        ] {
            assert!(cause.resets_modal(), "{cause:?} reseeds, so it must fold");
        }
        // `Rescan` はモーダルが閉じているときだけ走らせる、が呼び出し側の約束（tick のガード）。
        assert!(!super::ModelsRefresh::Poll.resets_modal());
    }

    /// **2 つのウィンドウの種別の和が登録簿を覆う**か。足し忘れると、その種別はどちらの一覧にも
    /// 出ず（カタログにあるので「カタログ外」にも落ちない）、UI から消せなくなる。
    #[test]
    fn every_registered_catalog_appears_in_a_window() {
        for (kind, _, _) in crate::model_download::REGISTERED_CATALOGS {
            assert!(
                super::SPEECH_KINDS.contains(kind) || super::SUMMARY_KINDS.contains(kind),
                "{kind:?} is in REGISTERED_CATALOGS but in neither window's list \
                 — add it to SPEECH_KINDS or SUMMARY_KINDS, or its models cannot be deleted"
            );
        }
    }

    /// 行の並びは素材の順のまま（UI のインデックスが操作対象を指すので、ここで並べ替えると
    /// **別のモデルを消す・別のモデルを選ぶ**）。見出しは操作を持たない。
    #[test]
    fn model_rows_keep_the_order_of_the_sources() {
        let downloader = crate::model_download::ModelDownloader::new();
        let sources = super::model_row_sources(
            vec![
                installed_spec(
                    crate::model_download::ModelKind::Summary,
                    summary_spec(),
                    4_000_000_000,
                ),
                extra_file("left-over.bin", 20),
            ],
            ALL_KINDS,
        );
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
        let none_installed = super::model_row_sources(Vec::new(), ALL_KINDS);
        assert_eq!(super::models_total_text(&none_installed), "");

        let one = super::model_row_sources(
            vec![installed_spec(
                crate::model_download::ModelKind::Speech,
                speech_spec(),
                77_691_713,
            )],
            ALL_KINDS,
        );
        assert_eq!(super::models_total_text(&one), "1 model — 74 MB");

        let two = super::model_row_sources(
            vec![
                installed_spec(
                    crate::model_download::ModelKind::Speech,
                    speech_spec(),
                    1_624_555_275,
                ),
                extra_file("left-over.bin", 1_624_555_275),
            ],
            ALL_KINDS,
        );
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
        assert!(
            super::downloaded_ids(&downloader).is_empty(),
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
        assert!(super::downloaded_ids(&downloader).is_empty());

        downloader.set_status_for_test(
            speech_spec(),
            crate::model_download::DownloadStatus::Downloaded,
        );
        assert_eq!(super::downloaded_ids(&downloader), vec![speech_spec().id]);
        // カタログ外のファイルは取得の記録を持たないので数に入らない。
        assert!(
            !super::downloaded_ids(&downloader).contains(&"left-over.bin"),
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
