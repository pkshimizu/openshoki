//! 検証つきモデルダウンロードの共有基盤（whisper / 議事録要約 LLM などで使う）。
//!
//! モデルはバイナリに埋め込まず（数百 MB〜GB の肥大化を避ける）、初回利用時に HTTPS で
//! 取得して OS 標準のデータディレクトリへ保存し、以後は再利用する（「内蔵」の実現方式）。
//!
//! ダウンロードは既知の SHA-256 で検証し、一時ファイル（プロセス固有名）→リネームで原子的に
//! 配置する（破損・部分ダウンロードをモデルとして残さない）。通信は受信のみで、音声などの
//! 機微データは一切送信しない（`docs/CONTEXT.md` のオンデバイス方針はそのまま）。
//!
//! UI 起点（設定画面での選択）とワーカー起点（`ensure_model`）が同じモデルを同時に要求しても、
//! 状態マップの check-and-set で **二重ダウンロードしない**（先着がダウンロードし、後続は
//! 完了を待つ）。状態マップはモデル ID をキーにするので、種別の違うモデル（whisper と LLM）が
//! 同じ `ModelDownloader` を共有しても互いに干渉しない。
//!
//! モデル種別ごとのカタログ（`ModelSpec` の配列）は各モジュールが持つ
//! （whisper なら `crate::whisper_model::CATALOG`）。このモジュールは「1 つの `ModelSpec` を
//! 取ってきて置く」ことと、**カタログに対する種別非依存の解決・検査**（`catalog_index` と
//! `catalog_checks`）、そして**置いたものの列挙と削除**（`installed_models` /
//! `ModelDownloader::delete`。#117）を担う。種別固有の中身（どのモデルを載せるか）には触らない。
//! カタログ集合の正は `REGISTERED_CATALOGS`（種別・カタログ・既定 ID の登録簿）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use sha2::{Digest, Sha256};

/// ダウンロードできるモデル 1 つの定義。カタログを持つ側（`whisper_model` 等）が
/// `&'static` の配列として宣言する。
///
/// URL と SHA-256 は配布元（HuggingFace の LFS メタデータ等）から取る。モデルを追加・
/// 差し替えるときは **URL と SHA-256 を必ずペアで**更新する。
///
/// **`url` / `sha256` は必ずソースコード上の定数にする**。設定ファイルやネットワーク応答など
/// 実行時の値から `ModelSpec` を組み立ててはいけない。取得 API が `&'static ModelSpec` を
/// 要求しているのはそのためで（`const` / `static` 宣言でしか作れない）、取得内容の信頼は
/// この `sha256` のピン留めだけに依存している。
///
/// **カタログに載せられるのは、単一ファイルで・認証なしに取得できる配布物だけ**。
/// 分割された gguf（`*-00001-of-000NN.gguf`）や、ライセンス同意が要る gated repo は
/// この構造では表現できない（対応するなら基盤側の拡張が要る）。モデル選定の段階で避けること。
///
/// 種別**固有**の表示項目（context 長・量子化ラベル等）が要るときは、このフィールドを
/// 増やさずカタログ側で包む（`struct LlmEntry { spec: ModelSpec, context_tokens: u32 }`）。
/// ここに置くのは種別をまたいで共通のものだけにする。
#[derive(Debug)]
pub struct ModelSpec {
    /// ログに出す種別の**表示名**（例: `Whisper speech`）。どちらのダウンロードかをログで
    /// 見分けるために使う（一覧の 2 行目はカタログの `description` を出すので、ここは出さない）。
    ///
    /// **破壊的操作の判定キーにしない**（文言を調整した瞬間に判定が変わる）。種別の識別は
    /// `ModelKind`（登録簿 `REGISTERED_CATALOGS` が持つ）で行う。
    pub kind: &'static str,
    /// 設定に保存する識別子。ダウンロード状態マップのキーも兼ねるため、
    /// **種別をまたいで一意**にすること。
    pub id: &'static str,
    /// 設定画面での表示名。
    pub display_name: &'static str,
    /// 精度・速度などの説明（設定画面の表示用）。
    pub description: &'static str,
    /// 正確なファイルサイズ（バイト）。進捗の分母と受信上限の基準に使う。
    pub size_bytes: u64,
    /// データディレクトリ配下の保存ファイル名。全種別を同じ `models/` へ混ぜて置くので、
    /// **種別をまたいで一意**にすること。衝突すると `ensure_model` の存在確認が他種別の
    /// ファイルを掴み、**検証を経ずに別モデルを「取得済み」として返す**（SHA-256 の検証は
    /// ダウンロード時にしか走らない）。パス要素を含まない素のファイル名にすること。
    pub filename: &'static str,
    /// 取得元 URL。
    pub url: &'static str,
    /// 公式 SHA-256。改ざん・破損の検知に使う。
    pub sha256: &'static str,
}

/// バイト数を設定画面向けの概数（`74 MB` / `1.5 GB`）にする。
pub fn format_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bytes = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", bytes / GB)
    } else {
        format!("{:.0} MB", bytes / MB)
    }
}

/// モデルの取得状況。設定画面の表示と二重ダウンロード防止に使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadStatus {
    /// 未取得（ダウンロードもしていない）。
    NotDownloaded,
    /// ダウンロード中（`received` / `total` バイト。キュー待ちは無く即開始される）。
    Downloading { received: u64, total: u64 },
    /// 取得済み（ディスクにファイルが在る。**存在確認のみで、既存ファイルの再検証はしない**。
    /// 検証済みが保証されるのは、このプロセスがこの起動で置いたものに限る）。
    Downloaded,
    /// 直近のダウンロードが失敗した（理由つき。メモリのみで、再試行でクリアされる）。
    Failed(String),
}

/// 取得の結末。**打ち切りは失敗ではない**ので、`Err` に畳まず分けて返す（状態行とログで
/// 「途中でやめた」と「失敗した」を混同させないため。#124）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DownloadOutcome {
    /// 最後まで受信し、SHA-256 の検証にも通った。
    Completed,
    /// 打ち切りフラグが立ったので受信ループを抜けた（受信済みのぶんは捨てる）。
    Cancelled,
}

/// ダウンローダが 1 つのロックで守る状態。**「見てから決める」操作を 1 回のロックに畳む**ため
/// 3 つのマップを 1 つの Mutex にまとめてある（`docs/rules/coding-conventions.md`）。
/// 別々のロックにすると、状態を見てから打ち切りフラグを立てるまでの間に取得が終わりうる。
#[derive(Debug, Default)]
struct State {
    /// モデル ID → 取得状況。エントリが無いモデルはディスクの有無で判定する
    /// （`Downloaded` / `NotDownloaded` は必ずしもマップに載らない）。
    status: HashMap<&'static str, DownloadStatus>,
    /// 取得中のモデル ID → 打ち切りフラグ。担当スレッドが受信ループで見て、立っていたら抜ける。
    /// エントリが在るのは担当を引き受けている間だけ（掃除するのは `DownloadGuard::record`。
    /// `status` が `Downloading` を抜けるのと**同じロック**で消す）。
    cancels: HashMap<&'static str, Arc<AtomicBool>>,
    /// 逐次ワーカーが**いま完了を必要としている**モデル ID → その本数。ここに載っている間は
    /// 打ち切らない（`cancel_download` の doc）。`ensure_model` の呼び出し区間で増減する。
    required: HashMap<&'static str, usize>,
    /// モデル ID → 打ち切りを要求した回数（世代）。`cancels` のフラグは**いまの担当のもの**
    /// なので、担当が交代すると新しいフラグに置き換わり、その隙間に来た要求が消える。世代は
    /// 担当をまたいで残るので、待っているスレッドが「自分が頼まれた後に打ち切りが来たか」を
    /// 判定できる（`acquire_and_transfer`）。
    cancel_epoch: HashMap<String, u64>,
}

impl State {
    /// そのモデルの取得に打ち切りが要求されているか。`status` が `Downloading` でも、これが
    /// 真なら「もう止まる取得」なので、取得中として数えない（`request_download` の早期 return と
    /// `in_flight_remaining_bytes` の両方がここを見る）。
    fn is_cancelling(&self, id: &str) -> bool {
        self.cancels
            .get(id)
            .is_some_and(|cancel| cancel.load(Ordering::Relaxed))
    }

    /// いままでに打ち切りを要求された回数。記録が無ければ 0。
    fn cancel_epoch_of(&self, id: &str) -> u64 {
        self.cancel_epoch.get(id).copied().unwrap_or(0)
    }
}

/// UI 起点の要求（`request_download`）でスレッドを立てるか。**純粋関数**にしてあるので、
/// 判定だけをテストできる（実際に立てる側は副作用を持つので叩けない）。
///
/// `recorded` は状態マップの記録（無ければ `None`）、`cancelling` は打ち切り要求済みか、
/// `on_disk` は記録が無いときのディスク上の有無。**網羅 match** にしてあるので、状態を足したら
/// 立てるかどうかを決めるまでコンパイルが通らない。
fn should_start_download(
    recorded: Option<&DownloadStatus>,
    cancelling: bool,
    on_disk: bool,
) -> bool {
    match recorded {
        Some(DownloadStatus::Downloaded) => false,
        // **打ち切り待ちは「取得中」に数えない**（#124）。数えると、A を落としている最中に
        // B → A と選び直したときに「A の担当はもう止まる／新しい担当は立たない」の隙間に落ちて、
        // 選んだモデルが誰にも取得されないまま終わる。立てておけば `acquire_and_transfer` の
        // 待ちループが前の担当の撤退を待って引き継ぐ（結局止まらなかったならその完了を拾う）。
        Some(DownloadStatus::Downloading { .. }) => cancelling,
        Some(DownloadStatus::NotDownloaded) | Some(DownloadStatus::Failed(_)) => true,
        None => !on_disk,
    }
}

/// モデルのダウンロードと状態を管理するハンドル。`Clone` で共有し、UI（設定画面）と
/// ワーカーの両方から同じ状態を参照・更新する。
#[derive(Clone)]
pub struct ModelDownloader {
    state: Arc<Mutex<State>>,
}

impl Default for ModelDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelDownloader {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    /// 表示用の現在状況。マップにあればそれを、無ければディスクの有無で判定する。
    /// ディスク判定で取得済みと分かったらマップへ記録し、以後の照会（設定画面の 100ms
    /// ポーリング）が毎回 stat を打たないようにする。表示はメモリ状態を優先するため、
    /// 取得後にファイルを外部で消しても表示は Downloaded のまま（実際の利用時は
    /// `ensure_model` がディスクを再確認するので機能は壊れない）。
    pub fn status_of(&self, spec: &'static ModelSpec) -> DownloadStatus {
        let mut state = self.lock();
        if let Some(current) = state.status.get(spec.id) {
            return current.clone();
        }
        match model_path(spec) {
            Some(path) if path.is_file() => {
                state.status.insert(spec.id, DownloadStatus::Downloaded);
                DownloadStatus::Downloaded
            }
            _ => DownloadStatus::NotDownloaded,
        }
    }

    /// 記録済みの取得状況を ID で引く（記録が無ければ `None`）。
    ///
    /// `status_of` と違い**ディスクを見ない**（モデル管理 UI（#138）はディスク走査の結果を
    /// 持っているので存在確認は不要）。進捗と失敗も要るので状態をそのまま返す
    /// （`&'static ModelSpec` を用意せずに引けるようにしてある: 一覧の行はカタログ外もありうる）。
    pub fn recorded_status(&self, id: &str) -> Option<DownloadStatus> {
        self.lock().status.get(id).cloned()
    }

    /// UI 起点: 未取得（または直近失敗）ならバックグラウンドスレッドでダウンロードを開始する。
    /// 取得済み・ダウンロード中ならスレッドを立てずに戻る（DL 中の完了待ちは `ensure_model` を
    /// 呼ぶ利用側だけが行えばよい）。結果は状態マップとログに残る。
    ///
    /// **同時ダウンロード数の上限は持たない**（#120 で判断。以下がその根拠）。whisper（最大
    /// 2.9GB）と要約 LLM（最大 4.4GB）の 2 種別があり、どちらも UI 起点（設定画面での選択）と
    /// ワーカー起点（`ensure_model`）を持つため**並走しうる**（推論を直列化する
    /// `crate::inference_slot` は、待たせても意味が無いダウンロードを対象にしていない）。
    /// それでも上限を入れないのは:
    ///
    /// - 直列化しても**受信の総バイトは変わらない**。帯域は共有されるだけなので「両方そろう
    ///   時刻」はほぼ同じで、早まるのは先頭の 1 本だけ。取得順を意図どおりにできるわけでも
    ///   ない（直列化は先着順で、優先度は持たない）。ワーカー起点同士なら「文字起こし →
    ///   その結果を入力にする要約」の順に要求されるので順序はもともと自然だが、UI 起点で
    ///   要約 LLM を先に落とし始めた直後に録音を止めれば、どちらの実装でも whisper は後回しになる。
    /// - 種別をまたいで直列化すると、**逐次ワーカーが無関係な取得を待つ**ことになる。
    ///   文字起こしのワーカーが要約 LLM の 4.4GB を待って数十分止まるのは、帯域を分け合う
    ///   不利より体感の害が大きい（`crate::inference_slot` がダウンロードを対象外にしたのと
    ///   同じ判断）。
    /// - 待ちを増やすと「担当が進まないまま状態が `Downloading` で残る」ときの影響範囲が広がる
    ///   （同一モデルの待ちにさえ上限つきタイムアウトが必要になっている。`acquire_and_transfer`）。
    ///
    /// 並走で本当に困るのはディスクなので、そちらは埋め尽くしを**上限ではなく事前確認**で防ぐ:
    /// 受信サイズの上限（`max_download_bytes`）と、開始前の空き容量確認
    /// （`insufficient_space_reason_for_dir`）。
    /// 確認に落ちた取得は**待たせずに失敗させる**（この doc の 2 点目のとおり、待たせる害を
    /// 避けるため）。文字起こし中なら当該セッションのジョブが失敗し、次のジョブ・設定画面での
    /// 再選択で再試行される（自動リトライは無い）。
    ///
    /// **入口は 3 つ**（設定画面の選択・ワーカーの `ensure_model`・モデル管理ウィンドウの
    /// 「Download」。#138 で 3 つ目が増えた）。管理ウィンドウからはカタログ全件を個別に始められる
    /// ので、続けて押せば同時に何本でも走る——上限を持たない判断は変えていないが、まとめて始めると
    /// **空き容量の事前確認で全部が落ちる**ことがある（各スレッドが他の在庫の残量を必要量へ
    /// 加算するため）。落ちた取得は状態と行の文言に理由が出るので、順に始め直せる。
    ///
    /// #124 までは**同種別で別モデルを選び直しても先の取得が止まらず**、最悪 3 本以上・10GB 級の
    /// 同時受信になりえた。それは `cancel_download` で解消したが、上限を設けない判断は変えて
    /// いない: 上の 3 点の根拠はどれも同時本数ではなく**待たせる害**についてのもので、打ち切りが
    /// 入っても変わらないため。打ち切りは並走を減らす方向にしか効かない（走っている取得を
    /// 止めるだけで、新しく待たせはしない）。
    pub fn request_download(&self, spec: &'static ModelSpec) {
        let Some(since_epoch) = self.begin_ui_request(spec) else {
            return;
        };
        let downloader = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("model-download-{}", spec.id))
            .spawn(move || {
                // check-and-set・進捗更新・結果記録まで `acquire_and_download` が行う。
                // **`ensure_model` は通らない**: あちらは「ワーカーがいま必要としている」印を
                // 立てるので、UI 起点の先行取得まで打ち切れなくなってしまう（`cancel_download`）。
                match downloader.acquire_and_download(spec, since_epoch) {
                    Ok(DownloadOutcome::Completed) => {}
                    // 打ち切りは失敗ではないので `eprintln!` にしない（ユーザーが選び直した
                    // 結果として起きる正常な経路）。
                    Ok(DownloadOutcome::Cancelled) => {
                        println!(
                            "Cancelled the {} model download because another model was selected",
                            spec.kind
                        );
                    }
                    Err(err) => {
                        eprintln!("Skipping the model download because it failed: {err}");
                    }
                }
            });
        if let Err(err) = spawned {
            eprintln!("Skipping the model download because the thread failed to start: {err}");
        }
    }

    /// UI 起点の要求を受け付ける。スレッドを立てるなら、その要求が握る**打ち切り世代**を返す。
    ///
    /// 判定・世代の捕捉・ディスクで見つけたときの記録を**1 回のロック**で行うのが要点
    /// （`docs/rules/coding-conventions.md` の「見てから決めるは 1 回のロックに畳む」）:
    ///
    /// - 判定と `is_cancelling` を分けると、その隙間で担当が `DownloadGuard::record` を通り、
    ///   「状態は `Downloading`・フラグはもう無い」という**実在しない組み合わせ**で判定する。
    /// - 世代の捕捉を分けると、その隙間に来た打ち切りの**後**の世代を握ってしまい、
    ///   `acquire_and_transfer` の比較をすり抜けて「捨てたはずのモデルを落とし直す」。
    ///
    /// `request_download` から切り出してあるのは、この繋ぎをテストで固定するため
    /// （spawn を含んだままだとテストから叩けない。`docs/rules/testing.md`）。
    fn begin_ui_request(&self, spec: &'static ModelSpec) -> Option<u64> {
        let mut state = self.lock();
        let recorded = state.status.get(spec.id).cloned();
        let cancelling = state.is_cancelling(spec.id);
        // 記録が無いときだけディスクを見る（`status_of` と同じく、見つけたら記録して以後の
        // 照会が stat を打たないようにする）。
        let on_disk = recorded.is_none() && model_path(spec).is_some_and(|path| path.is_file());
        if on_disk {
            state.status.insert(spec.id, DownloadStatus::Downloaded);
        }
        should_start_download(recorded.as_ref(), cancelling, on_disk)
            .then(|| state.cancel_epoch_of(spec.id))
    }

    /// 走っているそのモデルの取得に**打ち切りを要求する**。要求できたら（取得中で、ワーカーも
    /// 必要としていなければ）`true`。
    ///
    /// **`true` は「止まった」ではない**: やるのはフラグを立てることだけで、担当スレッドが気づく
    /// のは次のチャンクを読む手前。既に最後のチャンクを読み終えていれば、そのまま完走して
    /// 取得済みになる（それで困らない——欲しかったファイルが手に入っただけ）。
    ///
    /// **使うのは「選び直しでその取得が不要になった」ときだけ**（`windows::models::select_model`）。
    /// 打ち切ると受信済みのぶんは捨てられ、再開はできない（部分ファイルは `PartFile` の Drop が
    /// 消す。レジュームの仕組みは持たない）ので、不要になったと言い切れる場面に限る。
    ///
    /// **ワーカーが必要としているモデルは打ち切らない**（`State::required` に載っている）。
    /// 文字起こし／要約のジョブは投入時点のモデル ID を握っているので、設定を変えても
    /// **そのジョブが要るのは古いモデル**。ここで止めると `ensure_model` の待ちが解けた瞬間に
    /// 同じものを取り直すか、ジョブが失敗するだけで、誰も得をしない。
    ///
    /// 対象は呼び出し側が渡す「直前に選んでいたモデル」1 つだけで、他のモデルは巻き込まない
    /// （種別の全モデルを止める形にすると、管理ウィンドウの「Download」で別のモデルを明示的に
    /// 落としている最中に選び直しただけで、それが消える）。**ただし、選択中のモデルを管理
    /// ウィンドウから明示的に落としている最中に別モデルへ切り替えた場合は打ち切られる**
    /// （その取得はまさに「直前に選んでいたモデル」なので区別できない）。
    ///
    /// 打ち切りの結果は `DownloadOutcome::Cancelled` として担当スレッドが受け取り、状態は
    /// `NotDownloaded` へ戻る（**`Failed` にはしない**。失敗と区別できるようにするため。
    /// 対応表の正は `DownloadGuard::finish`）。
    pub fn cancel_download(&self, id: &str) -> bool {
        // 戻り値は情報用（テストと調査のため）。本番の呼び出し側（`windows::models::select_model`）は見ない。
        let mut state = self.lock();
        if state.required.contains_key(id) {
            return false;
        }
        // `required` の確認とフラグ設定を 1 回のロックで行う（放すと、その隙間に `RequiredMark`
        // が立って**ワーカーが必要としているモデルを打ち切って**しまう）。ロックが止められるのは
        // マップの一貫性だけで、担当スレッドの進行は止まらない——だから下の doc のとおり、
        // `true` は「要求できた」であって「止まった」ではない。
        // 世代は**担当の有無に関わらず**進める。担当の交代を待っているスレッドは、この世代を
        // 見て「自分が頼まれた後に打ち切りが来た」と分かり、取得を始めずに降りる。
        // **`required` の確認より後**に置くこと: 先に進めると、ワーカーが待っている最中に UI で
        // 別モデルを選んだだけで世代が動き、印があるのに `acquire_and_transfer` が降りて
        // ジョブが失敗する（`ensure_model` の「印が有効な区間では起きえない」が崩れる）。
        *state.cancel_epoch.entry(id.to_owned()).or_insert(0) += 1;
        match state.cancels.get(id) {
            Some(cancel) => {
                cancel.store(true, Ordering::Relaxed);
                true
            }
            None => false,
        }
    }

    /// モデルのパスを返す。未取得ならダウンロードして配置する（成功するか、失敗・タイムアウトで
    /// `Err` になるまで返さない）。他スレッドが同じモデルをダウンロード中なら、その完了を待って
    /// 結果を使う（二重取得しない。先行が失敗した場合は後続が担当を引き継いで再取得する）。
    ///
    /// **この呼び出し区間の間、このモデルは選び直しで打ち切られない**（`RequiredMark`。ジョブは
    /// 投入時点のモデル ID を握っているので、設定を変えてもそのジョブが要るのは古いモデル。
    /// 理由の詳細は `cancel_download` の doc）。
    ///
    /// ダウンロードは分オーダーかかりうるため、メインスレッドから呼ばない
    /// （ワーカー／`request_download` のスレッドから呼ぶ）。
    pub fn ensure_model(
        &self,
        spec: &'static ModelSpec,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = model_path(spec).ok_or("Cannot determine the data directory")?;
        match self.ensure_and_transfer(spec, &path, |cancel| {
            download_model(spec, &path, self, cancel)
        })? {
            DownloadOutcome::Completed => Ok(path),
            // 印が有効な区間では**起きえない**（`cancel_download` は同じロックの下で `required` を
            // 先に見る）。防御的な分岐で、万一起きたらジョブは失敗として縮退する
            // （検証していないファイルを黙って使わない）。
            DownloadOutcome::Cancelled => Err("the model download was cancelled".into()),
        }
    }

    /// ワーカー起点の取得。`acquire_and_transfer` に**「いま必要」の印を足しただけ**で、これが
    /// UI 起点（`request_download`）との唯一の違い。印がある間は選び直しで打ち切られない
    /// （理由は `cancel_download` の doc）。
    ///
    /// `ensure_model` から切り出してあるのは、この違いをテストで固定するため（`ensure_model` は
    /// 実ネットワークを叩くのでテストから呼べない。`docs/rules/testing.md`）。
    fn ensure_and_transfer(
        &self,
        spec: &'static ModelSpec,
        path: &Path,
        transfer: impl FnOnce(&AtomicBool) -> Result<DownloadOutcome, Box<dyn std::error::Error>>,
    ) -> Result<DownloadOutcome, Box<dyn std::error::Error>> {
        let _needed = RequiredMark::new(self, spec.id);
        // 印を立ててから世代を読む（読んでから立てるまでの間に打ち切りが来ると、それを
        // 見落としたまま担当を引き受けてしまう）。
        let since_epoch = self.lock().cancel_epoch_of(spec.id);
        self.acquire_and_transfer(spec, path, since_epoch, transfer)
    }

    /// UI 起点の取得。担当を引き受けて（または他スレッドの完了を待って）モデルを配置する。
    fn acquire_and_download(
        &self,
        spec: &'static ModelSpec,
        since_epoch: u64,
    ) -> Result<DownloadOutcome, Box<dyn std::error::Error>> {
        let path = model_path(spec).ok_or("Cannot determine the data directory")?;
        self.acquire_and_transfer(spec, &path, since_epoch, |cancel| {
            download_model(spec, &path, self, cancel)
        })
    }

    /// `acquire_and_download` の本体。**転送そのものを引数で受ける**ので、ネットワークにも
    /// ディスクにも触れないフェイクを渡して、担当の引き受け・打ち切りフラグの登録と掃除・
    /// 結末の記録という配線だけをテストできる（`docs/rules/testing.md` のミューテーション観点。
    /// ここを引数化しないと、フラグの登録を消しても既存テストが全部緑のまま通ってしまう）。
    ///
    /// `since_epoch` は**要求を出した時点**の打ち切り世代（`State::cancel_epoch`）。担当を
    /// 引き受ける直前に読み直して変わっていたら、待っている間に打ち切りが来たということなので
    /// 取得を始めずに降りる。担当が交代するとフラグ（`State::cancels`）は新しいものに
    /// 置き換わるため、フラグだけでは担当交代をまたいだ要求が消えてしまう。
    fn acquire_and_transfer(
        &self,
        spec: &'static ModelSpec,
        path: &Path,
        since_epoch: u64,
        transfer: impl FnOnce(&AtomicBool) -> Result<DownloadOutcome, Box<dyn std::error::Error>>,
    ) -> Result<DownloadOutcome, Box<dyn std::error::Error>> {
        // 待機の上限。担当スレッドが**進まない**（無応答接続、Drop が走らない `abort` 等）と
        // 状態が Downloading のまま残り、上限なしでは待機側（逐次のワーカー等）が永久に固まって
        // 以後のジョブが黙って止まる。DL 全体のタイムアウトより長い上限で打ち切り、エラーとして
        // 返す（次のジョブ・次の選択で再試行される）。unwind するパニックで担当が消えた場合は
        // `DownloadGuard` が `Failed` へ倒すので、待機側は次の周回で担当を引き継げる。
        let wait_deadline = std::time::Instant::now() + WAIT_FOR_OTHER_DOWNLOAD_TIMEOUT;
        let cancel = loop {
            {
                let mut state = self.lock();
                match state.status.get(spec.id) {
                    // 他スレッドがダウンロード中。ロックを放して完了を待つ。
                    Some(DownloadStatus::Downloading { .. }) => {}
                    _ => {
                        if path.is_file() {
                            state.status.insert(spec.id, DownloadStatus::Downloaded);
                            return Ok(DownloadOutcome::Completed);
                        }
                        // 待っている間に打ち切りが来ていたら、担当を引き受けずに降りる。
                        if state.cancel_epoch_of(spec.id) != since_epoch {
                            return Ok(DownloadOutcome::Cancelled);
                        }
                        // 自分がダウンロード担当になる（check-and-set。ロック内で遷移させ、
                        // 同じモデルを同時に見た 2 スレッドが両方ダウンロードするのを防ぐ）。
                        // 打ち切りフラグも**同じロックの中で**登録する（放してから登録すると、その
                        // 隙間に来た `cancel_download` が「取得中なのに打ち切れない」で落ちる）。
                        state.status.insert(
                            spec.id,
                            DownloadStatus::Downloading {
                                received: 0,
                                total: spec.size_bytes,
                            },
                        );
                        let cancel = Arc::new(AtomicBool::new(false));
                        state.cancels.insert(spec.id, Arc::clone(&cancel));
                        break cancel;
                    }
                }
            }
            if std::time::Instant::now() >= wait_deadline {
                return Err(
                    "timed out waiting for another download of the same model to finish".into(),
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        };

        // 担当を引き受けた区間は番人で囲み、結末の記録もそれに任せる（理由と後始末の契約は
        // `DownloadGuard` の doc）。この組（囲む・必ず `finish` に記録させる）を崩すと、取得中の
        // パニックで状態が `Downloading` のまま残る。打ち切りフラグの掃除も番人に任せる
        // （パニックで抜けても、次の取得が古いフラグを掴まない）。
        DownloadGuard::new(self, spec.id).finish(transfer(&cancel))
    }

    /// 走っている**他の**ダウンロードの残りバイト合計（`except_model_id` 自身は除く。
    /// `acquire_and_transfer` は取得開始時に自分を `Downloading` へ遷移させるので、除かないと
    /// 自分のサイズを二重に要求してしまう）。必要量へ加算する理由は
    /// `insufficient_space_reason` の doc。
    ///
    /// 既に書けたぶん（`received`）は空き容量に反映済みなので残りだけを数える。進捗の更新は
    /// `PROGRESS_STEP_BYTES` 刻みで遅れるが、その遅れは残りを多めに見る＝安全側に転ぶ。
    ///
    /// **打ち切りを要求済みの取得は数えない**（#124）。選び直しは「前のを打ち切る → 新しいのを
    /// 頼む」の順で走るが、前の担当が実際に止まるのは次のチャンクの手前なので、数えたままだと
    /// 新しい取得が**要らない容量まで要求して容量不足で落ちる**（自動リトライは無い）。止まる
    /// ことが決まっている取得はディスクを増やす方向に働かないので、外して安全側は崩れない。
    fn in_flight_remaining_bytes(&self, except_model_id: &str) -> u64 {
        let state = self.lock();
        state
            .status
            .iter()
            .filter_map(|(id, status)| match status {
                DownloadStatus::Downloading { received, total }
                    if *id != except_model_id && !state.is_cancelling(id) =>
                {
                    Some(total.saturating_sub(*received))
                }
                _ => None,
            })
            .sum()
    }

    /// そのモデルの取得に打ち切りが要求されているか（担当がまだ気づいていない区間で真）。
    #[cfg(test)]
    fn is_cancelling(&self, id: &str) -> bool {
        self.lock().is_cancelling(id)
    }

    /// テスト用: 状態を直接注入する（表示ロジックをディスク・ネットワーク非依存で検証する）。
    #[cfg(test)]
    pub(crate) fn set_status_for_test(&self, spec: &'static ModelSpec, status: DownloadStatus) {
        self.lock().status.insert(spec.id, status);
    }

    /// 取得済みのモデルファイルを**完全削除**する（ゴミ箱へは入れない。#117）。
    ///
    /// ゴミ箱へ送らないのは、カタログに URL と SHA-256 があって再取得できるため。ディスクを
    /// 空けるのが目的なのに、ゴミ箱に数 GB 残っては本末転倒になる（録音の削除とは扱いを変える。
    /// `docs/rules/security.md`）。
    ///
    /// **状態マップのエントリも一緒に消す**。`status_of` はメモリの状態を優先するので、消さないと
    /// 設定画面の 100ms ポーリングが「削除したのに Downloaded」と表示し続ける。
    ///
    /// 取得中（`Downloading`）のモデルは削除せずエラーにする: 完了時の rename でファイルが復活し、
    /// 「削除したのに残っている」ことになる。UI でも無効化するが、UI の状態は最後の tick 時点の
    /// もの（tick とクリックの間に取得が始まりうる）なので、**ここが最後の砦**。判定・削除・エントリ掃除を
    /// 1 回のロックの中で行う: ダウンロードの開始も同じロックを取る（`acquire_and_transfer` の
    /// check-and-set）ため、この間に取得が始まることはない
    /// （`docs/rules/coding-conventions.md` の「見てから決めるは 1 回のロックに畳む」）。
    pub fn delete(&self, model: &InstalledModel) -> Result<(), Box<dyn std::error::Error>> {
        let dir = models_dir().ok_or("Cannot determine the data directory")?;
        self.delete_in(&dir, model)
    }

    /// `delete` の本体（基点ディレクトリを引数で受け、テストから呼べるようにする）。
    fn delete_in(
        &self,
        dir: &Path,
        model: &InstalledModel,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !is_plain_filename(&model.filename) {
            // UI 経由で来る値なので、`models/` の外へ出る形を弾く（`../` や絶対パス）。
            return Err("the model file name is not a plain file name".into());
        }
        let path = dir.join(&model.filename);
        let mut state = self.lock();
        // 一覧を作ってから押されるまでの間に差し替えられていないかを、消す直前に見る
        // （`models/` がリンクへ、対象がリンクへ）。列挙側のガードだけでは、この窓が閉じない。
        if !dir.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            return Err("the models folder is not a directory".into());
        }
        if !path.symlink_metadata().is_ok_and(|meta| meta.is_file()) {
            return Err("the model file is not a regular file".into());
        }
        if let Some(id) = model.catalog_id
            && let Some(DownloadStatus::Downloading { .. }) = state.status.get(id)
        {
            return Err("the model is being downloaded".into());
        }
        // フルパスはエラーへ載せない（ユーザー名を含む。`docs/rules/security.md`）。呼び出し側が
        // 表示・ログに使うので、種別だけを返す。
        std::fs::remove_file(&path)
            .map_err(|err| format!("{} ({})", err.kind(), model.filename))?;
        if let Some(id) = model.catalog_id {
            state.status.remove(id);
        }
        Ok(())
    }

    /// 状態マップのガードを取る。poison（ロック保持中のパニック）でも状態表示・DL 管理を
    /// 止めないため、ガードを取り出して続行する（`docs/rules/error-handling.md`）。
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 取得を引き受けたスレッドが**結果を必ず状態マップへ残す**ための番人。`finish` が結末
/// （完了・打ち切り・失敗）を記録して解除し、記録せずに drop されたら（取得中のパニック等）
/// `Failed` へ倒す。**打ち切りフラグ（`State::cancels`）の掃除も同じ経路（`record`）で行う**
/// ので、状態が `Downloading` を抜けるのとフラグが消えるのが必ず同じロックの中になる。
///
/// **ここが `Downloading` の後始末の正**。これが無いと状態が `Downloading` のまま残り、
/// (1) `request_download` が早期 return してそのモデルを二度と取得しない、
/// (2) `insufficient_space_reason` の並走ぶんに恒久的に加算されて**他のモデルの取得まで
/// 空き容量不足として断られる**、という詰まりがプロセスの寿命いっぱい続く（`acquire_and_transfer` の
/// 待ちのタイムアウトは待ち側を打ち切るだけで、状態は畳まない）。
///
/// 畳めないのは Drop が走らない終わり方（`abort`・プロセスの強制終了）だけで、そのときは
/// 状態マップ自体も消えるので詰まりは残らない。
#[must_use = "the guard must be bound (and finished) or the status stays Downloading on panic"]
struct DownloadGuard<'a> {
    downloader: &'a ModelDownloader,
    id: &'static str,
    recorded: bool,
}

impl<'a> DownloadGuard<'a> {
    fn new(downloader: &'a ModelDownloader, id: &'static str) -> Self {
        Self {
            downloader,
            id,
            recorded: false,
        }
    }

    /// 取得の結末を状態マップへ記録し、番人を解除して結末をそのまま返す。
    ///
    /// 結末 → 状態の対応表は**この網羅 match が正**（`DownloadOutcome` にバリアントを足したら、
    /// ここで状態を決めるまでコンパイルが通らない）。とくに **打ち切りは `Failed` にしない**:
    /// ユーザーが選び直した結果なので、失敗として見せると状態行で「途中でやめた」と
    /// 「落とせなかった」が混ざる（#124）。受信ぶんは捨てて部分ファイルも消えているので、
    /// 未取得へ戻すのが実態と合う。
    ///
    /// 記録と解除をここにまとめてあるので、呼び出し側は順序を気にしなくてよい（`Drop` の中で
    /// 状態マップのロックを取るため、外でロックを保持したまま解除すると自己デッドロックする。
    /// その順序制約はコメントではなく実装で守る形にしてある）。
    fn finish(
        mut self,
        result: Result<DownloadOutcome, Box<dyn std::error::Error>>,
    ) -> Result<DownloadOutcome, Box<dyn std::error::Error>> {
        // 記録する値はロックを取る前に作る（`to_string` がパニックしても番人が畳める）。
        let status = match &result {
            Ok(DownloadOutcome::Completed) => DownloadStatus::Downloaded,
            Ok(DownloadOutcome::Cancelled) => DownloadStatus::NotDownloaded,
            Err(err) => DownloadStatus::Failed(err.to_string()),
        };
        self.record(status);
        result
    }

    /// 結果を記録して番人を解除する。打ち切りフラグの掃除も同じロックで行う
    /// （`Downloading` を抜けたのにフラグだけ残ると、次の取得が立った直後に打ち切られる）。
    fn record(&mut self, status: DownloadStatus) {
        let mut state = self.downloader.lock();
        state.status.insert(self.id, status);
        state.cancels.remove(self.id);
        self.recorded = true;
    }
}

impl Drop for DownloadGuard<'_> {
    fn drop(&mut self) {
        if self.recorded {
            return;
        }
        // 失敗として記録し、次の要求（次のジョブ・設定画面での再選択）で再試行できる状態に戻す。
        self.record(DownloadStatus::Failed(
            "the download stopped unexpectedly".to_owned(),
        ));
    }
}

/// `ensure_model` の呼び出し区間に「このモデルはいまワーカーが必要としている」印を立てる番人。
/// 印がある間は `cancel_download` が打ち切らない（理由はそちらの doc）。
///
/// 同じモデルを複数のワーカーが待つことがあるので本数で持ち、drop で減らす。パニックで抜けても
/// 減るので、印が残って**そのモデルを二度と打ち切れなくなる**ことはない。
struct RequiredMark<'a> {
    downloader: &'a ModelDownloader,
    id: &'static str,
}

impl<'a> RequiredMark<'a> {
    fn new(downloader: &'a ModelDownloader, id: &'static str) -> Self {
        *downloader.lock().required.entry(id).or_insert(0) += 1;
        Self { downloader, id }
    }
}

impl Drop for RequiredMark<'_> {
    fn drop(&mut self) {
        let mut state = self.downloader.lock();
        match state.required.get_mut(self.id) {
            Some(count) if *count > 1 => *count -= 1,
            // 最後の 1 本ならエントリごと消す（0 を残すと `contains_key` が真のままになる）。
            Some(_) => {
                state.required.remove(self.id);
            }
            // 増減が非対称になっている（この番人以外が `required` を触った）。印が張り付くと
            // そのモデルを二度と打ち切れなくなるので、黙って通さず debug ビルドで気づけるようにする。
            None => debug_assert!(false, "RequiredMark dropped without a matching mark"),
        }
    }
}

/// ダウンロード時の読み書きバッファサイズ。
const DOWNLOAD_BUF_SIZE: usize = 64 * 1024;

/// 進捗を共有状態へ反映する間隔（受信バイト）。毎読み込みでロックを取らないための間引き。
const PROGRESS_STEP_BYTES: u64 = 1024 * 1024;

/// 接続確立・応答ヘッダ受信のタイムアウト。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// ボディ受信全体のタイムアウト。低速回線でもカタログ中の最大モデル（現状は要約 LLM の
/// 約 4.4GB）を受け切れる長さにしつつ、
/// 無応答の接続（half-open 等）で呼び出しスレッドが恒久にハングしないようにする。
/// 超過時は失敗し、次の要求で再試行する。
const RECV_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120 * 60);

/// 他スレッドのダウンロード完了を待つ上限。DL 全体のタイムアウト（接続＋受信）より長くし、
/// 正常な待機を途中で打ち切らない。
const WAIT_FOR_OTHER_DOWNLOAD_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(130 * 60);

/// 取り残されたモデルの一時ファイルと見なす、最終更新からの経過時間
/// （`sweep_orphaned_part_files`。録音側は `recordings::STALE_SESSION_PART_AGE` で別に決める）。
///
/// **受信全体のタイムアウト（`RECV_BODY_TIMEOUT`）より長く**取る。あれは無通信の上限ではなく
/// **ボディ受信全体の期限**（2 時間）で、一時ファイルはヘッダ受信後に作られるので、生きた取得の
/// 一時ファイルの寿命はその 2 時間が上限（超えたら失敗して自分で片付ける）。3 時間はそこに
/// 1 時間の余裕を足した値（時計のずれ・mtime の粒度・受信後の検証と rename にかかる時間ぶん）。
/// 走っている取得を消さない保証そのものは mtime が更新され続けることで足りる
/// （`atomic_replace::sweep_orphaned_parts` の doc）。
const STALE_MODEL_PART_AGE: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);

/// 不足ぶんを表示するときの単位。この単位へ切り上げて出す（`insufficient_space_reason`）。
const REPORTED_SHORTFALL_UNIT_BYTES: u64 = 1024 * 1024;

/// モデルの取得後に残しておく空き容量（`insufficient_space_reason`）。数 GB のモデルで
/// ディスクを 0 まで埋めると、録音の書き出しも OS 自体も道連れにする。128 kbps・2 音源で
/// 約 115 MB/時（`src/recorder.rs` の `BITRATE`）なので、約 4 時間ぶんの録音を書き出せる余白。
const DISK_HEADROOM_BYTES: u64 = 512 * 1024 * 1024;

/// モデルの保存先（`<データディレクトリ>/models/<ファイル名>`）。種別を問わず同じディレクトリに
/// 置く（ファイル名がモデルを一意に表すため。混在しても衝突しない）。
///
/// `filename` が単一のファイル名でなければ `None`（`models/` の外へ書かせない）。カタログは
/// ソース上の定数なので通常は起こらないが、`pub` フィールドなので種別が増えたときの書き損じを
/// ここで止める（静的な検査は `catalog_checks::assert_valid`）。
fn model_path(spec: &ModelSpec) -> Option<PathBuf> {
    if !is_plain_filename(spec.filename) {
        eprintln!(
            "Skipping the model because its filename is not a plain file name: {}",
            spec.filename
        );
        return None;
    }
    models_dir().map(|dir| dir.join(spec.filename))
}

/// モデルの保存ディレクトリ（`<データディレクトリ>/models`）。
fn models_dir() -> Option<PathBuf> {
    crate::config::data_dir().map(|dir| dir.join("models"))
}

/// 強制終了などで残ったモデルの一時ファイルを回収する（起動時に 1 回呼ぶ）。判定と限界は
/// `atomic_replace::sweep_orphaned_parts` の doc。モデルは数 GB あり、保存先はユーザーが辿らない
/// データディレクトリ配下なので、残ると気づかれないまま容量を食う。
///
/// 録音側の一時ファイルは**ここでは**掃除しない（#134。範囲と時期は
/// `recordings::spawn_session_part_sweep` の doc）。
pub fn sweep_orphaned_part_files() {
    let Some(dir) = models_dir() else {
        return;
    };
    // 宛先名は絞らない（`models/` はアプリ専有で、カタログが増えると宛先名も増える）。
    crate::atomic_replace::sweep_orphaned_parts(
        &dir,
        std::time::SystemTime::now(),
        STALE_MODEL_PART_AGE,
        crate::atomic_replace::PartScope::AnyDest,
    );
}

/// モデルの種別。**破壊的操作（削除）の判定キー**で、表示用の `ModelSpec::kind` とは別物
/// （文言を調整した瞬間に判定が変わる形にしないため。`docs/rules/coding-conventions.md`）。
///
/// 種別を足したら、この enum と `REGISTERED_CATALOGS` に 1 つ足す。使う側は網羅 match で
/// 受けるので（`windows::models::kind_is_busy`）、足した種別の扱いを書き忘れるとコンパイルが通らない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    /// 文字起こし（whisper）。
    Speech,
    /// 議事録要約（LLM）。
    Summary,
}

/// 全カタログの登録簿（種別・カタログ・その既定 ID）。**種別を足したらここに 1 行足す**。
///
/// これが**カタログ集合の唯一の正**: 取得済みモデルの列挙（`installed_models`）と、横断の
/// 一意性検査（`catalog_checks`）が同じ登録簿を読む。呼び出し側で別に並べると、種別を足した人が
/// 片方だけ更新して**その種別のモデルが「カタログ外」に落ちる**（削除の安全弁が効かなくなり、
/// 「再取得できない」という嘘の警告も出る）。
pub(crate) const REGISTERED_CATALOGS: &[(ModelKind, &[ModelSpec], &str)] = &[
    (
        ModelKind::Speech,
        crate::whisper_model::CATALOG,
        crate::whisper_model::DEFAULT_MODEL_ID,
    ),
    (
        ModelKind::Summary,
        crate::summary_model::CATALOG,
        crate::summary_model::DEFAULT_MODEL_ID,
    ),
];

/// `models/` に置かれているモデルファイル 1 件（モデル一覧ウィンドウの 1 行。#117）。
///
/// 一覧の骨格はカタログの登録簿（`REGISTERED_CATALOGS`）で、**ディスクの走査はその行に実体と
/// 実サイズを与える**役目（#138。#117 では走査結果そのものが一覧だった）。カタログを差し替えた
/// 後の旧ファイルのように、**カタログに無いファイルも列挙する**（消せないと掃除できない）。
/// その場合は種別・表示名・ID が `None` になる。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledModel {
    /// `models/` 直下のファイル名。削除の対象を指すキー（`ModelDownloader::delete`）。
    pub filename: String,
    /// **実ファイルの長さ**。カタログの `size_bytes` ではない（途中で壊れたファイルの実サイズを
    /// 見せたいため）。
    pub size_bytes: u64,
    /// カタログが引けたときの**種別**（削除ガードの判定キー。`ModelKind`）。カタログ外は `None`。
    pub kind: Option<ModelKind>,
    /// カタログが引けたときのモデル ID。状態マップのキーなので、削除時のエントリ掃除に使う。
    /// 表示名・説明はカタログ（`ModelSpec`）から引くので、ここには持たない。
    pub catalog_id: Option<&'static str>,
}

/// `models/` にあるモデルを列挙する（サイズの大きい順）。データディレクトリが決まらない・
/// まだ 1 つも取得していない場合は空。
///
/// 表示名・種別は登録簿（`REGISTERED_CATALOGS`）から引く。**種別非依存**なので、種別が増えても
/// 登録簿に 1 行足すだけで一覧に並ぶ。
///
/// 走査そのものに失敗したら `Err`（呼び出し側が「1 つも無い」と区別して表示するため。空一覧に
/// 畳むと、権限エラーで読めないだけのときに「まだ何も無い」と嘘を言う）。
///
/// **取得中のモデルはここには出ない**: 受信中の中身は一時ファイル（`*.part.<pid>`）で、まだ
/// モデルではないため。そのぶん合計使用量は受信中のバイトを含まない（取得中であること自体は
/// 状態マップから分かるので、モデル管理 UI は行の状態として進捗を出す）。取り残された一時
/// ファイルの回収は `sweep_orphaned_part_files` が持つので、ここでは扱わない。
pub fn installed_models() -> std::io::Result<Vec<InstalledModel>> {
    let Some(dir) = models_dir() else {
        return Ok(Vec::new());
    };
    let catalogs: Vec<(ModelKind, &'static [ModelSpec])> = REGISTERED_CATALOGS
        .iter()
        .map(|(kind, catalog, _)| (*kind, *catalog))
        .collect();
    installed_models_in(&dir, &catalogs)
}

/// `installed_models` の本体（走査するディレクトリとカタログを引数で受け、テストから呼べる
/// ようにする）。
///
/// 走査は `dir` **直下の通常ファイルだけ**にする: ディレクトリとシンボリックリンクは対象外
/// （`entry.metadata()` はリンクを辿らないので、リンク自身の属性で判断する）。`dir` 自身が
/// リンクのときも辿らない——一覧の行はそのまま**完全削除**の対象になるので、`models/` を
/// 外部ボリュームへのリンクに差し替えられていたら、その先の無関係なファイルを消せてしまう
/// （`atomic_replace::sweep_orphaned_parts` と同じガード）。書きかけの一時ファイル
/// （`*.part.<pid>`）も除く: 成果物ではないし、取得中のものを消させると完了時の rename が
/// 失敗する。取り残しの回収は `sweep_orphaned_part_files` が持つ。
fn installed_models_in(
    dir: &Path,
    catalogs: &[(ModelKind, &'static [ModelSpec])],
) -> std::io::Result<Vec<InstalledModel>> {
    // リンクを辿らずにディレクトリであることを確かめる（未作成なら「1 つも無い」と同じ扱い）。
    match dir.symlink_metadata() {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            eprintln!("Skipping the model scan because the models path is not a directory");
            return Ok(Vec::new());
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    }

    let mut models: Vec<InstalledModel> = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        // 1 件読めなくても残りは並べる（握りつぶさずログに残す。`docs/rules/error-handling.md`）。
        // ログに出すのはファイル名だけ（フルパスはユーザー名を含む。`docs/rules/security.md`）。
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!("Skipping a model entry because it could not be read: {err}");
                continue;
            }
        };
        let name = entry.file_name();
        let Some(filename) = name.to_str() else {
            // 非 UTF-8 の名前は扱わない（カタログのファイル名は UTF-8）。消す対象にもしない。
            eprintln!("Skipping a model file because its name is not valid UTF-8");
            continue;
        };
        if crate::atomic_replace::is_part_file(Path::new(filename)) {
            continue;
        }
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                // 一覧にも合計にも出ないと、実際の使用量が表示より大きくなる（この一覧の目的が
                // 崩れる）ので、消えたことが分かるようログに残す。
                eprintln!(
                    "Skipping a model file because its metadata could not be read (file: {filename}, reason: {err})"
                );
                continue;
            }
        };
        if !metadata.is_file() {
            continue;
        }
        let found = catalogs
            .iter()
            .flat_map(|(kind, catalog)| catalog.iter().map(move |spec| (*kind, spec)))
            .find(|(_, spec)| spec.filename == filename);
        models.push(InstalledModel {
            filename: filename.to_owned(),
            size_bytes: metadata.len(),
            kind: found.map(|(kind, _)| kind),
            catalog_id: found.map(|(_, spec)| spec.id),
        });
    }
    // 大きい順（一覧の目的が「何が容量を食っているか」なので、効くものから見せる）。
    // 同サイズはファイル名で安定させる。
    models.sort_by(|a, b| {
        b.size_bytes
            .cmp(&a.size_bytes)
            .then_with(|| a.filename.cmp(&b.filename))
    });
    Ok(models)
}

/// 設定のモデルパス上書き（`whisper_model_path` / `summary_model_path`）が `models/` 直下を
/// 指しているなら、そのファイル名。
///
/// 上書きは `models/` の**外**を指すのが普通だが、直下を指すこともできる。その場合、上書き先だと
/// 分からないと (1) 実行中のジョブが読んでいるファイルを削除できてしまい、(2)「消しても再取得
/// される」という誤った案内を出してしまう（上書き中は `ensure_model` を通らない）。判定は
/// `windows::models::row_facts`。
///
/// 名前まで還元しておくのは、**行ごと・tick ごとに `canonicalize` を叩かない**ため（一覧は
/// 10Hz で組み直す）。解決はディスクを走査するタイミングで 1 回だけ行い、行の判定はファイル名の
/// 比較にする（`windows::models::OverrideFiles`）。
pub fn override_filename(override_path: Option<&Path>) -> Option<String> {
    let dir = models_dir()?;
    override_filename_in(&dir, override_path)
}

/// `override_filename` の本体（基点ディレクトリを引数で受け、テストから呼べるようにする）。
fn override_filename_in(dir: &Path, override_path: Option<&Path>) -> Option<String> {
    let path = override_path?;
    let name = path.file_name()?.to_str()?;
    is_override_of_in(dir, name, Some(path)).then(|| name.to_owned())
}

/// `is_override_of` の本体（基点ディレクトリを引数で受け、テストから呼べるようにする）。
fn is_override_of_in(dir: &Path, filename: &str, override_path: Option<&Path>) -> bool {
    let Some(override_path) = override_path else {
        return false;
    };
    let installed = dir.join(filename);
    if override_path == installed {
        return true;
    }
    // `..` やリンクを挟んだ書き方でも一致を見る（どちらかが解決できないなら一致とみなさない。
    // 素の比較は上で外れているので、ここで false にして「消してよい」側へは倒さない）。
    match (
        std::fs::canonicalize(override_path),
        std::fs::canonicalize(&installed),
    ) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

/// パス要素を持たない素のファイル名か（`/` や `..`、絶対パスを弾く）。
fn is_plain_filename(filename: &str) -> bool {
    let mut components = Path::new(filename).components();
    matches!(components.next(), Some(std::path::Component::Normal(_)))
        && components.next().is_none()
}

/// 受信サイズの上限。既知のモデルサイズ＋1 割の余裕。配信元の故障・想定外の応答で
/// ディスクを埋め尽くさないための保険（正常時は届かない）。
fn max_download_bytes(spec: &ModelSpec) -> u64 {
    spec.size_bytes + spec.size_bytes / 10
}

/// 保存先の空きを読んで足りなければ拒否の理由（ユーザーに出す文言）を返す
/// （判定そのものは `insufficient_space_reason`）。**空きが読めない場合は続行**する（`None`）:
/// 読めないだけで機能を落とすのは過剰で、受信サイズの上限と ENOSPC での失敗が最後の砦として
/// 残る（`docs/rules/error-handling.md` の縮退）。
///
/// `dir` はモデルの保存先ディレクトリ、`headroom` は残す余白（どちらもテスト容易性のため
/// 引数で受ける。`write_verified` の `max_bytes` と同じ）。
fn insufficient_space_reason_for_dir(
    dir: &Path,
    spec: &ModelSpec,
    in_flight: u64,
    headroom: u64,
) -> Option<String> {
    match fs2::available_space(dir) {
        Ok(available) => insufficient_space_reason(spec, available, in_flight, headroom),
        // `fs2` のエラーは `statvfs` の OS エラー（か NUL 混入）だけでパスを含まないため、
        // そのままログへ出せる（`docs/rules/security.md`）。
        Err(err) => {
            eprintln!(
                "Continuing without the free-disk-space check because the available space could not be read: {err}"
            );
            None
        }
    }
}

/// 空き容量が足りているかを判定し、足りなければ「どれだけ空ければよいか」を返す。
///
/// **必要量は「受信の上限（`max_download_bytes`）＋ 残す余白 ＋ 並走している他の取得の残り」**
/// （空きから差し引くのではなく、必要量へ加算する形で表している）。並走ぶんを見込むのは、同時
/// ダウンロードに上限を持たない設計（`request_download` の doc）で各自が同じ空きを当てにすると、
/// 合計でディスクが溢れるため。並走ぶんは相手の受信上限（＋1 割）ではなく残りバイトだけを見る
/// （過剰に断らないための割り切り）。一時ファイルは同じボリュームへ書いて rename するので、
/// モデル 1 本ぶんのピークは 2 倍にならない。
///
/// 文言は合算値ではなく**不足ぶん**を出す。合算値だと「4.4GB のモデルに 8GB 要る」と読めて、
/// 何をすればよいか分からない。並走ぶんだけで不足が埋まるなら「待てば解ける」ことも添える
/// （埋まらないなら待っても足りないので添えない）。
fn insufficient_space_reason(
    spec: &ModelSpec,
    available: u64,
    in_flight: u64,
    headroom: u64,
) -> Option<String> {
    let needed = max_download_bytes(spec)
        .saturating_add(headroom)
        .saturating_add(in_flight);
    if available >= needed {
        return None;
    }
    let shortfall = needed - available;
    // 表示は MB 単位へ**切り上げ**る。`format_size` は端数を丸めるので、そのまま出すと
    // 「0 MB 空けろ」（1MB 未満）や、言われたぶんを空けても足りない値（切り下げ側）になり、
    // 指示として成立しない。多めに言うぶんには実害が無い。
    // 掛け戻しは飽和させる（見積もりが `u64::MAX` へ飽和した異常系で、切り上げの掛け算が
    // オーバーフローしてパニックしないように）。
    let reported = shortfall
        .div_ceil(REPORTED_SHORTFALL_UNIT_BYTES)
        .saturating_mul(REPORTED_SHORTFALL_UNIT_BYTES);
    // 並走ぶんが終わるだけで不足が埋まるなら、待つのも手だと添える（埋まらないなら待っても
    // 足りないので添えない）。
    Some(if in_flight >= shortfall {
        format!(
            "not enough free disk space for {} — free up about {} or wait for the downloads in progress to finish",
            spec.display_name,
            format_size(reported)
        )
    } else {
        format!(
            "not enough free disk space for {} — free up about {} and try again",
            spec.display_name,
            format_size(reported)
        )
    })
}

/// モデルをダウンロードして `dest` へ原子的に配置し、進捗を状態マップへ反映する。
///
/// `cancel` が立ったら受信をやめ、**`dest` を作らずに** `DownloadOutcome::Cancelled` を返す
/// （書きかけの一時ファイルは `PartFile` の Drop が消す）。
fn download_model(
    spec: &'static ModelSpec,
    dest: &Path,
    downloader: &ModelDownloader,
    cancel: &AtomicBool,
) -> Result<DownloadOutcome, Box<dyn std::error::Error>> {
    // 接続を張る前に 1 回見る。ここから最初のチャンクが届くまでは打ち切りを見ない区間
    // （ディレクトリ作成・空き容量確認・DNS・TLS で秒〜数十秒）なので、既に不要と分かって
    // いる取得で GET を張りに行かない。
    if cancel.load(Ordering::Relaxed) {
        return Ok(DownloadOutcome::Cancelled);
    }
    if let Some(parent) = dest.parent() {
        // モデルは公開配布物で機微データではないため、権限は OS 既定でよい
        // （録音データの 0700/0600 とは扱いが異なる）。
        std::fs::create_dir_all(parent)?;
        // 受信を始める前に空きを見る（判定は `insufficient_space_reason`）。埋めてから ENOSPC で
        // 落ちると、帯域を数 GB 無駄にしたうえにディスクが枯渇した状態で失敗する（録音の
        // 書き出しも巻き込む）。
        let in_flight = downloader.in_flight_remaining_bytes(spec.id);
        if let Some(reason) =
            insufficient_space_reason_for_dir(parent, spec, in_flight, DISK_HEADROOM_BYTES)
        {
            return Err(reason.into());
        }
    }
    println!(
        "Downloading the {} model {} (about {})",
        spec.kind,
        spec.display_name,
        format_size(spec.size_bytes)
    );

    // タイムアウトを明示する。ureq の既定は無期限で、無応答の接続（half-open 等）に当たると
    // 呼び出しスレッド（ワーカー等）が恒久にハングしてしまう。
    // TLS 検証は ureq 既定（rustls + 同梱 Mozilla ルート）。OS のトラストストアとは独立だが、
    // 接続先は固定 URL 群で SHA-256 ピンによる完全性検証も重ねているため、これで足りる。
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .timeout_recv_response(Some(CONNECT_TIMEOUT))
        .timeout_recv_body(Some(RECV_BODY_TIMEOUT))
        .build()
        .into();
    let mut response = agent.get(spec.url).call()?;
    // 進捗の分母は応答の Content-Length を優先し、無ければカタログの既知サイズを使う。
    let total = response.body().content_length().unwrap_or(spec.size_bytes);
    let reader = response.body_mut().as_reader();

    // 一時ファイルへ書き、検証に通ってから本来の名前へ rename する（原子的）。途中で失敗しても
    // 壊れた/部分的なファイルがモデルとして残らない。一時ファイルの命名・後始末（失敗・パニック）は
    // `crate::atomic_replace::PartFile` が持つ（プロセス固有名にする理由もそちらの doc）。
    let part = crate::atomic_replace::PartFile::for_dest(dest)
        .ok_or("the model path does not end in a file name")?;
    let on_progress = |received: u64| {
        downloader
            .lock()
            .status
            .insert(spec.id, DownloadStatus::Downloading { received, total });
    };
    let outcome = verify_and_place(
        part,
        reader,
        spec.sha256,
        max_download_bytes(spec),
        on_progress,
        || cancel.load(Ordering::Relaxed),
    )?;
    if outcome == DownloadOutcome::Completed {
        println!("Downloaded the {} model {}", spec.kind, spec.display_name);
    }
    Ok(outcome)
}

/// 受信して検証し、**検証に通ったときだけ** `part` を本来の名前へ配置する。
///
/// `download_model` から切り出してあるのは、**「打ち切ったのに配置してしまう」ミューテーションを
/// テストで捕まえる**ため（ここを `download_model` に埋めたままだと、検査するのに実ネットワークが
/// 要る）。打ち切りでは `commit()` に進まないので `dest` は生まれず、書きかけの一時ファイルは
/// `part` の drop が消す＝**検証していないバイト列がモデルとして残らない**
/// （`docs/rules/security.md`）。
fn verify_and_place(
    part: crate::atomic_replace::PartFile,
    reader: impl Read,
    expected_sha256: &str,
    max_bytes: u64,
    on_progress: impl FnMut(u64),
    is_cancelled: impl FnMut() -> bool,
) -> Result<DownloadOutcome, Box<dyn std::error::Error>> {
    // **網羅 match** にしてあるので、結末を足した人は配置してよいかを必ず決めることになる。
    match write_verified(
        reader,
        part.path(),
        expected_sha256,
        max_bytes,
        on_progress,
        is_cancelled,
    )? {
        DownloadOutcome::Cancelled => return Ok(DownloadOutcome::Cancelled),
        DownloadOutcome::Completed => {}
    }
    part.commit()?;
    Ok(DownloadOutcome::Completed)
}

/// `reader` の内容を `dest` へ書き出しつつ SHA-256 を計算し、`expected_sha256` と一致しなければ
/// エラーを返す（ファイルは書かれたまま残る。後始末は呼び出し側の `PartFile` が持つ）。
/// `max_bytes` を超える受信は**失敗させる**（想定外の応答でディスクを埋めない保険。テスト容易性の
/// ため引数で受ける）。`on_progress` には累積受信バイトを `PROGRESS_STEP_BYTES` ごとに渡す。
///
/// `is_cancelled` が真を返したら受信をやめて `DownloadOutcome::Cancelled` を返す（**`Err` ではない**。
/// 打ち切りは失敗ではないので、呼び出し側が状態行とログで区別できるようにする。#124）。
/// この場合ハッシュの検証は行わない——途中までのバイト列に意味は無いため。呼び出し側は
/// **`Cancelled` を `commit()` へ進ませてはいけない**（`download_model` の網羅 match）。
fn write_verified(
    mut reader: impl Read,
    dest: &Path,
    expected_sha256: &str,
    max_bytes: u64,
    mut on_progress: impl FnMut(u64),
    mut is_cancelled: impl FnMut() -> bool,
) -> Result<DownloadOutcome, Box<dyn std::error::Error>> {
    let file = std::fs::File::create(dest)?;
    let mut writer = std::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; DOWNLOAD_BUF_SIZE];
    let mut written: u64 = 0;
    let mut last_reported: u64 = 0;
    loop {
        // 打ち切りの確認は**読む前**に置く（`read` は次のチャンクが届くまで戻らないので、後ろに
        // 置くと最後の 1 回ぶん待たされる）。粒度は 1 チャンク＝最大 64 KiB なので、データが
        // 流れている間の体感は「押した直後」（ms 級）。**ただし無通信になると `read` が
        // `RECV_BODY_TIMEOUT`（2 時間）まで戻らないので、その間は気づけない**——バイト粒度の
        // 保証であって時間の保証ではない。
        if is_cancelled() {
            return Ok(DownloadOutcome::Cancelled);
        }
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        written += read as u64;
        if written > max_bytes {
            return Err(format!("download exceeded the size limit ({max_bytes} bytes)").into());
        }
        if written - last_reported >= PROGRESS_STEP_BYTES {
            last_reported = written;
            on_progress(written);
        }
        hasher.update(&buf[..read]);
        writer.write_all(&buf[..read])?;
    }
    writer.flush()?;

    let digest = format!("{:x}", hasher.finalize());
    if digest != expected_sha256 {
        return Err(format!("checksum mismatch (expected {expected_sha256}, got {digest})").into());
    }
    Ok(DownloadOutcome::Completed)
}

/// カタログの静的な健全性チェック。**正はここ 1 箇所**にして、種別が増えても同じ検査が
/// 効くようにする（各カタログのテストから呼ぶ）。
#[cfg(test)]
pub(crate) mod catalog_checks {
    use super::{ModelSpec, is_plain_filename};

    /// 種別を問わず満たすべき条件。カタログ**内**の重複と、各エントリの形、そして
    /// **既定 ID がカタログに在ること**を見る。最後のひとつは `catalog_index` と各カタログの
    /// `default_spec` が持つ `expect`（「既定 ID は必ずカタログに在る」）の根拠で、種別が
    /// 増えても検査し忘れないようここに置く。
    pub(crate) fn assert_valid(catalog: &[ModelSpec], default_id: &str) {
        assert!(
            catalog.iter().any(|spec| spec.id == default_id),
            "the default id {default_id} is not in the catalog"
        );
        for (i, spec) in catalog.iter().enumerate() {
            assert!(
                catalog.iter().skip(i + 1).all(|other| other.id != spec.id),
                "duplicate id {}",
                spec.id
            );
            assert!(
                catalog
                    .iter()
                    .skip(i + 1)
                    .all(|other| other.filename != spec.filename),
                "duplicate filename {}",
                spec.filename
            );
            assert_eq!(spec.sha256.len(), 64, "bad sha256 for {}", spec.id);
            assert!(
                spec.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "bad sha256 for {}",
                spec.id
            );
            assert!(spec.size_bytes > 0, "zero size for {}", spec.id);
            // `models/` の外へ書かせない（`model_path` が実行時にも弾くが、ここで先に落とす）。
            assert!(
                is_plain_filename(spec.filename),
                "filename must be a plain file name: {}",
                spec.filename
            );
        }
    }

    /// 登録簿のカタログすべてが健全で、ID とファイル名は種別をまたいで一意
    /// （状態マップのキーと保存先が種別で混ざらないように）。
    ///
    /// 登録簿は本番と同じもの（`super::REGISTERED_CATALOGS`）を読む。`assert_valid` もここで
    /// 回すので、**カタログを足す側は登録簿へ 1 行足すだけでよい**（各カタログのテストからも
    /// 呼べるが、呼び忘れてもここで捕まる）。
    #[test]
    fn registered_catalogs_are_valid_and_globally_unique() {
        for (_, catalog, default_id) in super::REGISTERED_CATALOGS {
            assert_valid(catalog, default_id);
        }

        // 同じ種別を 2 回登録していないこと（コピペで `(Speech, summary_model::CATALOG)` と
        // 登録すると、要約 LLM が文字起こしの busy フラグで守られる＝要約中に消せてしまう）。
        for (i, (kind, _, _)) in super::REGISTERED_CATALOGS.iter().enumerate() {
            for (other, _, _) in super::REGISTERED_CATALOGS.iter().skip(i + 1) {
                assert_ne!(kind, other, "{kind:?} is registered twice");
            }
        }

        let specs: Vec<&ModelSpec> = super::REGISTERED_CATALOGS
            .iter()
            .flat_map(|(_, catalog, _)| catalog.iter())
            .collect();
        for (i, spec) in specs.iter().enumerate() {
            for other in specs.iter().skip(i + 1) {
                assert_ne!(spec.id, other.id, "id {} is used by two catalogs", spec.id);
                assert_ne!(
                    spec.filename, other.filename,
                    "filename {} is used by two catalogs",
                    spec.filename
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 既知データの SHA-256（`echo -n hello | sha256sum` 相当）。
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// テスト用の緩い上限。
    const TEST_MAX: u64 = u64::MAX;

    /// 打ち切りを見ない `is_cancelled`（打ち切りを扱わないテストの既定）。
    fn never_cancelled() -> bool {
        false
    }

    /// ダウンロード経路を通らないダミー定義で使う SHA-256。値に意味は無く、形式
    /// （64 桁の 16 進）だけを満たす。
    const UNUSED_SHA256: &str = "00000000000000000000000000000000000000000000000000000000000000ff";

    /// **カタログに無い**架空の定義 2 つ。基盤が種別を知らずに動くことを、実カタログや
    /// ディスクの状態に依存せず確かめるために使う（ネットワークへは出ない経路のみ）。
    static FAKE_LLM_MODEL: ModelSpec = ModelSpec {
        kind: "Test summary",
        id: "test-llm",
        display_name: "Test LLM",
        description: "used only by tests",
        size_bytes: 1_024,
        filename: "test-llm.gguf",
        url: "https://example.invalid/test-llm.gguf",
        sha256: UNUSED_SHA256,
    };

    static FAKE_SPEECH_MODEL: ModelSpec = ModelSpec {
        kind: "Test speech",
        id: "test-speech",
        display_name: "Test Speech",
        description: "used only by tests",
        size_bytes: 2_048,
        filename: "test-speech.bin",
        url: "https://example.invalid/test-speech.bin",
        sha256: UNUSED_SHA256,
    };

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shoki-model-{}-{name}", std::process::id()))
    }

    /// 一覧に出す `InstalledModel` を組む素材（テスト用のディレクトリを作って返す）。
    fn models_fixture(tag: &str) -> PathBuf {
        let dir = temp_path(tag);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the fixture dir should succeed");
        dir
    }

    fn fake_catalogs() -> Vec<(ModelKind, &'static [ModelSpec])> {
        // カタログは 2 つ渡す（種別非依存であること＝どちらのカタログからも引けることを見る）。
        static LLM: &[ModelSpec] = std::slice::from_ref(&FAKE_LLM_MODEL);
        static SPEECH: &[ModelSpec] = std::slice::from_ref(&FAKE_SPEECH_MODEL);
        vec![(ModelKind::Summary, LLM), (ModelKind::Speech, SPEECH)]
    }

    /// 一覧はディスクを正にして、カタログは表示名・種別の解決にだけ使う。カタログ外のファイルも
    /// 並べる（掃除できるようにするため）。並びはサイズの大きい順。
    #[test]
    fn installed_models_resolves_the_catalog_and_keeps_unknown_files() {
        let dir = models_fixture("installed");
        std::fs::write(dir.join(FAKE_SPEECH_MODEL.filename), vec![b'x'; 30])
            .expect("writing the fixture should succeed");
        std::fs::write(dir.join(FAKE_LLM_MODEL.filename), vec![b'x'; 10])
            .expect("writing the fixture should succeed");
        std::fs::write(dir.join("left-over.bin"), vec![b'x'; 20])
            .expect("writing the fixture should succeed");

        let models =
            installed_models_in(&dir, &fake_catalogs()).expect("the fixture dir is readable");
        let names: Vec<&str> = models.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(
            names,
            vec![
                FAKE_SPEECH_MODEL.filename,
                "left-over.bin",
                FAKE_LLM_MODEL.filename
            ],
            "the biggest file comes first"
        );
        assert_eq!(models[0].size_bytes, 30, "the size is the real file length");
        assert_eq!(models[0].kind, Some(ModelKind::Speech));
        assert_eq!(models[0].catalog_id, Some(FAKE_SPEECH_MODEL.id));
        // カタログ外はファイル名とサイズだけが分かる。
        assert_eq!(models[1].kind, None);
        assert_eq!(models[1].catalog_id, None);
        // 2 つ目のカタログからも引ける（種別非依存。種別も登録簿の値が入る）。
        assert_eq!(models[2].catalog_id, Some(FAKE_LLM_MODEL.id));
        assert_eq!(models[2].kind, Some(ModelKind::Summary));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一覧に出すのは直下の通常ファイルだけ。ディレクトリ・シンボリックリンク・書きかけの
    /// 一時ファイルは出さない（消す対象にしない）。
    #[test]
    fn installed_models_lists_only_plain_files() {
        let dir = models_fixture("installed-kinds");
        std::fs::write(dir.join("real.bin"), b"x").expect("writing the fixture should succeed");
        std::fs::create_dir(dir.join("subdir.bin")).expect("creating the subdir should succeed");
        std::fs::write(dir.join("subdir.bin").join("inner.bin"), b"x")
            .expect("writing the fixture should succeed");
        // 取得中・強制終了で残った一時ファイル（回収は `sweep_orphaned_part_files`）。
        std::fs::write(dir.join("real.bin.part.123"), b"x")
            .expect("writing the fixture should succeed");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("real.bin"), dir.join("linked.bin"))
            .expect("creating the symlink should succeed");

        let models =
            installed_models_in(&dir, &fake_catalogs()).expect("the fixture dir is readable");
        let names: Vec<&str> = models.iter().map(|m| m.filename.as_str()).collect();
        assert_eq!(names, vec!["real.bin"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 走査できないディレクトリ（未作成）でも落ちず、空一覧になる。
    #[test]
    fn installed_models_degrades_when_the_folder_is_missing() {
        let dir = temp_path("installed-missing");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            installed_models_in(&dir, &fake_catalogs())
                .expect("the fixture dir is readable")
                .is_empty()
        );
    }

    /// **走査するディレクトリ自身がシンボリックリンク**なら辿らない。一覧の行はそのまま完全削除の
    /// 対象になるので、`models/` をリンクに差し替えられていたらリンク先の無関係なファイルを
    /// 消せてしまう（エントリ単位のリンク除外では防げない）。
    #[test]
    #[cfg(unix)]
    fn installed_models_does_not_follow_a_symlinked_folder() {
        let real = models_fixture("installed-linked-target");
        std::fs::write(real.join("victim.bin"), b"x").expect("writing the fixture should succeed");
        let root = models_fixture("installed-linked");
        let link = root.join("models");
        std::os::unix::fs::symlink(&real, &link).expect("creating the symlink should succeed");

        let listed = installed_models_in(&link, &fake_catalogs())
            .expect("a symlinked folder is reported as empty, not as an error");
        assert!(
            listed.is_empty(),
            "files behind a symlinked models folder must not be listed"
        );

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&real);
    }

    /// 実カタログ（登録簿）を通した列挙が、**登録簿の種別をそのまま写す**こと。行の種別は削除
    /// ガードの判定キーなので、写しが壊れると（`kind: None` など）そのモデルはガードの外に落ちる。
    #[test]
    fn installed_models_copies_the_kind_from_the_registry() {
        let dir = models_fixture("installed-registry");
        let catalogs: Vec<(ModelKind, &'static [ModelSpec])> = REGISTERED_CATALOGS
            .iter()
            .map(|(kind, catalog, _)| (*kind, *catalog))
            .collect();
        // 登録簿の全 spec のファイル名で 1 バイトのファイルを置く。
        for (_, catalog, _) in REGISTERED_CATALOGS {
            for spec in catalog.iter() {
                std::fs::write(dir.join(spec.filename), b"x")
                    .expect("writing the fixture should succeed");
            }
        }

        let listed = installed_models_in(&dir, &catalogs).expect("the fixture dir is readable");
        for (kind, catalog, _) in REGISTERED_CATALOGS {
            for spec in catalog.iter() {
                let row = listed
                    .iter()
                    .find(|model| model.filename == spec.filename)
                    .unwrap_or_else(|| panic!("{} should be listed", spec.id));
                assert_eq!(row.kind, Some(*kind), "wrong kind for {}", spec.id);
                assert_eq!(row.catalog_id, Some(spec.id));
            }
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 削除の直前確認: **基点がディレクトリでない**（`models/` がリンクへ差し替えられた）とき、
    /// リンク先の同名ファイルを消さずに失敗する。列挙側のガード
    /// （`installed_models_does_not_follow_a_symlinked_folder`）と対称。
    #[test]
    #[cfg(unix)]
    fn delete_refuses_when_the_models_folder_is_a_symlink() {
        let real = models_fixture("delete-linked-target");
        let victim = real.join("victim.bin");
        std::fs::write(&victim, b"x").expect("writing the fixture should succeed");
        let root = models_fixture("delete-linked");
        let link = root.join("models");
        std::os::unix::fs::symlink(&real, &link).expect("creating the symlink should succeed");

        let model = InstalledModel {
            filename: "victim.bin".to_owned(),
            size_bytes: 1,
            kind: None,
            catalog_id: None,
        };
        ModelDownloader::new()
            .delete_in(&link, &model)
            .expect_err("deleting through a symlinked models folder must be refused");
        assert!(victim.exists(), "the file behind the symlink must be kept");

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&real);
    }

    /// 削除の直前確認: 対象が通常ファイルでない（ディレクトリ・シンボリックリンク）なら消さない。
    #[test]
    fn delete_refuses_targets_that_are_not_regular_files() {
        let dir = models_fixture("delete-not-a-file");
        std::fs::create_dir(dir.join("as-a-dir.bin")).expect("creating the fixture should succeed");
        let downloader = ModelDownloader::new();
        let model = |name: &str| InstalledModel {
            filename: name.to_owned(),
            size_bytes: 1,
            kind: None,
            catalog_id: None,
        };

        downloader
            .delete_in(&dir, &model("as-a-dir.bin"))
            .expect_err("a directory must not be deleted");
        assert!(dir.join("as-a-dir.bin").is_dir());

        #[cfg(unix)]
        {
            let target = dir.join("outside.bin");
            std::fs::write(&target, b"x").expect("writing the fixture should succeed");
            std::os::unix::fs::symlink(&target, dir.join("as-a-link.bin"))
                .expect("creating the symlink should succeed");
            downloader
                .delete_in(&dir, &model("as-a-link.bin"))
                .expect_err("a symlink must not be deleted");
            assert!(dir.join("as-a-link.bin").is_symlink());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 設定のモデルパス上書きがこの行を指しているかの判定。`models/` の外を指すのが普通だが、
    /// 直下を指すこともできる（そのときは削除を守る側に効く）。
    #[test]
    fn is_override_of_matches_only_this_file() {
        let dir = models_fixture("override");
        let model = InstalledModel {
            filename: "custom.gguf".to_owned(),
            size_bytes: 1,
            kind: None,
            catalog_id: None,
        };
        let installed = dir.join(&model.filename);
        std::fs::write(&installed, b"x").expect("writing the fixture should succeed");

        // 素の一致。
        assert!(is_override_of_in(&dir, &model.filename, Some(&installed)));
        // `.` / `..` を挟んだ書き方でも一致する（canonicalize で解決する）。
        let indirect = dir.join("sub").join("..").join(&model.filename);
        std::fs::create_dir_all(dir.join("sub")).expect("creating the subdir should succeed");
        assert!(is_override_of_in(&dir, &model.filename, Some(&indirect)));
        // 別のファイル・上書き無しは一致しない。
        assert!(!is_override_of_in(
            &dir,
            &model.filename,
            Some(&dir.join("other.gguf"))
        ));
        assert!(!is_override_of_in(&dir, &model.filename, None));
        // 実ファイルが無くても素の比較で一致する（canonicalize は両方失敗する）。
        let missing = InstalledModel {
            filename: "not-created.gguf".to_owned(),
            size_bytes: 1,
            kind: None,
            catalog_id: None,
        };
        assert!(is_override_of_in(
            &dir,
            &missing.filename,
            Some(&dir.join(&missing.filename))
        ));
        // `models/` の外を指す上書き（本来の使い方）は、この行とは無関係。
        let outside = temp_path("override-outside");
        std::fs::write(&outside, b"x").expect("writing the fixture should succeed");
        assert!(!is_override_of_in(&dir, &model.filename, Some(&outside)));

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 上書きパスが `models/` 直下を指すときだけ、そのファイル名を返す（行の判定を名前の比較だけで
    /// 済ませるための解決。`models/` の外を指す本来の使い方では `None`）。
    #[test]
    fn override_filename_resolves_only_inside_the_models_folder() {
        let dir = models_fixture("override-filename");
        let inside = dir.join("custom.gguf");
        std::fs::write(&inside, b"x").expect("writing the fixture should succeed");

        assert_eq!(
            override_filename_in(&dir, Some(&inside)),
            Some("custom.gguf".to_owned())
        );
        // `..` を挟んでも解決できる（`canonicalize` で一致を見る）。
        std::fs::create_dir_all(dir.join("sub")).expect("creating the subdir should succeed");
        let indirect = dir.join("sub").join("..").join("custom.gguf");
        assert_eq!(
            override_filename_in(&dir, Some(&indirect)),
            Some("custom.gguf".to_owned())
        );
        // `models/` の外を指す上書き（本来の使い方）・未設定・ファイル名で終わらないパスは `None`。
        let outside = temp_path("override-filename-outside");
        std::fs::write(&outside, b"x").expect("writing the fixture should succeed");
        assert_eq!(override_filename_in(&dir, Some(&outside)), None);
        assert_eq!(override_filename_in(&dir, None), None);
        assert_eq!(override_filename_in(&dir, Some(Path::new("/"))), None);

        let _ = std::fs::remove_file(&outside);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 削除はファイルを消し、**状態マップのエントリも消す**（消さないと設定画面の 100ms
    /// ポーリングが「削除したのに Downloaded」と表示し続ける）。
    #[test]
    fn delete_removes_the_file_and_forgets_the_status() {
        let dir = models_fixture("delete");
        let path = dir.join(FAKE_LLM_MODEL.filename);
        std::fs::write(&path, b"x").expect("writing the fixture should succeed");
        let downloader = ModelDownloader::new();
        downloader
            .lock()
            .status
            .insert(FAKE_LLM_MODEL.id, DownloadStatus::Downloaded);

        let models =
            installed_models_in(&dir, &fake_catalogs()).expect("the fixture dir is readable");
        assert_eq!(models.len(), 1);
        downloader
            .delete_in(&dir, &models[0])
            .expect("deleting an installed model should succeed");

        assert!(!path.exists(), "the model file should be gone");
        assert!(
            !downloader.lock().status.contains_key(FAKE_LLM_MODEL.id),
            "the status entry should be gone so the display falls back to the disk"
        );
        assert!(
            installed_models_in(&dir, &fake_catalogs())
                .expect("the fixture dir is readable")
                .is_empty()
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 取得中のモデルは削除しない（完了時の rename でファイルが復活し、「削除したのに残って
    /// いる」ことになる）。UI でも無効化するが、最後の砦はここ。
    #[test]
    fn delete_refuses_a_model_that_is_being_downloaded() {
        let dir = models_fixture("delete-downloading");
        let path = dir.join(FAKE_LLM_MODEL.filename);
        std::fs::write(&path, b"x").expect("writing the fixture should succeed");
        let downloader = ModelDownloader::new();
        downloader.lock().status.insert(
            FAKE_LLM_MODEL.id,
            DownloadStatus::Downloading {
                received: 0,
                total: FAKE_LLM_MODEL.size_bytes,
            },
        );

        let models =
            installed_models_in(&dir, &fake_catalogs()).expect("the fixture dir is readable");
        downloader
            .delete_in(&dir, &models[0])
            .expect_err("a model that is being downloaded must not be deleted");
        assert!(path.exists(), "the file must be left alone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `models/` の外へ出る名前は削除しない（UI 経由で来る値なので形を検証する）。
    #[test]
    fn delete_refuses_names_that_leave_the_models_folder() {
        let dir = models_fixture("delete-escape");
        let outside = dir.join("victim.bin");
        std::fs::write(&outside, b"x").expect("writing the fixture should succeed");
        let inner = dir.join("inner");
        std::fs::create_dir(&inner).expect("creating the subdir should succeed");
        let downloader = ModelDownloader::new();

        for filename in [
            "../victim.bin".to_owned(),
            outside.to_string_lossy().into_owned(),
            "inner/../victim.bin".to_owned(),
        ] {
            let model = InstalledModel {
                filename,
                size_bytes: 1,
                kind: None,
                catalog_id: None,
            };
            downloader
                .delete_in(&inner, &model)
                .expect_err("a name that leaves the folder must be refused");
        }
        assert!(outside.exists(), "the file outside must be left alone");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_size_uses_mb_and_gb() {
        assert_eq!(format_size(77_691_713), "74 MB");
        assert_eq!(format_size(487_601_967), "465 MB");
        assert_eq!(format_size(1_624_555_275), "1.5 GB");
        assert_eq!(format_size(3_095_033_483), "2.9 GB");
    }

    #[test]
    fn max_download_bytes_allows_ten_percent_over() {
        assert_eq!(max_download_bytes(&FAKE_LLM_MODEL), 1_024 + 102);
    }

    /// 空き容量の判定は「受信上限＋余白＋並走ぶんの残り」で見る（#120。上限を設けない代わりの
    /// 歯止めなので、並走ぶんを必要量へ加算しないと 2 本で溢れる）。
    #[test]
    fn insufficient_space_reason_counts_margin_and_other_downloads() {
        // 受信上限は 1,126 バイト。余白 100・並走なしなら 1,226 でちょうど足りる。
        assert!(insufficient_space_reason(&FAKE_LLM_MODEL, 1_226, 0, 100).is_none());
        assert!(insufficient_space_reason(&FAKE_LLM_MODEL, 1_225, 0, 100).is_some());
        // 並走ぶん（他の取得の残り）を足した分だけ余分に要る。
        assert!(insufficient_space_reason(&FAKE_LLM_MODEL, 1_226, 1, 100).is_some());
        assert!(insufficient_space_reason(&FAKE_LLM_MODEL, 1_227, 1, 100).is_none());
        // 見積もりが飽和しても桁が折り返らず、断る側へ転ぶ（`saturating_add`。加算で 0 付近へ
        // 回り込むと、実際には足りないのに通してディスクを溢れさせる）。
        assert!(insufficient_space_reason(&FAKE_LLM_MODEL, 1_000_000, u64::MAX, 100).is_some());
    }

    /// 文言は合算値ではなく**不足ぶん**を出す。並走ぶんだけで不足が埋まるなら「待てば解ける」も
    /// 添え、埋まらないなら添えない（待っても足りないため）。状態行に `Download failed: {reason}`
    /// として出る唯一の説明なので固定する。
    #[test]
    fn insufficient_space_reason_tells_how_much_to_free() {
        const MB: u64 = 1024 * 1024;

        // 受信上限 1,126 ＋ 余白 3MB に対して空き 1MB → 不足は 2MB 強で、MB 単位へ切り上げて 3MB。
        // 並走が無いので「空けて再試行」だけを案内する。
        let alone = insufficient_space_reason(&FAKE_LLM_MODEL, MB, 0, 3 * MB)
            .expect("less than the needed space should be refused");
        assert_eq!(
            alone,
            "not enough free disk space for Test LLM — free up about 3 MB and try again"
        );

        // 必要は 3MB＋受信上限、空きは 1MB＋受信上限 → 不足 2MB。並走ぶん 3MB に収まるので待てる。
        let waitable = insufficient_space_reason(&FAKE_LLM_MODEL, MB + 1_126, 3 * MB, 0)
            .expect("less than the needed space should be refused");
        assert_eq!(
            waitable,
            "not enough free disk space for Test LLM — free up about 2 MB or wait for the downloads in progress to finish"
        );

        // 並走ぶんがあっても、それだけでは埋まらない不足は「空けて再試行」を案内する
        // （待っても足りないのに待たせない）。
        let not_waitable = insufficient_space_reason(&FAKE_LLM_MODEL, MB, MB, 5 * MB)
            .expect("less than the needed space should be refused");
        assert!(
            not_waitable.ends_with("and try again"),
            "waiting does not help here: {not_waitable}"
        );

        // 並走ぶんと不足がちょうど同じなら、待つだけで足りる（`>=` の等号側。`>` に狭めると
        // 「待てば解ける状況で空けろと言う」表示になる）。
        let exactly_waitable = insufficient_space_reason(&FAKE_LLM_MODEL, 1_226, 1, 100)
            .expect("one byte short should be refused");
        assert!(
            exactly_waitable.ends_with("to finish"),
            "waiting alone is enough here: {exactly_waitable}"
        );

        // 1MB 未満の不足は「0 MB 空けろ」にならないよう、切り上げて 1MB と言い切る。
        let tiny = insufficient_space_reason(&FAKE_LLM_MODEL, 1_226 - 1, 0, 100)
            .expect("one byte short should be refused");
        assert_eq!(
            tiny,
            "not enough free disk space for Test LLM — free up about 1 MB and try again"
        );
    }

    /// 空きが読めないときは確認を飛ばして続行する（縮退。受信上限と ENOSPC が最後の砦）。
    #[test]
    fn insufficient_space_reason_for_dir_continues_when_the_space_cannot_be_read() {
        let missing = temp_path("no-such-dir-for-space-check");
        assert!(!missing.exists(), "the fixture path should not exist");
        // 判定に入れば必ず断る値（必要量を極端に大きく）を渡しても None ＝ 判定へ入っていない。
        assert!(
            insufficient_space_reason_for_dir(&missing, &FAKE_LLM_MODEL, u64::MAX, u64::MAX)
                .is_none()
        );
    }

    /// 並走ぶんの残りバイトは、自分以外の `Downloading` だけを数える。
    #[test]
    fn in_flight_remaining_bytes_skips_self_and_finished_downloads() {
        let downloader = ModelDownloader::new();
        downloader.set_status_for_test(
            &FAKE_SPEECH_MODEL,
            DownloadStatus::Downloading {
                received: 500,
                total: 2_048,
            },
        );
        // 自分の分は数えない（`acquire_and_transfer` が取得開始時に Downloading へ遷移させるため、
        // 数えると自分のサイズを二重に要求してしまう）。
        downloader.set_status_for_test(
            &FAKE_LLM_MODEL,
            DownloadStatus::Downloading {
                received: 0,
                total: 1_024,
            },
        );
        assert_eq!(
            downloader.in_flight_remaining_bytes(FAKE_LLM_MODEL.id),
            2_048 - 500
        );

        // 終わった取得・失敗した取得は残りバイトを持たない。
        downloader.set_status_for_test(&FAKE_SPEECH_MODEL, DownloadStatus::Downloaded);
        assert_eq!(downloader.in_flight_remaining_bytes(FAKE_LLM_MODEL.id), 0);
        downloader.set_status_for_test(&FAKE_SPEECH_MODEL, DownloadStatus::Failed("boom".into()));
        assert_eq!(downloader.in_flight_remaining_bytes(FAKE_LLM_MODEL.id), 0);

        // Content-Length が実サイズより多く来た異常時（received > total）は 0 として数える
        // （`saturating_sub`。桁の折り返しで巨大な残りにしない）。
        downloader.set_status_for_test(
            &FAKE_SPEECH_MODEL,
            DownloadStatus::Downloading {
                received: 3_000,
                total: 2_048,
            },
        );
        assert_eq!(downloader.in_flight_remaining_bytes(FAKE_LLM_MODEL.id), 0);
    }

    /// 番人の契約（`DownloadGuard` の doc）: 結果を記録せずに抜けたら `Failed` へ倒し、
    /// `finish` なら結果をそのまま記録する。
    #[test]
    fn download_guard_records_the_outcome_or_marks_failed() {
        let downloader = ModelDownloader::new();
        let downloading = DownloadStatus::Downloading {
            received: 0,
            total: 1_024,
        };

        // 記録せずに抜けた（パニック相当）。
        downloader.set_status_for_test(&FAKE_LLM_MODEL, downloading.clone());
        drop(DownloadGuard::new(&downloader, FAKE_LLM_MODEL.id));
        assert_eq!(
            downloader.status_of(&FAKE_LLM_MODEL),
            DownloadStatus::Failed("the download stopped unexpectedly".to_owned())
        );

        // 成功を記録した。
        downloader.set_status_for_test(&FAKE_LLM_MODEL, downloading.clone());
        DownloadGuard::new(&downloader, FAKE_LLM_MODEL.id)
            .finish(Ok(DownloadOutcome::Completed))
            .expect("finishing with Ok should return Ok");
        assert_eq!(
            downloader.status_of(&FAKE_LLM_MODEL),
            DownloadStatus::Downloaded
        );

        // 失敗を記録した（理由はそのまま状態行に出る）。
        downloader.set_status_for_test(&FAKE_LLM_MODEL, downloading);
        let err = DownloadGuard::new(&downloader, FAKE_LLM_MODEL.id)
            .finish(Err("boom".into()))
            .expect_err("finishing with Err should return Err");
        assert_eq!(err.to_string(), "boom");
        assert_eq!(
            downloader.status_of(&FAKE_LLM_MODEL),
            DownloadStatus::Failed("boom".to_owned())
        );
    }

    #[test]
    fn write_verified_accepts_matching_checksum() {
        let dest = temp_path("ok.bin");
        let end = write_verified(
            b"hello".as_slice(),
            &dest,
            HELLO_SHA256,
            TEST_MAX,
            |_| {},
            never_cancelled,
        )
        .expect("matching checksum should succeed");
        assert_eq!(end, DownloadOutcome::Completed);
        assert_eq!(std::fs::read(&dest).expect("readable"), b"hello");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn write_verified_rejects_checksum_mismatch_and_leaves_file() {
        let dest = temp_path("bad.bin");
        let err = write_verified(
            b"tampered".as_slice(),
            &dest,
            HELLO_SHA256,
            TEST_MAX,
            |_| {},
            never_cancelled,
        )
        .expect_err("checksum mismatch should fail");
        assert!(err.to_string().contains("checksum mismatch"));
        // doc の契約どおり、失敗してもファイルは残る（後始末は呼び出し側の責務）。
        assert!(dest.is_file(), "the partial file should remain for cleanup");
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn write_verified_handles_multi_chunk_input_and_reports_progress() {
        // 読み書きループが複数チャンクにまたがる経路と、進捗コールバックの単調増加を確認する。
        let data = vec![0xA5u8; DOWNLOAD_BUF_SIZE * 2 + 123];
        let expected = format!("{:x}", Sha256::digest(&data));
        let dest = temp_path("multi.bin");
        let mut reported: Vec<u64> = Vec::new();
        write_verified(
            data.as_slice(),
            &dest,
            &expected,
            TEST_MAX,
            |received| reported.push(received),
            never_cancelled,
        )
        .expect("matching checksum should succeed");
        assert_eq!(
            std::fs::metadata(&dest).expect("metadata").len(),
            data.len() as u64
        );
        // PROGRESS_STEP_BYTES（1MB）未満の入力では進捗は報告されないこともある。
        // 単調増加だけを確認する。
        assert!(reported.windows(2).all(|w| w[0] < w[1]));
        let _ = std::fs::remove_file(&dest);
    }

    #[test]
    fn write_verified_aborts_over_size_limit() {
        // 上限超過の入力は途中で打ち切る。実運用サイズを流すと重いので、小さな上限で
        // 打ち切り経路そのものを検証する（上限は引数化されており同じコードパス）。
        let dest = temp_path("oversize.bin");
        let limit = DOWNLOAD_BUF_SIZE as u64;
        let err = write_verified(
            std::io::Read::take(std::io::repeat(0), limit + DOWNLOAD_BUF_SIZE as u64),
            &dest,
            HELLO_SHA256,
            limit,
            |_| {},
            never_cancelled,
        )
        .expect_err("exceeding the size limit should fail");
        assert!(err.to_string().contains("size limit"));
        let _ = std::fs::remove_file(&dest);
    }

    /// 打ち切りフラグが立ったら、受信をやめて `Cancelled` を返す（`Err` にしない）。
    ///
    /// reader は `HELLO_SHA256` と**合わない**中身にしてあり、長さも打ち切り予定より十分長い。
    /// そのため打ち切りチェックを消すと、最後まで読んでハッシュ不一致の `Err` になり
    /// `expect` で落ちる（ハングせずに落ちるよう、無限 reader ではなく `take` で有限にしてある）。
    /// 打ち切り時にハッシュを検証しないことも、これで同時に固定できる。
    #[test]
    fn write_verified_stops_when_cancelled() {
        let dest = temp_path("cancelled.bin");
        let chunk = DOWNLOAD_BUF_SIZE as u64;
        let mut checks = 0u32;
        let end = write_verified(
            std::io::Read::take(std::io::repeat(0), chunk * 10),
            &dest,
            HELLO_SHA256,
            TEST_MAX,
            |_| {},
            || {
                // 2 チャンク読ませてから立てる（開始直後だけの経路にしない）。
                checks += 1;
                checks > 2
            },
        )
        .expect("cancelling is not a failure");
        assert_eq!(end, DownloadOutcome::Cancelled);
        // **ちょうど 2 チャンク**（`repeat` + `take` なので毎回きっかり埋まる）。厳密に見るのは、
        // 確認を `read` の後ろへ動かすミューテーションを捕まえるため（範囲で見ると 3 チャンクに
        // なっても通ってしまう）。
        assert_eq!(
            std::fs::metadata(&dest).expect("metadata").len(),
            chunk * 2,
            "the check must run before the read"
        );
        // 部分ファイルは残る（後始末は呼び出し側の `PartFile`）。
        assert!(dest.is_file());
        let _ = std::fs::remove_file(&dest);
    }

    /// 打ち切ったら**モデルファイルを配置しない**（検証を飛ばしているので、置いたら未検証の
    /// バイト列が取得済みとして扱われる）。一時ファイルも残らない。
    #[test]
    fn verify_and_place_leaves_nothing_behind_when_cancelled() {
        let dest = temp_path("not-placed.bin");
        let _ = std::fs::remove_file(&dest);
        let part = crate::atomic_replace::PartFile::for_dest(&dest).expect("dest has a file name");
        let part_path = part.path().to_path_buf();

        let outcome = verify_and_place(
            part,
            std::io::Read::take(std::io::repeat(0), DOWNLOAD_BUF_SIZE as u64 * 4),
            HELLO_SHA256,
            TEST_MAX,
            |_| {},
            || true,
        )
        .expect("cancelling is not a failure");

        assert_eq!(outcome, DownloadOutcome::Cancelled);
        assert!(!dest.exists(), "an unverified file must not be placed");
        assert!(!part_path.exists(), "the temporary file must be removed");
    }

    /// 検証に通ったら配置する（上のテストが「常に配置しない」で通らないようにする対）。
    #[test]
    fn verify_and_place_installs_a_verified_file() {
        let dest = temp_path("placed.bin");
        let _ = std::fs::remove_file(&dest);
        let part = crate::atomic_replace::PartFile::for_dest(&dest).expect("dest has a file name");

        let outcome = verify_and_place(
            part,
            b"hello".as_slice(),
            HELLO_SHA256,
            TEST_MAX,
            |_| {},
            never_cancelled,
        )
        .expect("a matching checksum should succeed");

        assert_eq!(outcome, DownloadOutcome::Completed);
        assert_eq!(std::fs::read(&dest).expect("readable"), b"hello");
        let _ = std::fs::remove_file(&dest);
    }

    /// 担当の交代をまたいでも打ち切りが届く。
    ///
    /// フラグ（`State::cancels`）は**いまの担当のもの**なので、担当が代わると新しいフラグに
    /// 置き換わり、その隙間に来た要求が消える。世代（`State::cancel_epoch`）はモデル ID に
    /// 紐づいて残るので、待っていたスレッドは「自分が頼まれた後に打ち切りが来た」と分かって
    /// 取得を始めずに降りられる。これが無いと、ユーザーが捨てたモデルを引き継ぎスレッドが
    /// 最初から落とし直す。
    #[test]
    fn a_cancel_during_the_handover_stops_the_taking_over_thread() {
        let downloader = ModelDownloader::new();
        let missing = temp_path("never-created-4.bin");

        // 引き継ぎスレッドが要求を出した時点の世代。
        let since_epoch = downloader.lock().cancel_epoch_of(FAKE_LLM_MODEL.id);

        // 待っている間に打ち切りが来る（担当がいないので `false` が返るが、世代は進む）。
        assert!(!downloader.cancel_download(FAKE_LLM_MODEL.id));

        let outcome = downloader
            .acquire_and_transfer(&FAKE_LLM_MODEL, &missing, since_epoch, |_| {
                panic!("the transfer must not start after a cancel arrived while waiting")
            })
            .expect("cancelling is not a failure");

        assert_eq!(outcome, DownloadOutcome::Cancelled);
        // 担当を引き受けていないので、状態も触っていない。
        assert_eq!(downloader.recorded_status(FAKE_LLM_MODEL.id), None);
    }

    /// UI 起点でスレッドを立てるかの判定（全 5 通りを固定する）。
    #[test]
    fn should_start_download_treats_a_cancelling_transfer_as_not_in_flight() {
        let downloading = DownloadStatus::Downloading {
            received: 0,
            total: 1,
        };

        assert!(!should_start_download(
            Some(&DownloadStatus::Downloaded),
            false,
            false
        ));
        // 走っている取得はそのまま任せる。
        assert!(!should_start_download(Some(&downloading), false, false));
        // ただし打ち切り待ちなら、撤退を待って引き継ぐために立てる。
        assert!(should_start_download(Some(&downloading), true, false));
        assert!(should_start_download(
            Some(&DownloadStatus::NotDownloaded),
            false,
            false
        ));
        assert!(should_start_download(
            Some(&DownloadStatus::Failed("boom".to_owned())),
            false,
            false
        ));
        // 記録が無いときはディスクの有無で決める。
        assert!(should_start_download(None, false, false));
        assert!(!should_start_download(None, false, true));
    }

    /// 打ち切りは「取得中で、かつワーカーが必要としていない」ときだけ効く。
    #[test]
    fn cancel_download_only_targets_an_unneeded_in_flight_download() {
        let downloader = ModelDownloader::new();

        // 取得中でなければ打ち切るものが無い。
        assert!(!downloader.cancel_download(FAKE_LLM_MODEL.id));

        // 担当を引き受けている状態を作る。`cancel_download` が見るのは `cancels` と `required`
        // だけなので、`status` は置かない（置いても効かないのに「その条件も検査した」と
        // 読めてしまうため）。「`cancels` と `Downloading` が必ず同時に立つ」ことは
        // `acquire_and_transfer` を通す下のテストが担保する。
        let cancel = Arc::new(AtomicBool::new(false));
        downloader
            .lock()
            .cancels
            .insert(FAKE_LLM_MODEL.id, Arc::clone(&cancel));

        // ワーカーが必要としている間は打ち切らない（ジョブは投入時のモデルを要る）。
        {
            let _needed = RequiredMark::new(&downloader, FAKE_LLM_MODEL.id);
            assert!(!downloader.cancel_download(FAKE_LLM_MODEL.id));
            assert!(!cancel.load(Ordering::Relaxed));
        }

        // 印が外れたら打ち切れる。
        assert!(downloader.cancel_download(FAKE_LLM_MODEL.id));
        assert!(cancel.load(Ordering::Relaxed));
    }

    /// 担当の引き受けから結末の記録までの**配線**を、転送を差し替えて検査する。
    ///
    /// 転送そのもの（ネットワーク・ディスク）は通らないので、ここで見るのは
    /// 「`Downloading` と打ち切りフラグが同じロックで同時に立つ」「フラグが担当に届く」
    /// 「結末が状態へ記録され、フラグが掃除される」の 3 点。この継ぎ目が無いと、フラグの登録を
    /// 消すミューテーションが**全テスト緑のまま通ってしまう**（`docs/rules/testing.md`）。
    #[test]
    fn acquire_and_transfer_wires_the_cancel_flag_through_to_the_transfer() {
        let downloader = ModelDownloader::new();
        let missing = temp_path("never-created.bin");

        let outcome = downloader
            .acquire_and_transfer(&FAKE_LLM_MODEL, &missing, 0, |cancel| {
                // 担当を引き受けた時点で、取得中かつ打ち切り可能になっている。
                assert!(matches!(
                    downloader.recorded_status(FAKE_LLM_MODEL.id),
                    Some(DownloadStatus::Downloading { .. })
                ));
                assert!(!cancel.load(Ordering::Relaxed));
                // 外から打ち切りを頼むと、担当が持っているフラグに届く。
                assert!(downloader.cancel_download(FAKE_LLM_MODEL.id));
                assert!(cancel.load(Ordering::Relaxed));
                Ok(DownloadOutcome::Cancelled)
            })
            .expect("cancelling is not a failure");

        assert_eq!(outcome, DownloadOutcome::Cancelled);
        // 打ち切りは失敗ではないので未取得へ戻り、フラグは掃除されている。
        assert_eq!(
            downloader.recorded_status(FAKE_LLM_MODEL.id),
            Some(DownloadStatus::NotDownloaded)
        );
        assert!(downloader.lock().cancels.is_empty());
    }

    /// 打ち切り待ちの取得は「取得中」に数えない。数えると (1) 同じモデルを選び直したときに
    /// `request_download` が早期 return して**誰も取得しないまま終わり**、(2) 他モデルの
    /// 空き容量の事前確認が、止まると決まっている取得のぶんまで要求する。
    #[test]
    fn a_cancelling_download_counts_as_neither_in_flight_nor_reserved_space() {
        let downloader = ModelDownloader::new();
        let missing = temp_path("never-created-2.bin");

        downloader
            .acquire_and_transfer(&FAKE_LLM_MODEL, &missing, 0, |_| {
                // 打ち切り前: 取得中なので、他モデルの見積もりに残りバイトが乗る。
                assert_eq!(
                    downloader.in_flight_remaining_bytes(FAKE_SPEECH_MODEL.id),
                    FAKE_LLM_MODEL.size_bytes
                );
                assert!(!downloader.is_cancelling(FAKE_LLM_MODEL.id));

                assert!(downloader.cancel_download(FAKE_LLM_MODEL.id));

                // 打ち切り後: 取得中とは数えない。
                assert!(downloader.is_cancelling(FAKE_LLM_MODEL.id));
                assert_eq!(
                    downloader.in_flight_remaining_bytes(FAKE_SPEECH_MODEL.id),
                    0
                );
                Ok(DownloadOutcome::Cancelled)
            })
            .expect("cancelling is not a failure");
    }

    /// `ensure_model`（ワーカー起点）は「いま必要」の印を立て、`request_download`（UI 起点の
    /// 先行取得）は立てない——これが両者の唯一の違い。印がある間に打ち切れてしまうと、
    /// 文字起こし中に設定でモデルを選び直しただけでジョブの取得が止まる。
    #[test]
    fn only_the_worker_entry_marks_the_model_as_required() {
        let downloader = ModelDownloader::new();
        let missing = temp_path("never-created-3.bin");

        // ワーカー起点（`ensure_model` が通る経路）は印を立てるので、打ち切れない。
        let worker_ran = std::cell::Cell::new(false);
        downloader
            .ensure_and_transfer(&FAKE_LLM_MODEL, &missing, |cancel| {
                worker_ran.set(true);
                assert!(!downloader.cancel_download(FAKE_LLM_MODEL.id));
                assert!(!cancel.load(Ordering::Relaxed));
                Ok(DownloadOutcome::Cancelled)
            })
            .expect("cancelling is not a failure");
        // **担当を引き受けたことまで確かめる**: 世代の不一致で降りても戻り値は同じ
        // `Ok(Cancelled)` なので、走ったかを見ないとテストが素通りする。
        assert!(worker_ran.get(), "the worker entry must take the download");

        // UI 起点（`request_download` が通る経路）は印を立てないので、打ち切れる。世代は
        // ハードコードせず、要求時点の値を読む（前半の副作用に暗黙依存しないため）。
        let since_epoch = downloader.lock().cancel_epoch_of(FAKE_LLM_MODEL.id);
        let ui_ran = std::cell::Cell::new(false);
        downloader
            .acquire_and_transfer(&FAKE_LLM_MODEL, &missing, since_epoch, |cancel| {
                ui_ran.set(true);
                assert!(downloader.cancel_download(FAKE_LLM_MODEL.id));
                assert!(cancel.load(Ordering::Relaxed));
                Ok(DownloadOutcome::Cancelled)
            })
            .expect("cancelling is not a failure");
        assert!(ui_ran.get(), "the UI entry must take the download");
    }

    /// ワーカーが必要としている間は、打ち切りが**世代も進めない**。進めてしまうと、UI で別の
    /// モデルを選んだだけでワーカーの担当引き受けが世代の不一致で降り、ジョブが失敗する。
    #[test]
    fn a_required_model_keeps_its_cancel_epoch() {
        let downloader = ModelDownloader::new();
        let before = downloader.lock().cancel_epoch_of(FAKE_LLM_MODEL.id);

        {
            let _needed = RequiredMark::new(&downloader, FAKE_LLM_MODEL.id);
            assert!(!downloader.cancel_download(FAKE_LLM_MODEL.id));
            assert_eq!(downloader.lock().cancel_epoch_of(FAKE_LLM_MODEL.id), before);
        }

        // 印が外れたら進む（上の assert が「そもそも進まない実装」で通らないようにする対）。
        assert!(!downloader.cancel_download(FAKE_LLM_MODEL.id));
        assert_eq!(
            downloader.lock().cancel_epoch_of(FAKE_LLM_MODEL.id),
            before + 1
        );
    }

    /// UI 起点の要求の受け付け（判定＋世代の捕捉）。`request_download` はこの結果で
    /// スレッドを立てるかを決めるだけなので、繋ぎの契約はここで固定する。
    #[test]
    fn begin_ui_request_starts_only_when_needed_and_carries_the_current_epoch() {
        let downloader = ModelDownloader::new();
        let downloading = DownloadStatus::Downloading {
            received: 0,
            total: FAKE_LLM_MODEL.size_bytes,
        };

        // 取得中で打ち切られていなければ立てない。
        downloader.set_status_for_test(&FAKE_LLM_MODEL, downloading.clone());
        downloader
            .lock()
            .cancels
            .insert(FAKE_LLM_MODEL.id, Arc::new(AtomicBool::new(false)));
        assert_eq!(downloader.begin_ui_request(&FAKE_LLM_MODEL), None);

        // 打ち切り待ちなら立てる。返るのは**打ち切りの後**の世代なので、引き継ぎスレッドは
        // `acquire_and_transfer` の比較で降りずに担当を引き受けられる。
        assert!(downloader.cancel_download(FAKE_LLM_MODEL.id));
        let epoch = downloader.lock().cancel_epoch_of(FAKE_LLM_MODEL.id);
        assert_eq!(downloader.begin_ui_request(&FAKE_LLM_MODEL), Some(epoch));

        // 取得済みなら立てない。
        downloader.set_status_for_test(&FAKE_LLM_MODEL, DownloadStatus::Downloaded);
        assert_eq!(downloader.begin_ui_request(&FAKE_LLM_MODEL), None);
    }

    /// 同じモデルを 2 つのワーカーが必要としても、片方が抜けただけでは印は外れない
    /// （本数で持つ理由。片方の終了で打ち切れるようになると、残った側のジョブが巻き添えになる）。
    #[test]
    fn required_marks_nest_by_count() {
        let downloader = ModelDownloader::new();
        let cancel = Arc::new(AtomicBool::new(false));
        downloader
            .lock()
            .cancels
            .insert(FAKE_LLM_MODEL.id, Arc::clone(&cancel));

        let outer = RequiredMark::new(&downloader, FAKE_LLM_MODEL.id);
        {
            let _inner = RequiredMark::new(&downloader, FAKE_LLM_MODEL.id);
        }
        assert!(
            !downloader.cancel_download(FAKE_LLM_MODEL.id),
            "the outer worker still needs it"
        );
        drop(outer);
        assert!(downloader.cancel_download(FAKE_LLM_MODEL.id));
    }

    /// 打ち切りの記録は `NotDownloaded`（`Failed` ではない）で、打ち切りフラグも掃除される。
    /// 掃除しないと、次に始めた取得が立った直後に古いフラグで打ち切られる。
    #[test]
    fn finishing_a_cancelled_download_clears_the_flag_and_records_not_downloaded() {
        let downloader = ModelDownloader::new();
        {
            let mut state = downloader.lock();
            state.status.insert(
                FAKE_LLM_MODEL.id,
                DownloadStatus::Downloading {
                    received: 0,
                    total: FAKE_LLM_MODEL.size_bytes,
                },
            );
            state
                .cancels
                .insert(FAKE_LLM_MODEL.id, Arc::new(AtomicBool::new(true)));
        }

        DownloadGuard::new(&downloader, FAKE_LLM_MODEL.id)
            .finish(Ok(DownloadOutcome::Cancelled))
            .expect("cancelling is not a failure");

        assert_eq!(
            downloader.recorded_status(FAKE_LLM_MODEL.id),
            Some(DownloadStatus::NotDownloaded)
        );
        assert!(downloader.lock().cancels.is_empty());
        // フラグが残っていないので、次の取得は打ち切り済み扱いにならない。
        assert!(!downloader.cancel_download(FAKE_LLM_MODEL.id));
    }

    #[test]
    fn status_of_prefers_in_memory_state() {
        // マップに載っている状態（進行中・失敗）はディスクの有無より優先される。
        // ディスクフォールバック自体は実環境のデータディレクトリに依存するためここでは
        // 検証しない（実 DL の #[ignore] スモークが Downloaded への遷移を確認する）。
        let downloader = ModelDownloader::new();
        let spec = &FAKE_LLM_MODEL;
        downloader.set_status_for_test(
            spec,
            DownloadStatus::Downloading {
                received: 1,
                total: 100,
            },
        );
        assert!(matches!(
            downloader.status_of(spec),
            DownloadStatus::Downloading { .. }
        ));
        downloader.set_status_for_test(spec, DownloadStatus::Failed("boom".into()));
        assert_eq!(
            downloader.status_of(spec),
            DownloadStatus::Failed("boom".into())
        );
    }

    /// 第 2 のモデル種別を足しても基盤がそのまま使えること。状態は ID をキーにするので
    /// 互いに混ざらず、保存先はファイル名で分かれる。
    ///
    /// 実カタログ・実ディスクに依存しないよう、両方ともダミー定義で見る（実カタログを
    /// 混ぜると「マップに無い側は Downloaded か NotDownloaded」という自明な条件しか
    /// 書けず、汚染しても落ちないテストになる）。
    #[test]
    fn a_second_model_kind_is_tracked_independently() {
        let downloader = ModelDownloader::new();
        downloader.set_status_for_test(&FAKE_LLM_MODEL, DownloadStatus::Failed("boom".into()));
        downloader.set_status_for_test(
            &FAKE_SPEECH_MODEL,
            DownloadStatus::Downloading {
                received: 1,
                total: 2_048,
            },
        );

        // 片方の状態がもう片方へ漏れない（ID が衝突していればここで落ちる）。
        assert_eq!(
            downloader.status_of(&FAKE_LLM_MODEL),
            DownloadStatus::Failed("boom".into())
        );
        assert_eq!(
            downloader.status_of(&FAKE_SPEECH_MODEL),
            DownloadStatus::Downloading {
                received: 1,
                total: 2_048,
            }
        );

        // 保存先はファイル名だけが違う（同じ models/ 配下）。
        let (Some(llm_path), Some(speech_path)) =
            (model_path(&FAKE_LLM_MODEL), model_path(&FAKE_SPEECH_MODEL))
        else {
            // データディレクトリを解決できない環境ではこの確認だけ飛ばす（黙って通さない）。
            eprintln!("Skipping the path comparison because the data directory is unavailable");
            return;
        };
        assert_eq!(llm_path.parent(), speech_path.parent());
        assert_eq!(
            llm_path.file_name().and_then(|n| n.to_str()),
            Some(FAKE_LLM_MODEL.filename)
        );
        assert_ne!(llm_path, speech_path);
    }

    /// パス要素を含むファイル名は `models/` の外を指しうるので解決しない。
    #[test]
    fn model_path_rejects_non_plain_filenames() {
        static ESCAPING: ModelSpec = ModelSpec {
            kind: "Test",
            id: "test-escaping",
            display_name: "Escaping",
            description: "used only by tests",
            size_bytes: 1,
            filename: "../outside.bin",
            url: "https://example.invalid/outside.bin",
            sha256: UNUSED_SHA256,
        };
        assert!(model_path(&ESCAPING).is_none());
        assert!(!is_plain_filename("a/b.bin"));
        assert!(!is_plain_filename("/abs.bin"));
        assert!(is_plain_filename("ggml-tiny.bin"));
    }
}
