//! 内蔵 whisper モデルのカタログ（選べるモデルの一覧と識別子の解決）。
//!
//! ダウンロード・検証・状態管理そのものは種別非依存の共有基盤
//! （`crate::model_download`）が持つ。このモジュールは「whisper としてどのモデルを選べるか」
//! だけを定義する（議事録要約 LLM のカタログは別モジュールが同じ形で持つ想定）。
//!
//! 使用モデルは設定画面のカタログ（`CATALOG`）から選べる。選択後の取得・進捗表示の挙動は
//! `crate::model_download` の doc を参照。

use crate::model_download::ModelSpec;

/// ログに出す種別。`Downloading the Whisper speech model Small (about 465 MB)` のように使う。
///
/// カタログ単位の属性をエントリごとに複写している（揃っていることは下のテストが見る）。
/// 種別が 3 つ以上になって文言が揺れるようなら、`ModelKind` の enum か
/// `struct ModelCatalog { kind, specs }` へ寄せる。
const KIND: &str = "Whisper speech";

/// 選べるモデルの一覧（小さい順）。設定画面の ComboBox はこの順で並ぶため、
/// モデルを足すときはここへ 1 エントリ追加するだけでよい。
///
/// URL・SHA-256 は HuggingFace（whisper.cpp 公式配布）の LFS メタデータより。
/// モデルを追加・差し替えるときは URL と SHA-256 を必ずペアで更新する。
pub const CATALOG: &[ModelSpec] = &[
    ModelSpec {
        kind: KIND,
        id: "tiny",
        display_name: "Tiny",
        description: "fastest, lowest accuracy",
        size_bytes: 77_691_713,
        filename: "ggml-tiny.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-tiny.bin",
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    },
    ModelSpec {
        kind: KIND,
        id: "base",
        display_name: "Base",
        description: "fast, basic accuracy",
        size_bytes: 147_951_465,
        filename: "ggml-base.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
    },
    ModelSpec {
        kind: KIND,
        id: "small",
        display_name: "Small",
        description: "balanced speed and accuracy",
        size_bytes: 487_601_967,
        filename: "ggml-small.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    },
    ModelSpec {
        kind: KIND,
        id: "medium",
        display_name: "Medium",
        description: "high accuracy, slower",
        size_bytes: 1_533_763_059,
        filename: "ggml-medium.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-medium.bin",
        sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    },
    ModelSpec {
        kind: KIND,
        id: "large-v3-turbo",
        display_name: "Large v3 Turbo",
        description: "high accuracy, faster than Large",
        size_bytes: 1_624_555_275,
        filename: "ggml-large-v3-turbo.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo.bin",
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    },
    ModelSpec {
        kind: KIND,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_download::{DownloadStatus, ModelDownloader};

    #[test]
    fn catalog_is_consistent() {
        // 種別を問わない検査（ID・ファイル名の重複、SHA-256 の形式、サイズ）は共有基盤の
        // 正を呼ぶ。ここには whisper 固有の条件だけ書く。
        crate::model_download::catalog_checks::assert_valid(CATALOG);

        assert!(spec_for(DEFAULT_MODEL_ID).is_some());
        for spec in CATALOG {
            // 配布元の URL は保存ファイル名で終わる（追加・差し替え時の取り違えを検知する）。
            assert!(
                spec.url.ends_with(spec.filename),
                "url mismatch for {}",
                spec.id
            );
            // 種別はログの見分けに使うので、カタログ内で揃っていること。
            assert_eq!(spec.kind, KIND, "unexpected kind for {}", spec.id);
        }
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
        let spec = default_spec();
        let path = downloader
            .ensure_model(spec)
            .expect("the model should download and verify");
        assert!(path.is_file());
        assert_eq!(downloader.status_of(spec), DownloadStatus::Downloaded);
    }

    #[test]
    fn model_index_resolves_known_and_falls_back() {
        assert_eq!(model_index("tiny"), 0);
        assert_eq!(CATALOG[model_index(DEFAULT_MODEL_ID)].id, DEFAULT_MODEL_ID);
        // カタログ外は既定モデルの位置へ。
        assert_eq!(CATALOG[model_index("no-such-model")].id, DEFAULT_MODEL_ID);
    }
}
