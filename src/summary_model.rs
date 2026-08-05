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

/// 選べるモデルの一覧（小さい順）。設定画面の ComboBox はこの順で並ぶため、モデルを足すときは
/// ここへ 1 エントリ追加するだけでよい（whisper 側と同じ）。
///
/// `description` には **4 分の会議での所要時間とピーク RSS の目安**を先に置く。数 GB の
/// ダウンロードと数十秒・数 GB の実行コストが選択で決まるので、選ぶ前に読めるようにする
/// （元プランの「設定画面では所要時間とメモリの目安を添えること」）。
/// 数値の正は #78 の計測（`docs/plans/done/20260722-meeting-minutes-summary.md` の表。
/// 3B が 25.1s / 3.72GB、7B が 53.5s / 8.18GB）で、秒は繰り上げ・GB は小数第 1 位。
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
        description: "25 s and 3.7 GB of memory for a 4-min meeting, but can invent details",
        size_bytes: 2_104_932_768,
        filename: "qwen2.5-3b-instruct-q4_k_m.gguf",
        url: "https://huggingface.co/Qwen/Qwen2.5-3B-Instruct-GGUF/resolve/main/qwen2.5-3b-instruct-q4_k_m.gguf",
        sha256: "626b4a6678b86442240e33df819e00132d3ba7dddfe1cdc4fbb18e0a9615c62d",
    },
    ModelSpec {
        kind: KIND,
        id: "qwen2.5-7b-instruct-q4-k-m",
        display_name: "Qwen2.5 7B Instruct",
        description: "54 s and 8.2 GB of memory for a 4-min meeting, more faithful",
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

/// 識別子 → カタログ内インデックス。カタログ外（手編集値）は既定モデルの位置へ
/// フォールバックする（解決は共有基盤の `catalog_index` が正。whisper 側と同じ挙動になる）。
pub fn model_index(id: &str) -> usize {
    crate::model_download::catalog_index(CATALOG, id, DEFAULT_MODEL_ID)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_consistent() {
        // 種別を問わない検査（ID・ファイル名の重複、SHA-256 の形式、サイズ、既定 ID の存在）は
        // 共有基盤の正を呼ぶ。ここには要約 LLM 固有の条件だけ書く。
        crate::model_download::catalog_checks::assert_valid(CATALOG, DEFAULT_MODEL_ID);

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
            // 設定画面は description をそのまま選択肢に出す。所要時間とメモリの目安を
            // 書き忘れると、ユーザーが選ぶ材料（#119 の受け入れ条件）を失うので形で見る。
            assert!(
                spec.description.contains(" s and ")
                    && spec.description.contains(" of memory for a "),
                "description should carry the time and memory estimate: {}",
                spec.id
            );
        }
    }

    #[test]
    fn model_index_resolves_known_and_falls_back() {
        assert_eq!(model_index("qwen2.5-3b-instruct-q4-k-m"), 0);
        assert_eq!(CATALOG[model_index(DEFAULT_MODEL_ID)].id, DEFAULT_MODEL_ID);
        // カタログ外は既定モデルの位置へ（先頭ではなく既定の位置に落ちることを見る）。
        assert_eq!(CATALOG[model_index("no-such-model")].id, DEFAULT_MODEL_ID);
    }

    #[test]
    fn unknown_id_has_no_spec() {
        // カタログ外の手編集値は解決できない（利用側は `spec_or_default` で既定へ丸める）。
        assert!(spec_for("no-such-model").is_none());
    }
}
