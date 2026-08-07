//! 録音セッションの探索。設定の保存先（`recording_dir`）配下にある `<%Y%m%d-%H%M%S>` 形式の
//! セッションディレクトリを列挙し、含まれる音源（mic / system）・文字起こし・議事録要約の
//! 有無を調べて新しい順に並べる。Recordings ウィンドウの一覧表示に使う。
//!
//! 読むだけのモジュールではない: 一覧に出たセッションに取り残された一時ファイルの回収
//! （`spawn_session_part_sweep`）も持つ。**消す**のはこの 1 箇所だけなので、範囲と時期の判断は
//! そこの doc に集約する。
//!
//! `recording_dir` は設定（手編集されうる信頼境界外）由来で、無関係なファイル・ディレクトリが
//! 混じりうる。名前が日時形式でないもの・音源ファイルが 1 つも無いものは安全にスキップし、
//! 走査失敗（ディレクトリ不在など）でも空一覧を返してアプリを落とさない。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// 取り残された一時ファイルと見なす、最終更新からの経過時間（`spawn_session_part_sweep`）。
///
/// セッション側の一時ファイルは**寿命が短い**: 書き手（`mixdown::normalize_if_quiet` と
/// `summarize::write_summary`）はどちらも中身を先にメモリで作り、一時ファイルへは 1 回書いて
/// すぐ rename するので、通常は 1 秒に満たない（モデル取得の `STALE_MODEL_PART_AGE` が 3 時間
/// なのは、受信そのものが数十分かかることに由来する別の理由）。時計のずれ・mtime の粒度ぶんの
/// 余裕もこの 1 時間に含む。
///
/// 走っている書き込みを消さない保証そのものは mtime が更新され続けることで足りる
/// （`atomic_replace::sweep_orphaned_parts` の doc）。ただし**書き出し中のスリープ**を挟むと
/// 経過は伸びうる（mtime は止まり「今」が進む）。その場合に失われるのは走っていた書き出し
/// 1 回ぶんで、rename が失敗してログに出るだけ（成果物は元のまま壊れない）。
const STALE_SESSION_PART_AGE: Duration = Duration::from_secs(60 * 60);

/// 掃除する一時ファイルの宛先名（`spawn_session_part_sweep`）。セッションディレクトリは**ユーザーが
/// 中身を置ける場所**なので、アプリが `PartFile` 経由で書く宛先だけに絞る（`mix.mp3` は
/// `PartFile` を通さず直接書くので入れない）。名前の由来は `atomic_replace::PartScope`。
const SWEPT_PART_DESTS: &[&str] = &[MIC_MP3, SYSTEM_MP3, crate::summarize::SUMMARY_FILENAME];

/// セッションディレクトリ名の日時フォーマット（`main.rs` の録音開始時の命名と一致させること）。
const DIR_DATETIME_FORMAT: &str = "%Y%m%d-%H%M%S";
/// 一覧に表示する日時フォーマット（カンプに合わせて分まで）。
const DISPLAY_DATETIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// 1 つの録音セッション。ディレクトリと、含まれる音源・文字起こし・議事録要約の有無を持つ。
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
    /// 議事録要約（`summary.md`）があるか。中身の妥当性は見ない（表示側の `load_summary` が
    /// 破損・空を縮退させる）。
    pub has_summary: bool,
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
        let has_summary = dir.join(crate::summarize::SUMMARY_FILENAME).is_file();

        sessions.push(RecordingSession {
            datetime,
            display_datetime: datetime.format(DISPLAY_DATETIME_FORMAT).to_string(),
            dir,
            has_mic,
            has_system,
            has_mix,
            has_transcript,
            has_summary,
        });
    }

    // 新しい順（日時降順）。同時刻はディレクトリ名でも安定させる必要はないが、決定的にするため
    // パスで二次ソートする。
    sessions.sort_by(|a, b| b.datetime.cmp(&a.datetime).then_with(|| a.dir.cmp(&b.dir)));
    sessions
}

/// 一覧に出たセッションの直下に取り残された一時ファイル（`*.part.<pid>`）を回収する
/// （Recordings ウィンドウを開くたびに、`list_sessions` の結果を渡して呼ぶ）。
///
/// `PartFile` の Drop が走らない終わり方（`abort`・強制終了・電源喪失）で残ったものが対象。
/// 発話由来の派生物（正規化中の音声・議事録）なので、ユーザーが気づかないまま録音フォルダに
/// 残り続けないようにする（`docs/review-perspectives/security.md`）。古さの判定と、そこから
/// 来る限界（強制終了の直後は回収されない）は `atomic_replace::sweep_orphaned_parts` の doc。
///
/// **これがユーザーの保存先でファイルを消す唯一の経路**なので、範囲を 3 重に絞る:
///
/// 1. **時期**: ユーザーが Recordings ウィンドウを開いたときだけ（常駐の起動時には走らない。
///    #130 で「起動時にユーザーの選んだ保存先を走査するリスクは取らない」と見送った判断を、
///    「保存先を**丸ごと**走査しない」に狭めて保つ）。
/// 2. **場所**: `list_sessions` が返したセッション（日時形式の名前で音源を持つディレクトリ）の
///    直下だけ。保存先に置かれた無関係なフォルダには触れない（`recording_dir` は手編集されうる
///    設定由来）。セッションディレクトリ自体が**シンボリックリンク**なら掃除しない:
///    `list_sessions` の `is_dir()` はリンクを辿るので、日時形式の名前のリンクを置かれると
///    掃除が録音ツリーの外へ出る。一覧に出す（読むだけ）のは許し、消す側は辿らない。
/// 3. **名前**: `SWEPT_PART_DESTS`（アプリが `PartFile` で書く宛先）の一時ファイルだけ。
///    これで、たまたま `*.part.<数字>` という名前のユーザーのファイル（分割書庫など）には
///    原理的に触れない。
///
/// 消したファイルは**ゴミ箱へは送らない**（セッション削除との差）。成果物ではなく書き損じの
/// 断片で、復元する価値が無いため。
///
/// 走査は**バックグラウンドスレッドで行い、呼び出し側は待たない**（表示には使わない副作用な
/// ので待つ理由が無く、セッション数に比例する I/O を UI スレッドへ載せない。
/// `docs/rules/performance.md`）。戻り値のハンドルはそのための同期点で、本番は捨てて構わない
/// （掃除が終わる前にアプリが終了しても、次に開いたときにまた走る）。
///
/// 音源が 1 つも無いディレクトリは一覧に出ないので掃除されない。一時ファイルを作る経路は
/// どれも音源が在る状態でしか走る（録音そのものは `PartFile` を通さず直接書く）ので、作られた
/// 時点では必ず一覧に出る。あとで音源だけ消されたセッションの残骸は回収されないが、残るのは
/// 1〜2 ファイルなので許容する。
pub fn spawn_session_part_sweep(
    sessions: &[RecordingSession],
    now: SystemTime,
) -> Option<std::thread::JoinHandle<()>> {
    let dirs: Vec<PathBuf> = sessions.iter().map(|session| session.dir.clone()).collect();
    match std::thread::Builder::new()
        .name("session-part-sweep".into())
        .spawn(move || sweep_session_dirs(&dirs, now))
    {
        Ok(handle) => Some(handle),
        Err(err) => {
            // 掃除は「次に開いたときにまた走る」ので、失敗しても機能に影響しない。
            eprintln!(
                "Skipping the cleanup of leftover temporary files because a thread could not be started: {err}"
            );
            None
        }
    }
}

/// 掃除の本体（`spawn_session_part_sweep` が別スレッドで呼ぶ）。範囲の判断はそちらの doc。
fn sweep_session_dirs(dirs: &[PathBuf], now: SystemTime) {
    for dir in dirs {
        // リンクを辿らずにディレクトリであることを確かめる（上の 2.）。
        if !dir.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
            continue;
        }
        crate::atomic_replace::sweep_orphaned_parts(
            dir,
            now,
            STALE_SESSION_PART_AGE,
            crate::atomic_replace::PartScope::Dests(SWEPT_PART_DESTS),
        );
    }
}

/// セッションディレクトリ名（`%Y%m%d-%H%M%S`）を日時としてパースする。形式外なら `None`。
fn parse_session_datetime(name: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(name, DIR_DATETIME_FORMAT).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        RecordingSession, STALE_SESSION_PART_AGE, list_sessions, parse_session_datetime,
        spawn_session_part_sweep,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// 掃除を走らせて終わるまで待つ（本番は待たないが、テストは決定的にしたいのでハンドルを
    /// join する）。
    fn sweep_and_wait(sessions: &[RecordingSession], now: SystemTime) {
        spawn_session_part_sweep(sessions, now)
            .expect("the sweep thread should start in test")
            .join()
            .expect("the sweep thread should not panic");
    }

    /// テスト用に、指定ディレクトリ配下へセッションディレクトリと空の音源/文字起こしファイルを作る。
    fn make_session(root: &Path, name: &str, files: &[&str]) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("creating the session dir succeeds in test");
        for f in files {
            fs::write(dir.join(f), b"").expect("writing the placeholder file succeeds in test");
        }
    }

    fn unique_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shoki-recordings-{tag}-{}", std::process::id()))
    }

    /// 取り残された一時ファイルを、一覧に出たセッションの直下からだけ回収する。
    ///
    /// 対象は `*.part.<数字>` かつ十分に古いものだけで、ユーザーのファイル・書き込み中の
    /// 一時ファイル・**一覧に出ないディレクトリ**（日時形式でない名前、音源が無いもの）は
    /// 触らない。#130 の「ユーザーのフォルダを丸ごと走査しない」を保つ線引きなので、
    /// 範囲もここで固定する。
    #[test]
    fn sweep_session_parts_only_touches_listed_sessions() {
        let root = unique_root("sweep");
        let _ = fs::remove_dir_all(&root);
        make_session(&root, "20260628-143025", &["mic.mp3"]);
        make_session(&root, "20260628-150000", &["system.mp3"]);
        // 一覧に出ないディレクトリ: 日時形式でない名前と、音源が 1 つも無いもの。
        make_session(&root, "notes", &["mic.mp3"]);
        make_session(&root, "20260628-160000", &["mic.json"]);

        let listed_old = root.join("20260628-143025").join("mic.mp3.part.123");
        let listed_second = root.join("20260628-150000").join("system.mp3.part.456");
        let listed_other_name = root.join("20260628-143025").join("archive.zip.part.1");
        let listed_user_file = root.join("20260628-143025").join("notes.part.txt");
        let unlisted_by_name = root.join("notes").join("mic.mp3.part.789");
        let unlisted_by_content = root.join("20260628-160000").join("summary.md.part.789");
        // 保存先のルート自体（セッションではない）に置かれたもの。
        let in_the_root = root.join("mic.mp3.part.999");
        for path in [
            &listed_old,
            &listed_second,
            &listed_other_name,
            &listed_user_file,
            &unlisted_by_name,
            &unlisted_by_content,
            &in_the_root,
        ] {
            fs::write(path, b"x").expect("writing the fixture succeeds in test");
        }

        // 実ファイルの mtime は「今」なので、判定の現在時刻を未来へずらして経過を作る
        // （`atomic_replace` のテストと同じ流儀）。
        let sessions = list_sessions(&root);
        sweep_and_wait(&sessions, SystemTime::now() + STALE_SESSION_PART_AGE);
        assert!(!listed_old.exists(), "a leftover part file must be removed");
        assert!(
            !listed_second.exists(),
            "every listed session must be swept, not just the first"
        );
        assert!(
            listed_other_name.exists(),
            "a part file whose destination we never write must be kept"
        );
        assert!(
            listed_user_file.exists(),
            "a user file that merely contains .part must be kept"
        );
        assert!(
            unlisted_by_name.exists(),
            "a folder that is not a session must not be swept"
        );
        assert!(
            unlisted_by_content.exists(),
            "a folder without audio is not listed, so it must not be swept"
        );
        assert!(
            in_the_root.exists(),
            "the recording folder itself must not be swept"
        );

        // 書き込み中（mtime が新しい）の一時ファイルは、古さの条件を満たさないので残る。
        // 閾値の桁も固定する: 1 時間より短い経過では消えない。
        fs::write(&listed_old, b"x").expect("writing the fixture succeeds in test");
        sweep_and_wait(&sessions, SystemTime::now());
        sweep_and_wait(
            &sessions,
            SystemTime::now() + STALE_SESSION_PART_AGE - Duration::from_secs(60),
        );
        assert!(
            listed_old.exists(),
            "a part file that is still being written must be kept"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// セッションディレクトリ自体がシンボリックリンクなら掃除しない（掃除が録音ツリーの外へ
    /// 出ないための線引き。`list_sessions` の `is_dir()` はリンクを辿るので一覧には出る）。
    #[test]
    #[cfg(unix)]
    fn sweep_does_not_follow_a_symlinked_session() {
        let root = unique_root("sweep-symlink");
        let _ = fs::remove_dir_all(&root);
        // 録音ツリーの外にあるセッションらしいディレクトリ（リンク先）。
        let outside = unique_root("sweep-symlink-outside");
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).expect("creating the outside dir succeeds in test");
        fs::write(outside.join("mic.mp3"), b"").expect("writing the fixture succeeds in test");
        let victim = outside.join("mic.mp3.part.123");
        fs::write(&victim, b"x").expect("writing the fixture succeeds in test");

        fs::create_dir_all(&root).expect("creating the root succeeds in test");
        let link = root.join("20260628-143025");
        std::os::unix::fs::symlink(&outside, &link).expect("creating the symlink succeeds in test");

        let sessions = list_sessions(&root);
        assert_eq!(
            sessions.len(),
            1,
            "the symlink is listed (is_dir follows it)"
        );
        sweep_and_wait(&sessions, SystemTime::now() + STALE_SESSION_PART_AGE);
        assert!(
            victim.exists(),
            "a part file behind a symlinked session must not be swept"
        );

        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
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
            &["mic.mp3", "system.mp3", "mic.json", "summary.md"],
        );
        make_session(&root, "20260628-110500", &["mic.mp3"]);
        make_session(&root, "20260627-164200", &["system.mp3"]);

        let sessions = list_sessions(&root);
        assert_eq!(sessions.len(), 3);
        // 新しい順。
        assert_eq!(sessions[0].display_datetime, "2026-06-28 14:30");
        assert_eq!(sessions[1].display_datetime, "2026-06-28 11:05");
        assert_eq!(sessions[2].display_datetime, "2026-06-27 16:42");
        // 音源・文字起こし・議事録要約の判定とサマリー。
        assert!(sessions[0].has_mic && sessions[0].has_system && sessions[0].has_transcript);
        assert!(sessions[0].has_summary);
        assert_eq!(sessions[0].source_summary(), "Mic + System");
        assert_eq!(sessions[1].source_summary(), "Mic only");
        assert!(!sessions[1].has_transcript);
        // 文字起こしも要約も無いセッション（要約の有無は独立に判定される）。
        assert!(!sessions[1].has_summary);
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
