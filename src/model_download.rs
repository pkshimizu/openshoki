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
//! `catalog_checks`）だけを担う。種別固有の中身（どのモデルを載せるか）には触らない。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
    /// ログに出す種別（例: `Whisper speech`）。モデル種別が増えたとき、どちらの
    /// ダウンロードかをログで見分けるために使う。
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

/// 識別子 → カタログ内インデックス。カタログ外（設定の手編集値）は `default_id` の位置へ
/// フォールバックする（値自体は書き換えず、表示だけ既定位置になる）。
///
/// 種別ごとのカタログが同じ解決をするための正（`whisper_model::model_index` /
/// `summary_model::model_index` から呼ぶ）。設定画面の ComboBox の選択位置に使う。
pub fn catalog_index(catalog: &[ModelSpec], id: &str, default_id: &str) -> usize {
    catalog
        .iter()
        .position(|spec| spec.id == id)
        .unwrap_or_else(|| {
            catalog
                .iter()
                .position(|spec| spec.id == default_id)
                .expect("the default model id is always in the catalog")
        })
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

/// モデルのダウンロードと状態を管理するハンドル。`Clone` で共有し、UI（設定画面）と
/// ワーカーの両方から同じ状態を参照・更新する。
#[derive(Clone)]
pub struct ModelDownloader {
    /// モデル ID → 取得状況。エントリが無いモデルはディスクの有無で判定する
    /// （`Downloaded` / `NotDownloaded` は必ずしもマップに載らない）。
    status: Arc<Mutex<HashMap<&'static str, DownloadStatus>>>,
}

impl Default for ModelDownloader {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelDownloader {
    pub fn new() -> Self {
        Self {
            status: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 表示用の現在状況。マップにあればそれを、無ければディスクの有無で判定する。
    /// ディスク判定で取得済みと分かったらマップへ記録し、以後の照会（設定画面の 100ms
    /// ポーリング）が毎回 stat を打たないようにする。表示はメモリ状態を優先するため、
    /// 取得後にファイルを外部で消しても表示は Downloaded のまま（実際の利用時は
    /// `ensure_model` がディスクを再確認するので機能は壊れない）。
    pub fn status_of(&self, spec: &'static ModelSpec) -> DownloadStatus {
        let mut status = self.lock();
        if let Some(current) = status.get(spec.id) {
            return current.clone();
        }
        match model_path(spec) {
            Some(path) if path.is_file() => {
                status.insert(spec.id, DownloadStatus::Downloaded);
                DownloadStatus::Downloaded
            }
            _ => DownloadStatus::NotDownloaded,
        }
    }

    /// UI 起点: 未取得（または直近失敗）ならバックグラウンドスレッドでダウンロードを開始する。
    /// 取得済み・ダウンロード中ならスレッドを立てずに戻る（DL 中の完了待ちは `ensure_model` を
    /// 呼ぶ利用側だけが行えばよい）。結果は状態マップとログに残る。
    ///
    /// **同時ダウンロード数の上限は持たない**（#120 で判断。以下がその根拠）。whisper（最大
    /// 2.9GB）と要約 LLM（最大 4.4GB）の 2 種別があり、どちらも UI 起点（設定画面での選択）と
    /// ワーカー起点（`ensure_model`）を持つため**並走しうる**（推論を直列化する
    /// `crate::inference_slot` は、待たせても意味が無いダウンロードを対象にしていない）。
    /// さらに同種別で別モデルを選び直しても先の取得は中断しないので、最悪では 3 本以上・
    /// 10GB 級の同時受信になる。それでも上限を入れないのは:
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
    ///   （同一モデルの待ちにさえ上限つきタイムアウトが必要になっている。`ensure_model`）。
    ///
    /// 並走で本当に困るのはディスクなので、そちらは埋め尽くしを**上限ではなく事前確認**で防ぐ:
    /// 受信サイズの上限（`max_download_bytes`）と、開始前の空き容量確認
    /// （`insufficient_space_reason_for_dir`）。
    /// 確認に落ちた取得は**待たせずに失敗させる**（この doc の 2 点目のとおり、待たせる害を
    /// 避けるため）。文字起こし中なら当該セッションのジョブが失敗し、次のジョブ・設定画面での
    /// 再選択で再試行される（自動リトライは無い）。
    ///
    /// 同種別で別モデルを選び直したときに先の取得を打ち切る仕組みは別件（#124）。
    pub fn request_download(&self, spec: &'static ModelSpec) {
        match self.status_of(spec) {
            DownloadStatus::Downloaded | DownloadStatus::Downloading { .. } => return,
            DownloadStatus::NotDownloaded | DownloadStatus::Failed(_) => {}
        }
        let downloader = self.clone();
        let spawned = std::thread::Builder::new()
            .name(format!("model-download-{}", spec.id))
            .spawn(move || {
                // ensure_model が check-and-set・進捗更新・結果記録まで行う。取得済みなら即返る。
                if let Err(err) = downloader.ensure_model(spec) {
                    eprintln!("Skipping the model download because it failed: {err}");
                }
            });
        if let Err(err) = spawned {
            eprintln!("Skipping the model download because the thread failed to start: {err}");
        }
    }

    /// モデルのパスを返す。未取得ならダウンロードして配置する（成功するまで返さない）。
    /// 他スレッドが同じモデルをダウンロード中なら、その完了を待って結果を使う（二重取得しない。
    /// 先行が失敗した場合は後続が担当を引き継いで再取得する）。
    ///
    /// ダウンロードは分オーダーかかりうるため、メインスレッドから呼ばない
    /// （ワーカー／`request_download` のスレッドから呼ぶ）。
    pub fn ensure_model(
        &self,
        spec: &'static ModelSpec,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = model_path(spec).ok_or("Cannot determine the data directory")?;
        // 待機の上限。担当スレッドが**進まない**（無応答接続、Drop が走らない `abort` 等）と
        // 状態が Downloading のまま残り、上限なしでは待機側（逐次のワーカー等）が永久に固まって
        // 以後のジョブが黙って止まる。DL 全体のタイムアウトより長い上限で打ち切り、エラーとして
        // 返す（次のジョブ・次の選択で再試行される）。unwind するパニックで担当が消えた場合は
        // `DownloadGuard` が `Failed` へ倒すので、待機側は次の周回で担当を引き継げる。
        let wait_deadline = std::time::Instant::now() + WAIT_FOR_OTHER_DOWNLOAD_TIMEOUT;
        loop {
            {
                let mut status = self.lock();
                match status.get(spec.id) {
                    // 他スレッドがダウンロード中。ロックを放して完了を待つ。
                    Some(DownloadStatus::Downloading { .. }) => {}
                    _ => {
                        if path.is_file() {
                            status.insert(spec.id, DownloadStatus::Downloaded);
                            return Ok(path);
                        }
                        // 自分がダウンロード担当になる（check-and-set。ロック内で遷移させ、
                        // 同じモデルを同時に見た 2 スレッドが両方ダウンロードするのを防ぐ）。
                        status.insert(
                            spec.id,
                            DownloadStatus::Downloading {
                                received: 0,
                                total: spec.size_bytes,
                            },
                        );
                        break;
                    }
                }
            }
            if std::time::Instant::now() >= wait_deadline {
                return Err(
                    "timed out waiting for another download of the same model to finish".into(),
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }

        // 担当を引き受けた区間は番人で囲み、結果の記録もそれに任せる（理由と後始末の契約は
        // `DownloadGuard` の doc）。この 2 行の組（囲む・`finish` に記録させる）を崩すと、
        // 取得中のパニックで状態が `Downloading` のまま残る——テストでは捕まらないので崩さないこと。
        let guard = DownloadGuard::new(self, spec.id);
        guard.finish(download_model(spec, &path, self))?;
        Ok(path)
    }

    /// 走っている**他の**ダウンロードの残りバイト合計（`except_model_id` 自身は除く。
    /// `ensure_model` は取得開始時に自分を `Downloading` へ遷移させるので、除かないと自分の
    /// サイズを二重に要求してしまう）。必要量へ加算する理由は `insufficient_space_reason` の doc。
    ///
    /// 既に書けたぶん（`received`）は空き容量に反映済みなので残りだけを数える。進捗の更新は
    /// `PROGRESS_STEP_BYTES` 刻みで遅れるが、その遅れは残りを多めに見る＝安全側に転ぶ。
    fn in_flight_remaining_bytes(&self, except_model_id: &str) -> u64 {
        self.lock()
            .iter()
            .filter_map(|(id, status)| match status {
                DownloadStatus::Downloading { received, total } if *id != except_model_id => {
                    Some(total.saturating_sub(*received))
                }
                _ => None,
            })
            .sum()
    }

    /// テスト用: 状態を直接注入する（表示ロジックをディスク・ネットワーク非依存で検証する）。
    #[cfg(test)]
    pub(crate) fn set_status_for_test(&self, spec: &'static ModelSpec, status: DownloadStatus) {
        self.lock().insert(spec.id, status);
    }

    /// 状態マップのガードを取る。poison（ロック保持中のパニック）でも状態表示・DL 管理を
    /// 止めないため、ガードを取り出して続行する（`docs/rules/error-handling.md`）。
    fn lock(&self) -> MutexGuard<'_, HashMap<&'static str, DownloadStatus>> {
        self.status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 取得を引き受けたスレッドが**結果を必ず状態マップへ残す**ための番人。`finish` で結果を
/// 記録して解除し、記録せずに drop されたら（取得中のパニック等）`Failed` へ倒す。
///
/// **ここが `Downloading` の後始末の正**。これが無いと状態が `Downloading` のまま残り、
/// (1) `request_download` が早期 return してそのモデルを二度と取得しない、
/// (2) `insufficient_space_reason` の並走ぶんに恒久的に加算されて**他のモデルの取得まで
/// 空き容量不足として断られる**、という詰まりがプロセスの寿命いっぱい続く（`ensure_model` の
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

    /// 取得の結果を状態マップへ記録し、番人を解除して結果をそのまま返す。
    ///
    /// 記録と解除をここにまとめてあるので、呼び出し側は順序を気にしなくてよい（`Drop` の中で
    /// 状態マップのロックを取るため、外でロックを保持したまま解除すると自己デッドロックする。
    /// その順序制約はコメントではなく実装で守る形にしてある）。
    fn finish(
        mut self,
        result: Result<(), Box<dyn std::error::Error>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // 記録する値はロックを取る前に作る（`to_string` がパニックしても番人が畳める）。
        let status = match &result {
            Ok(()) => DownloadStatus::Downloaded,
            Err(err) => DownloadStatus::Failed(err.to_string()),
        };
        self.downloader.lock().insert(self.id, status);
        self.recorded = true;
        result
    }
}

impl Drop for DownloadGuard<'_> {
    fn drop(&mut self) {
        if self.recorded {
            return;
        }
        // 失敗として記録し、次の要求（次のジョブ・設定画面での再選択）で再試行できる状態に戻す。
        self.downloader.lock().insert(
            self.id,
            DownloadStatus::Failed("the download stopped unexpectedly".to_owned()),
        );
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

/// 取り残された一時ファイルと見なす、最終更新からの経過時間（`sweep_orphaned_part_files`）。
///
/// **受信全体のタイムアウト（`RECV_BODY_TIMEOUT` の 2 時間）より長く**取る。走っている取得の
/// 一時ファイルは書き込みのたびに mtime が更新されるので、これだけ放置されたものは生きた取得では
/// ありえない（2 時間受信が進まなければ、受信側がタイムアウトして自分で片付ける）。多重起動した
/// 別プロセスの取得を壊さないための余裕でもある。
const STALE_PART_AGE: std::time::Duration = std::time::Duration::from_secs(3 * 60 * 60);

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
/// 録音側の一時ファイル（`mixdown::normalize_if_quiet` が `mic.mp3` / `system.mp3` を書き直す
/// ときの `*.part.*`）は掃除しない: そちらは**ユーザーが選んだ保存先**にあり、Finder から見えて
/// 自分で消せる。1 セッションぶん（128 kbps で 1 時間あたり数十 MB）と小さいので、起動時に
/// ユーザーのフォルダを走査して消すリスクを取るほどではないという判断。
pub fn sweep_orphaned_part_files() {
    let Some(dir) = models_dir() else {
        return;
    };
    let removed = crate::atomic_replace::sweep_orphaned_parts(
        &dir,
        std::time::SystemTime::now(),
        STALE_PART_AGE,
    );
    if removed > 0 {
        println!("Reclaimed {removed} leftover model download(s)");
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
fn download_model(
    spec: &'static ModelSpec,
    dest: &Path,
    downloader: &ModelDownloader,
) -> Result<(), Box<dyn std::error::Error>> {
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
            .insert(spec.id, DownloadStatus::Downloading { received, total });
    };
    write_verified(
        reader,
        part.path(),
        spec.sha256,
        max_download_bytes(spec),
        on_progress,
    )?;
    part.commit()?;
    println!("Downloaded the {} model {}", spec.kind, spec.display_name);
    Ok(())
}

/// `reader` の内容を `dest` へ書き出しつつ SHA-256 を計算し、`expected_sha256` と一致しなければ
/// エラーを返す（ファイルは書かれたまま残る。後始末は呼び出し側の `PartFile` が持つ）。
/// `max_bytes` を超える受信は打ち切る（想定外の応答でディスクを埋めない保険。テスト容易性の
/// ため引数で受ける）。`on_progress` には累積受信バイトを `PROGRESS_STEP_BYTES` ごとに渡す。
fn write_verified(
    mut reader: impl Read,
    dest: &Path,
    expected_sha256: &str,
    max_bytes: u64,
    mut on_progress: impl FnMut(u64),
) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(dest)?;
    let mut writer = std::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut buf = [0u8; DOWNLOAD_BUF_SIZE];
    let mut written: u64 = 0;
    let mut last_reported: u64 = 0;
    loop {
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
    Ok(())
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

    /// 全カタログの登録簿（カタログと、その既定 ID）。**種別を足したらここに 1 行足す**。
    /// 下のテストが横断で一意性を見る。
    const ALL_CATALOGS: &[(&[ModelSpec], &str)] = &[
        (
            crate::whisper_model::CATALOG,
            crate::whisper_model::DEFAULT_MODEL_ID,
        ),
        (
            crate::summary_model::CATALOG,
            crate::summary_model::DEFAULT_MODEL_ID,
        ),
    ];

    /// 登録簿のカタログすべてが健全で、ID とファイル名は種別をまたいで一意
    /// （状態マップのキーと保存先が種別で混ざらないように）。
    ///
    /// `assert_valid` もここで回すので、**カタログを足す側は登録簿へ 1 行足すだけでよい**
    /// （各カタログのテストからも呼べるが、呼び忘れてもここで捕まる）。
    #[test]
    fn registered_catalogs_are_valid_and_globally_unique() {
        for (catalog, default_id) in ALL_CATALOGS {
            assert_valid(catalog, default_id);
        }

        let specs: Vec<&ModelSpec> = ALL_CATALOGS
            .iter()
            .flat_map(|(catalog, _)| catalog.iter())
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
        // 自分の分は数えない（`ensure_model` が取得開始時に Downloading へ遷移させるため、
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
            .finish(Ok(()))
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
        write_verified(b"hello".as_slice(), &dest, HELLO_SHA256, TEST_MAX, |_| {})
            .expect("matching checksum should succeed");
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
        write_verified(data.as_slice(), &dest, &expected, TEST_MAX, |received| {
            reported.push(received);
        })
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
        )
        .expect_err("exceeding the size limit should fail");
        assert!(err.to_string().contains("size limit"));
        let _ = std::fs::remove_file(&dest);
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
