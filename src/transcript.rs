//! 録音セッションの文字起こし（`mic.json` / `system.json`）を読み込み、話者ラベル付きの 1 本の
//! トランスクリプトへマージする。
//!
//! JSON は #30（`src/transcribe.rs`）が生成する。本モジュールは**読むだけ**で、生成には関与しない。
//! 再生はミックスの単一タイムライン（`src/player.rs` / `src/mixdown.rs`）なので、各音源の秒はその
//! まま共通タイムラインに対応する。話者は JSON 内の値でなくファイル名（`mic.json` / `system.json`）で
//! 区別する。追加フィールド（`language` 等）は無視して読める（`deny_unknown_fields` を付けない）。
//!
//! 文字起こしが未生成・欠落・破損のセッションは空のトランスクリプトとして扱い、落とさない
//! （`docs/rules/error-handling.md`）。呼び出し側は空なら「Not Transcribed Yet」を表示する。

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;

/// 文字起こし JSON のファイル名。`transcribe.rs` が `<音源名>.json` で保存する名前と一致させること。
const MIC_JSON: &str = "mic.json";
const SYSTEM_JSON: &str = "system.json";

/// 読み込む文字起こし JSON のサイズ上限。保存先ディレクトリの JSON は手で置換されうる信頼境界外の
/// 入力なので、想定外の巨大ファイルでメモリを大量確保しない保険（`docs/rules/security.md`）。
/// 実際の文字起こしは長時間録音でも高々数 MB。
const MAX_TRANSCRIPT_BYTES: u64 = 32 * 1024 * 1024;

/// セグメントの話者（どの音源の文字起こしか）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Mic,
    System,
}

impl Speaker {
    /// UI の話者バッジに出す英語ラベル。
    pub fn label(self) -> &'static str {
        match self {
            Speaker::Mic => "Mic",
            Speaker::System => "System",
        }
    }
}

/// マージ済みトランスクリプトの 1 セグメント。時刻はセッション開始からの秒（共通タイムライン）。
/// JSON の `end` は現状使わないため保持しない（ハイライトは次のセグメント開始まで継続する仕様）。
#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptSegment {
    pub start_secs: f64,
    pub text: String,
    pub speaker: Speaker,
}

impl TranscriptSegment {
    /// 開始秒を `Duration` にする。信頼境界外の JSON 由来なので、不正値（負・非有限・巨大）でも
    /// パニックせず `ZERO` へ丸める。表示（時刻ラベル）とシークの双方がこれを使い、丸め方針の
    /// 食い違いを防ぐ。
    pub fn start_duration(&self) -> Duration {
        Duration::try_from_secs_f64(self.start_secs).unwrap_or(Duration::ZERO)
    }
}

/// JSON 読み取り用。#30 の出力のうち本ビューが使う `segments` だけを取り、他フィールドは無視する。
#[derive(Deserialize)]
struct TranscriptFile {
    #[serde(default)]
    segments: Vec<RawSegment>,
}

/// JSON の 1 セグメント。`text` は欠けていても既定値で読めるようにする（前方互換）。
/// `end` は使わないため読まない（未知フィールドとして無視される）。
#[derive(Deserialize)]
struct RawSegment {
    start: f64,
    #[serde(default)]
    text: String,
}

/// セッションの `mic.json` / `system.json` を読み、話者ラベル付きで開始秒の昇順にマージする。
/// 欠落・破損の音源はスキップ（その音源のセグメントは無し）。両方無ければ空を返す。
pub fn load_transcript(session_dir: &Path) -> Vec<TranscriptSegment> {
    let mut segments = load_one(&session_dir.join(MIC_JSON), Speaker::Mic);
    segments.extend(load_one(&session_dir.join(SYSTEM_JSON), Speaker::System));
    // 開始秒で安定ソート（同秒は mic→system の追加順を保つ）。NaN は来ない想定だが total_cmp で安全に。
    segments.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));
    segments
}

/// 1 つの文字起こし JSON を読む。欠落（未生成）は静かに、読み取り失敗・過大・破損はログして、
/// いずれも空を返す（縮退。アプリは落とさない）。
///
/// ログにはどちらのファイルで起きたかが分かるようファイル名（`mic.json` 等）だけを含める
/// （フルパス＝保存先や発話内容の機微情報は出さない）。
fn load_one(path: &Path, speaker: Speaker) -> Vec<TranscriptSegment> {
    use std::io::Read;

    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        // 未生成（ファイルが無い）は正常な縮退。ログもしない。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        // 権限・I/O エラーなどは異常なので、調査の手掛かりを残す。
        Err(err) => {
            eprintln!("Skipping the transcript {name} because it could not be opened: {err}");
            return Vec::new();
        }
    };
    // 信頼境界外の入力（手で置換されうる）なので、開いたハンドルの fstat で通常ファイルであることを
    // 確認し（FIFO 等は読み終わらないことがある）、サイズ上限は読み込みそのものに掛ける
    // （事前の metadata 判定だけでは差し替えに追従できない。`docs/rules/security.md`）。
    if let Ok(meta) = file.metadata()
        && !meta.is_file()
    {
        eprintln!("Skipping the transcript {name} because it is not a regular file");
        return Vec::new();
    }
    let mut limited = file.take(MAX_TRANSCRIPT_BYTES + 1);
    let mut text = String::new();
    if let Err(err) = limited.read_to_string(&mut text) {
        eprintln!("Skipping the transcript {name} because it could not be read: {err}");
        return Vec::new();
    }
    // 上限＋1 バイトまで読み切った（limit が尽きた）なら上限超過。
    if limited.limit() == 0 {
        eprintln!("Skipping the transcript {name} because it is too large");
        return Vec::new();
    }
    let parsed: TranscriptFile = match serde_json::from_str(&text) {
        Ok(parsed) => parsed,
        Err(err) => {
            // エラーの Display は JSON 中の値（＝発話テキスト）を含みうるため出さず、位置だけログする
            // （録音由来の機微データをログへ漏らさない。`docs/rules/security.md`）。
            eprintln!(
                "Skipping the transcript {name} because it could not be parsed (line {}, column {})",
                err.line(),
                err.column()
            );
            return Vec::new();
        }
    };
    parsed
        .segments
        .into_iter()
        .map(|s| TranscriptSegment {
            start_secs: s.start,
            text: s.text,
            speaker,
        })
        .collect()
}

/// 再生位置に対応するセグメントの index を返す（開始秒が再生位置以下である最後のセグメント）。
/// まだどのセグメントも始まっていない（位置が先頭セグメントより前）・空なら `None`。
/// `load_transcript` が開始秒の昇順を保証しているので二分探索で引く（再生 tick ごとに呼ばれる）。
pub fn current_index(segments: &[TranscriptSegment], pos_secs: f64) -> Option<usize> {
    let count = segments.partition_point(|seg| seg.start_secs <= pos_secs);
    count.checked_sub(1)
}

#[cfg(test)]
mod tests {
    use super::{Speaker, current_index, load_transcript};
    use std::fs;
    use std::path::PathBuf;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("openshoki-transcript-{tag}-{}", std::process::id()))
    }

    #[test]
    fn load_transcript_merges_both_sources_in_time_order() {
        let dir = unique_dir("merge");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // 追加フィールド（language など）が混じっても読めること・時刻順マージを確認する。
        fs::write(
            dir.join("mic.json"),
            r#"{"source":"mic","language":"en","segments":[
                {"start":0.0,"end":3.0,"text":"hello"},
                {"start":6.0,"end":8.0,"text":"world"}
            ]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("system.json"),
            r#"{"segments":[{"start":3.0,"end":5.0,"text":"reply"}]}"#,
        )
        .unwrap();

        let segments = load_transcript(&dir);
        assert_eq!(segments.len(), 3);
        // 開始秒の昇順にマージされ、話者はファイル名で決まる。
        assert_eq!(segments[0].speaker, Speaker::Mic);
        assert_eq!(segments[0].text, "hello");
        assert_eq!(segments[1].speaker, Speaker::System);
        assert_eq!(segments[1].text, "reply");
        assert_eq!(segments[2].speaker, Speaker::Mic);
        assert_eq!(segments[2].text, "world");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_transcript_skips_missing_and_broken_json() {
        let dir = unique_dir("broken");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        // system.json のみ・かつ壊れた JSON → 空（落ちない）。mic.json は欠落。
        fs::write(dir.join("system.json"), b"{ this is not json").unwrap();
        assert!(load_transcript(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_transcript_empty_when_no_files() {
        let dir = unique_dir("none").join("missing");
        assert!(load_transcript(&dir).is_empty());
    }

    #[test]
    fn current_index_tracks_playback_position() {
        let dir = unique_dir("index");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("mic.json"),
            r#"{"segments":[
                {"start":1.0,"end":3.0,"text":"a"},
                {"start":3.0,"end":6.0,"text":"b"},
                {"start":6.0,"end":9.0,"text":"c"}
            ]}"#,
        )
        .unwrap();
        let segments = load_transcript(&dir);

        // 先頭セグメントより前は None、開始ちょうどからそのセグメントに対応する。
        assert_eq!(current_index(&segments, 0.5), None);
        assert_eq!(current_index(&segments, 1.0), Some(0));
        assert_eq!(current_index(&segments, 2.5), Some(0));
        assert_eq!(current_index(&segments, 3.0), Some(1));
        assert_eq!(current_index(&segments, 100.0), Some(2));
        // 空なら None。
        assert_eq!(current_index(&[], 1.0), None);

        let _ = fs::remove_dir_all(&dir);
    }
}
