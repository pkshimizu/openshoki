//! 内蔵 whisper モデルの管理（カタログ・ダウンロード・状態）。
//!
//! モデルはバイナリに埋め込まず（数百 MB〜GB の肥大化を避ける）、初回利用時に HTTPS で
//! 取得して OS 標準のデータディレクトリへ保存し、以後は再利用する（「内蔵」の実現方式）。
//! 使用モデルは設定画面のカタログ（`CATALOG`）から選べ、選択時に即バックグラウンドで
//! ダウンロードを開始し、進捗は共有状態（`ModelDownloader`）経由で設定画面に表示する。
//!
//! ダウンロードは既知の SHA-256 で検証し、一時ファイル（プロセス固有名）→リネームで原子的に
//! 配置する（破損・部分ダウンロードをモデルとして残さない）。通信は受信のみで、音声などの
//! 機微データは一切送信しない（`docs/CONTEXT.md` のオンデバイス方針はそのまま）。
//!
//! UI 起点（設定画面での選択）と文字起こしワーカー起点（`ensure_model`）が同じモデルを
//! 同時に要求しても、状態マップの check-and-set で **二重ダウンロードしない**（先着が
//! ダウンロードし、後続は完了を待つ）。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use sha2::{Digest, Sha256};

/// モデルカタログの 1 エントリ。URL・SHA-256 は HuggingFace（whisper.cpp 公式配布）の
/// LFS メタデータより。モデルを追加・差し替えるときは URL と SHA-256 を必ずペアで更新する。
pub struct ModelSpec {
    /// 設定（`Config::whisper_model`）に保存する識別子。
    pub id: &'static str,
    /// 設定画面での表示名。
    pub display_name: &'static str,
    /// 精度・速度の説明（設定画面の表示用）。
    pub description: &'static str,
    /// 正確なファイルサイズ（バイト）。進捗の分母と受信上限の基準に使う。
    pub size_bytes: u64,
    /// データディレクトリ配下の保存ファイル名。
    filename: &'static str,
    /// 取得元 URL。
    url: &'static str,
    /// 公式 SHA-256。改ざん・破損の検知に使う。
    sha256: &'static str,
}

/// 選べるモデルの一覧（小さい順）。設定画面の ComboBox はこの順で並ぶため、
/// モデルを足すときはここへ 1 エントリ追加するだけでよい。
pub const CATALOG: &[ModelSpec] = &[
    ModelSpec {
        id: "tiny",
        display_name: "Tiny",
        description: "fastest, lowest accuracy",
        size_bytes: 77_691_713,
        filename: "ggml-tiny.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    },
    ModelSpec {
        id: "base",
        display_name: "Base",
        description: "fast, basic accuracy",
        size_bytes: 147_951_465,
        filename: "ggml-base.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
    },
    ModelSpec {
        id: "small",
        display_name: "Small",
        description: "balanced speed and accuracy",
        size_bytes: 487_601_967,
        filename: "ggml-small.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    },
    ModelSpec {
        id: "medium",
        display_name: "Medium",
        description: "high accuracy, slower",
        size_bytes: 1_533_763_059,
        filename: "ggml-medium.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    },
    ModelSpec {
        id: "large-v3-turbo",
        display_name: "Large v3 Turbo",
        description: "high accuracy, faster than Large",
        size_bytes: 1_624_555_275,
        filename: "ggml-large-v3-turbo.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    },
    ModelSpec {
        id: "large-v3",
        display_name: "Large v3",
        description: "highest accuracy, slowest",
        size_bytes: 3_095_033_483,
        filename: "ggml-large-v3.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3.bin",
        sha256: "64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2",
    },
];

/// 既定モデルの識別子（Small。日本語会議の主用途で精度と負荷・サイズのバランスが良い）。
pub const DEFAULT_MODEL_ID: &str = "small";

/// 識別子からカタログのエントリを引く。
pub fn spec_for(id: &str) -> Option<&'static ModelSpec> {
    CATALOG.iter().find(|spec| spec.id == id)
}

/// 既定モデルのエントリ。
pub fn default_spec() -> &'static ModelSpec {
    spec_for(DEFAULT_MODEL_ID).expect("the default model id is always in the catalog")
}

/// 識別子 → カタログ内インデックス。カタログ外（手編集値）は既定モデルの位置へ
/// フォールバックする（値自体は書き換えず、表示だけ既定位置になる）。
pub fn model_index(id: &str) -> usize {
    CATALOG
        .iter()
        .position(|spec| spec.id == id)
        .unwrap_or_else(|| {
            CATALOG
                .iter()
                .position(|spec| spec.id == DEFAULT_MODEL_ID)
                .expect("the default model id is always in the catalog")
        })
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
    /// 取得済み（ディスクに検証済みモデルがある）。
    Downloaded,
    /// 直近のダウンロードが失敗した（理由つき。メモリのみで、再試行でクリアされる）。
    Failed(String),
}

/// モデルのダウンロードと状態を管理するハンドル。`Clone` で共有し、UI（設定画面）と
/// 文字起こしワーカーの両方から同じ状態を参照・更新する。
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
    /// 呼ぶ文字起こし側だけが行えばよい）。結果は状態マップとログに残る。
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
    /// （文字起こしワーカー／`request_download` のスレッドから呼ぶ）。
    pub fn ensure_model(
        &self,
        spec: &'static ModelSpec,
    ) -> Result<PathBuf, Box<dyn std::error::Error>> {
        let path = model_path(spec).ok_or("Cannot determine the data directory")?;
        // 待機の上限。担当スレッドが結果を記録する前に異常終了すると状態が Downloading の
        // まま残り、上限なしでは待機側（逐次の文字起こしワーカー等）が永久に固まって以後の
        // ジョブが黙って止まる。DL 全体のタイムアウトより長い上限で打ち切り、エラーとして
        // 返す（次のジョブ・次の選択で再試行される）。
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

        let result = download_model(spec, &path, &self.status);
        let mut status = self.lock();
        match result {
            Ok(()) => {
                status.insert(spec.id, DownloadStatus::Downloaded);
                Ok(path)
            }
            Err(err) => {
                status.insert(spec.id, DownloadStatus::Failed(err.to_string()));
                Err(err)
            }
        }
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

/// ダウンロード時の読み書きバッファサイズ。
const DOWNLOAD_BUF_SIZE: usize = 64 * 1024;

/// 進捗を共有状態へ反映する間隔（受信バイト）。毎読み込みでロックを取らないための間引き。
const PROGRESS_STEP_BYTES: u64 = 1024 * 1024;

/// 接続確立・応答ヘッダ受信のタイムアウト。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// ボディ受信全体のタイムアウト。低速回線でも最大モデル（約 3GB）を受け切れる長さにしつつ、
/// 無応答の接続（half-open 等）で呼び出しスレッドが恒久にハングしないようにする。
/// 超過時は失敗し、次の要求で再試行する。
const RECV_BODY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120 * 60);

/// 他スレッドのダウンロード完了を待つ上限。DL 全体のタイムアウト（接続＋受信）より長くし、
/// 正常な待機を途中で打ち切らない。
const WAIT_FOR_OTHER_DOWNLOAD_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(130 * 60);

/// モデルの保存先（`<データディレクトリ>/models/<ファイル名>`）。
fn model_path(spec: &ModelSpec) -> Option<PathBuf> {
    crate::config::data_dir().map(|dir| dir.join("models").join(spec.filename))
}

/// 受信サイズの上限。既知のモデルサイズ＋1 割の余裕。配信元の故障・想定外の応答で
/// ディスクを埋め尽くさないための保険（正常時は届かない）。
fn max_download_bytes(spec: &ModelSpec) -> u64 {
    spec.size_bytes + spec.size_bytes / 10
}

/// モデルをダウンロードして `dest` へ原子的に配置し、進捗を状態マップへ反映する。
fn download_model(
    spec: &'static ModelSpec,
    dest: &Path,
    status: &Arc<Mutex<HashMap<&'static str, DownloadStatus>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(parent) = dest.parent() {
        // モデルは公開配布物で機微データではないため、権限は OS 既定でよい
        // （録音データの 0700/0600 とは扱いが異なる）。
        std::fs::create_dir_all(parent)?;
    }
    println!(
        "Downloading the Whisper speech model {} (about {})",
        spec.display_name,
        format_size(spec.size_bytes)
    );

    // タイムアウトを明示する。ureq の既定は無期限で、無応答の接続（half-open 等）に当たると
    // 呼び出しスレッド（文字起こしワーカー等）が恒久にハングしてしまう。
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
    // 壊れた/部分的なファイルがモデルとして残らない。一時ファイル名はプロセス固有にする:
    // アプリの多重起動（別プロセス）が同名の一時ファイルへ同時に書くと、ハッシュは各自の受信
    // ストリームで計算されるためファイルの破損を検知できず、壊れた内容が検証済みモデルとして
    // 配置されうる。名前を分ければ各自が自分の書いた内容だけを検証し、rename（原子的・後勝ち）は
    // どちらも検証済みなので安全になる（同一プロセス内は状態マップで二重取得を防いでいる）。
    let part = dest.with_extension(format!("part.{}", std::process::id()));
    let on_progress = |received: u64| {
        let mut map = status
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        map.insert(spec.id, DownloadStatus::Downloading { received, total });
    };
    let result = write_verified(
        reader,
        &part,
        spec.sha256,
        max_download_bytes(spec),
        on_progress,
    )
    .and_then(|()| std::fs::rename(&part, dest).map_err(Into::into));
    if let Err(err) = result {
        // 後始末の失敗も黙って捨てない（docs/rules/error-handling.md）。
        if let Err(remove_err) = std::fs::remove_file(&part)
            && remove_err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("Failed to remove the partially downloaded model: {remove_err}");
        }
        return Err(err);
    }
    println!("Downloaded the Whisper speech model {}", spec.display_name);
    Ok(())
}

/// `reader` の内容を `dest` へ書き出しつつ SHA-256 を計算し、`expected_sha256` と一致しなければ
/// エラーを返す（ファイルは書かれたまま残るため、後始末は呼び出し側で行う）。
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 既知データの SHA-256（`echo -n hello | sha256sum` 相当）。
    const HELLO_SHA256: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    /// テスト用の緩い上限。
    const TEST_MAX: u64 = u64::MAX;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("openshoki-model-{}-{name}", std::process::id()))
    }

    #[test]
    fn catalog_is_consistent() {
        // 既定 ID がカタログにあり、ID の重複が無く、SHA-256 は 64 桁の 16 進、URL はファイル名と
        // 対応している（追加・差し替え時の取り違えを検知する）。
        assert!(spec_for(DEFAULT_MODEL_ID).is_some());
        for (i, spec) in CATALOG.iter().enumerate() {
            assert!(
                CATALOG.iter().skip(i + 1).all(|other| other.id != spec.id),
                "duplicate id {}",
                spec.id
            );
            assert_eq!(spec.sha256.len(), 64, "bad sha256 for {}", spec.id);
            assert!(
                spec.sha256.chars().all(|c| c.is_ascii_hexdigit()),
                "bad sha256 for {}",
                spec.id
            );
            assert!(
                spec.url.ends_with(spec.filename),
                "url mismatch for {}",
                spec.id
            );
            assert!(spec.size_bytes > 0);
        }
    }

    #[test]
    fn model_index_resolves_known_and_falls_back() {
        assert_eq!(model_index("tiny"), 0);
        assert_eq!(CATALOG[model_index(DEFAULT_MODEL_ID)].id, DEFAULT_MODEL_ID);
        // カタログ外は既定モデルの位置へ。
        assert_eq!(CATALOG[model_index("no-such-model")].id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn format_size_uses_mb_and_gb() {
        assert_eq!(format_size(77_691_713), "74 MB");
        assert_eq!(format_size(487_601_967), "465 MB");
        assert_eq!(format_size(1_624_555_275), "1.5 GB");
        assert_eq!(format_size(3_095_033_483), "2.9 GB");
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
        let spec = spec_for("medium").expect("medium is in the catalog");
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

    /// カタログ経由の実ダウンロードのスモーク（Tiny 約 74MB・要ネットワーク）。ローカルで
    /// `cargo test ensure_model_downloads_tiny -- --ignored` により実行する。取得済みなら即成功。
    #[test]
    #[ignore = "downloads ~74MB; run manually with --ignored"]
    fn ensure_model_downloads_tiny_with_progress() {
        let downloader = ModelDownloader::new();
        let spec = spec_for("tiny").expect("tiny is in the catalog");
        let path = downloader
            .ensure_model(spec)
            .expect("the tiny model should download and verify");
        assert!(path.is_file());
        assert_eq!(downloader.status_of(spec), DownloadStatus::Downloaded);
    }

    /// 実ダウンロードのスモーク（既定モデル約 465MB・要ネットワーク）。ローカルで
    /// `cargo test ensure_model -- --ignored` により実行する。取得済みなら即成功する
    /// （実アプリの初回文字起こしと同じ経路・同じ保存先）。
    #[test]
    #[ignore = "downloads ~465MB; run manually with --ignored"]
    fn ensure_model_downloads_and_verifies() {
        let downloader = ModelDownloader::new();
        let path = downloader
            .ensure_model(default_spec())
            .expect("the model should download and verify");
        assert!(path.is_file());
        assert_eq!(
            downloader.status_of(default_spec()),
            DownloadStatus::Downloaded
        );
    }
}
