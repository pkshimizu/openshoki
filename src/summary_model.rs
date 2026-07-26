//! 議事録要約に使うオンデバイス LLM のカタログ（選べるモデルの一覧と識別子の解決）。
//!
//! `src/whisper_model.rs` と同じ形で、ダウンロード・検証・状態管理そのものは種別非依存の
//! 共有基盤（`crate::model_download`）に任せ、このモジュールは「要約にどのモデルを選べるか」
//! だけを定義する。
//!
//! 収録した 2 つと既定の選び方は #78 の検証で確定した
//! （`docs/plans/done/20260722-meeting-minutes-summary.md` の「採用モデル」）。

use crate::model_download::ModelSpec;

/// ログに出す種別。`Downloading the Summary LLM model Qwen2.5 7B Instruct (about 4.4 GB)` の
/// ように使う（whisper 側の `Whisper speech` と見分けるための語）。
const KIND: &str = "Summary LLM";

/// 選べるモデルの一覧（小さい順）。設定画面の選択 UI はまだ無く（issue 未起票。#81 は
/// Recordings ウィンドウでの表示・手動生成なので別件）、現状は設定 `summary_model` の
/// 手編集で切り替える。
///
/// URL・SHA-256 は HuggingFace の LFS メタデータより。モデルを追加・差し替えるときは
/// URL と SHA-256 を必ずペアで更新する。
///
/// 7B は**公式 Qwen リポジトリの GGUF が分割配布**（`-00001-of-00002`）で、共有基盤の
/// 「単一ファイルのみ」という制約（`ModelSpec` の doc）に抵触する。単一ファイルで再配布して
/// いる bartowski から取る。3B は公式が単一ファイルなので公式から取る。
pub const CATALOG: &[ModelSpec] = &[
    ModelSpec {
        kind: KIND,
        id: "qwen2.5-3b-instruct-q4-k-m",
        display_name: "Qwen2.5 3B Instruct",
        description: "faster and lighter, but can invent details",
        size_bytes: 2_104_932_768,
        filename: "qwen2.5-3b-instruct-q4_k_m.gguf",
        url: "https://huggingface.co/Qwen/Qwen2.5-3B-Instruct-GGUF/resolve/main/qwen2.5-3b-instruct-q4_k_m.gguf",
        sha256: "626b4a6678b86442240e33df819e00132d3ba7dddfe1cdc4fbb18e0a9615c62d",
    },
    ModelSpec {
        kind: KIND,
        id: "qwen2.5-7b-instruct-q4-k-m",
        display_name: "Qwen2.5 7B Instruct",
        description: "more faithful, about twice the time and memory",
        size_bytes: 4_683_074_240,
        filename: "Qwen2.5-7B-Instruct-Q4_K_M.gguf",
        url: "https://huggingface.co/bartowski/Qwen2.5-7B-Instruct-GGUF/resolve/main/Qwen2.5-7B-Instruct-Q4_K_M.gguf",
        sha256: "65b8fcd92af6b4fefa935c625d1ac27ea29dcb6ee14589c55a8f115ceaaa1423",
    },
];

/// 既定モデルの識別子（7B）。3B は半分の時間・メモリで済むが、#78 の検証で**日本語の
/// アクションアイテムに曜日を捏造し、期限欄にプロンプト由来の雛形語を漏らした**。議事録では
/// 捏造の害が取りこぼしより大きいという判断で 7B を既定にしている。
pub const DEFAULT_MODEL_ID: &str = "qwen2.5-7b-instruct-q4-k-m";

/// 識別子からカタログのエントリを引く。
pub fn spec_for(id: &str) -> Option<&'static ModelSpec> {
    CATALOG.iter().find(|spec| spec.id == id)
}

/// 既定モデルのエントリ。
pub fn default_spec() -> &'static ModelSpec {
    spec_for(DEFAULT_MODEL_ID).expect("the default model id is always in the catalog")
}

/// 設定値からエントリを引く。カタログ外の手編集値は既定モデルへフォールバックする
/// （利用側と設定画面の表示で同じ解決をするための単一の口）。
pub fn spec_or_default(id: &str) -> &'static ModelSpec {
    spec_for(id).unwrap_or_else(default_spec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        // 種別を問わない検査（ID・ファイル名の重複、SHA-256 の形式、サイズ）は共有基盤の
        // 正を呼ぶ。ここには要約 LLM 固有の条件だけ書く。
        crate::model_download::catalog_checks::assert_valid(CATALOG);

        assert!(spec_for(DEFAULT_MODEL_ID).is_some());
        for spec in CATALOG {
            // 配布元の URL は保存ファイル名で終わる（追加・差し替え時の取り違えを検知する）。
            assert!(
                spec.url.ends_with(spec.filename),
                "url mismatch for {}",
                spec.id
            );
            // GGUF 以外（分割配布の断片など）を取り違えて載せない。
            assert!(
                spec.filename.ends_with(".gguf"),
                "not a gguf file: {}",
                spec.filename
            );
            // 種別はログの見分けに使うので、カタログ内で揃っていること。
            assert_eq!(spec.kind, KIND, "unexpected kind for {}", spec.id);
        }
    }

    #[test]
    fn unknown_id_has_no_spec() {
        // カタログ外の手編集値は解決できない（利用側は `spec_or_default` で既定へ丸める）。
        assert!(spec_for("no-such-model").is_none());
    }
}
