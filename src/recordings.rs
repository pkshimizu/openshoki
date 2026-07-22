//! 録音セッションの探索。設定の保存先（`recording_dir`）配下にある `<%Y%m%d-%H%M%S>` 形式の
//! セッションディレクトリを列挙し、含まれる音源（mic / system）・文字起こしの有無を調べて
//! 新しい順に並べる。Recordings ウィンドウの一覧表示に使う。
//!
//! `recording_dir` は設定（手編集されうる信頼境界外）由来で、無関係なファイル・ディレクトリが
//! 混じりうる。名前が日時形式でないもの・音源ファイルが 1 つも無いものは安全にスキップし、
//! 走査失敗（ディレクトリ不在など）でも空一覧を返してアプリを落とさない。

use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;

/// 音源・文字起こしのファイル名。`recorder.rs`（`mic.mp3`）・`system_audio.rs`（`system.mp3`）・
/// `transcribe.rs`（`<音源名>.json`）の固定名と一致させること（`docs/CONTEXT.md` の
/// セッションディレクトリ規約）。片方だけ変えると一覧の判定がずれる。
const MIC_MP3: &str = "mic.mp3";
const SYSTEM_MP3: &str = "system.mp3";
const MIC_JSON: &str = "mic.json";
const SYSTEM_JSON: &str = "system.json";
/// 録音後に生成されるミックス音声（`src/mixdown.rs`。両音源セッションの再生対象）。名前は
/// `mixdown::MIX_FILENAME` と一致させること。
const MIX_MP3: &str = "mix.mp3";

/// セッションディレクトリ名の日時フォーマット（`main.rs` の録音開始時の命名と一致させること）。
const DIR_DATETIME_FORMAT: &str = "%Y%m%d-%H%M%S";
/// 一覧に表示する日時フォーマット（カンプに合わせて分まで）。
const DISPLAY_DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// 1 つの録音セッション。ディレクトリと、含まれる音源・文字起こしの有無を持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSession {
    /// ソート用の日時（ディレクトリ名からパース）。表示には `display_datetime` を使う。
    datetime: NaiveDateTime,
    /// セッションディレクトリの絶対/相対パス。
    pub dir: PathBuf,
    /// 一覧表示用の日時文字列（例 `2026-06-28 14:30`）。
    pub display_datetime: String,
    /// `mic.mp3` があるか。
    pub has_mic: bool,
    /// `system.mp3` があるか。
    pub has_system: bool,
    /// 録音後生成の `mix.mp3` があるか（両音源セッションの再生に使う）。
    pub has_mix: bool,
    /// 文字起こし（`mic.json` / `system.json` のいずれか）があるか。
    pub has_transcript: bool,
}

impl RecordingSession {
    /// 再生対象ファイルのパス。両音源のセッションは録音後生成の `mix.mp3`（まだ無ければ再生不可で
    /// `None`）、単一音源のセッションはその音源ファイルそのもの。音源なしは `None`。
    ///
    /// 両音源で `mix.mp3` を再生対象にするのは、選択時に毎回デコード＋ミックスすると UI が固まる
    /// ため（重い処理は録音直後の生成へ移す。`src/mixdown.rs`）。
    pub fn playback_path(&self) -> Option<PathBuf> {
        match (self.has_mic, self.has_system) {
            (true, true) => self.has_mix.then(|| self.dir.join(MIX_MP3)),
            (true, false) => Some(self.dir.join(MIC_MP3)),
            (false, true) => Some(self.dir.join(SYSTEM_MP3)),
            (false, false) => None,
        }
    }

    /// 再生できるか（再生対象ファイルが定まるか）。両音源で `mix.mp3` 未生成のときは false。
    pub fn is_playable(&self) -> bool {
        self.playback_path().is_some()
    }

    /// 文字起こしの対象となる音源ファイル（存在する `mic.mp3` / `system.mp3`）。
    /// 手動再実行（Recordings ウィンドウの Transcribe ボタン）の投入対象に使う。
    pub fn audio_source_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if self.has_mic {
            paths.push(self.dir.join(MIC_MP3));
        }
        if self.has_system {
            paths.push(self.dir.join(SYSTEM_MP3));
        }
        paths
    }

    /// 含まれる音源を表す英語サマリー（右ペインのヘッダ表示用）。
    pub fn source_summary(&self) -> &'static str {
        match (self.has_mic, self.has_system) {
            (true, true) => "Mic + System",
            (true, false) => "Mic only",
            (false, true) => "System only",
            // 音源なしのセッションは一覧に含めない（`list_sessions` がスキップ）ため通常起きない。
            (false, false) => "No audio",
        }
    }
}

/// `recording_dir` を走査して録音セッションを新しい順（日時降順）で返す。
///
/// ディレクトリが無い・読めないときは空一覧を返す（縮退。ログを残す）。名前が日時形式でない
/// エントリ、ディレクトリでないエントリ、音源が 1 つも無いセッションはスキップする。
pub fn list_sessions(recording_dir: &Path) -> Vec<RecordingSession> {
    let entries = match std::fs::read_dir(recording_dir) {
        Ok(entries) => entries,
        Err(err) => {
            // 保存先が未作成（まだ一度も録音していない）なども含む。落とさず空一覧にする。
            eprintln!("Skipping the recordings scan because the folder could not be read: {err}");
            return Vec::new();
        }
    };

    let mut sessions: Vec<RecordingSession> = Vec::new();
    for entry in entries.flatten() {
        let dir = entry.path();
        // ディレクトリ以外（ファイル等）は対象外。
        if !dir.is_dir() {
            continue;
        }
        let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(datetime) = parse_session_datetime(name) else {
            continue; // 日時形式でない名前はスキップ。
        };

        let has_mic = dir.join(MIC_MP3).is_file();
        let has_system = dir.join(SYSTEM_MP3).is_file();
        // 音源が 1 つも無いディレクトリ（欠落・作りかけ）は一覧に出さない。
        if !has_mic && !has_system {
            continue;
        }
        let has_mix = dir.join(MIX_MP3).is_file();
        let has_transcript = dir.join(MIC_JSON).is_file() || dir.join(SYSTEM_JSON).is_file();

        sessions.push(RecordingSession {
            datetime,
            display_datetime: datetime.format(DISPLAY_DATETIME_FORMAT).to_string(),
            dir,
            has_mic,
            has_system,
            has_mix,
            has_transcript,
        });
    }

    // 新しい順（日時降順）。同時刻はディレクトリ名でも安定させる必要はないが、決定的にするため
    // パスで二次ソートする。
    sessions.sort_by(|a, b| b.datetime.cmp(&a.datetime).then_with(|| a.dir.cmp(&b.dir)));
    sessions
}

/// セッションディレクトリ名（`%Y%m%d-%H%M%S`）を日時としてパースする。形式外なら `None`。
fn parse_session_datetime(name: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(name, DIR_DATETIME_FORMAT).ok()
}

#[cfg(test)]
mod tests {
    use super::{RecordingSession, list_sessions, parse_session_datetime};
    use std::fs;
    use std::path::{Path, PathBuf};

    /// テスト用に、指定ディレクトリ配下へセッションディレクトリと空の音源/文字起こしファイルを作る。
    fn make_session(root: &Path, name: &str, files: &[&str]) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("creating the session dir succeeds in test");
        for f in files {
            fs::write(dir.join(f), b"").expect("writing the placeholder file succeeds in test");
        }
    }

    fn unique_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("openshoki-recordings-{tag}-{}", std::process::id()))
    }

    #[test]
    fn parse_session_datetime_accepts_valid_and_rejects_others() {
        assert!(parse_session_datetime("20260628-143025").is_some());
        // 形式外はすべて None（信頼境界外の無関係な名前を弾く）。
        assert!(parse_session_datetime("recordings").is_none());
        assert!(parse_session_datetime("2026-06-28").is_none());
        assert!(parse_session_datetime("20260628").is_none());
        assert!(parse_session_datetime("").is_none());
    }

    #[test]
    fn list_sessions_orders_newest_first_and_reports_sources() {
        let root = unique_root("order");
        let _ = fs::remove_dir_all(&root);
        make_session(
            &root,
            "20260628-143025",
            &["mic.mp3", "system.mp3", "mic.json"],
        );
        make_session(&root, "20260628-110500", &["mic.mp3"]);
        make_session(&root, "20260627-164200", &["system.mp3"]);

        let sessions = list_sessions(&root);
        assert_eq!(sessions.len(), 3);
        // 新しい順。
        assert_eq!(sessions[0].display_datetime, "2026-06-28 14:30");
        assert_eq!(sessions[1].display_datetime, "2026-06-28 11:05");
        assert_eq!(sessions[2].display_datetime, "2026-06-27 16:42");
        // 音源・文字起こしの判定とサマリー。
        assert!(sessions[0].has_mic && sessions[0].has_system && sessions[0].has_transcript);
        assert_eq!(sessions[0].source_summary(), "Mic + System");
        assert_eq!(sessions[1].source_summary(), "Mic only");
        assert!(!sessions[1].has_transcript);
        assert_eq!(sessions[2].source_summary(), "System only");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_sessions_skips_invalid_names_and_empty_sessions() {
        let root = unique_root("skip");
        let _ = fs::remove_dir_all(&root);
        make_session(&root, "20260628-143025", &["mic.mp3"]); // 有効
        make_session(&root, "not-a-session", &["mic.mp3"]); // 名前が日時形式でない
        make_session(&root, "20260628-110500", &["notes.txt"]); // 音源が無い
        fs::create_dir_all(&root).ok();
        fs::write(root.join("20260628-090000"), b"").ok(); // ディレクトリでないファイル

        let sessions = list_sessions(&root);
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].display_datetime, "2026-06-28 14:30");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn playback_path_prefers_mix_for_dual_source_else_single_source() {
        let root = unique_root("playback");
        let _ = fs::remove_dir_all(&root);
        // 両音源＋mix → mix.mp3 が再生対象。
        make_session(
            &root,
            "20260628-143025",
            &["mic.mp3", "system.mp3", "mix.mp3"],
        );
        // 両音源だが mix 未生成 → 再生不可。
        make_session(&root, "20260628-110500", &["mic.mp3", "system.mp3"]);
        // 単一音源（mic のみ）→ その音源が再生対象。
        make_session(&root, "20260627-164200", &["mic.mp3"]);

        let sessions = list_sessions(&root);
        assert_eq!(sessions.len(), 3);
        // 新しい順。
        assert_eq!(
            sessions[0].playback_path(),
            Some(root.join("20260628-143025").join("mix.mp3"))
        );
        assert!(sessions[0].is_playable());
        // 両音源で mix が無ければ再生不可（選択時にその場ミックスはしない）。
        assert_eq!(sessions[1].playback_path(), None);
        assert!(!sessions[1].is_playable());
        // 単一音源はその音源ファイルを直接再生する。
        assert_eq!(
            sessions[2].playback_path(),
            Some(root.join("20260627-164200").join("mic.mp3"))
        );
        assert!(sessions[2].is_playable());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn list_sessions_returns_empty_for_missing_dir() {
        // 一度も録音していない等でディレクトリが無くても落ちず空一覧。
        let root = unique_root("missing").join("does-not-exist");
        let sessions: Vec<RecordingSession> = list_sessions(&root);
        assert!(sessions.is_empty());
    }
}
