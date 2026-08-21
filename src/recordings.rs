//! 録音セッションの探索。設定の保存先（`recording_dir`）配下にある `<%Y%m%d-%H%M%S>` 形式の
//! セッションディレクトリを列挙し、含まれる音源（mic / system）・文字起こし・議事録要約の
//! 有無を調べて新しい順に並べる。Recordings ウィンドウの一覧表示に使う。
//!
//! 読むだけのモジュールではない: 一覧に出たセッションに取り残された一時ファイルの回収
//! （`spawn_session_part_sweep`）も持つ。保存先のファイルを消す経路はほかにもあるが
//! （書き損じの後始末＝`atomic_replace::PartFile` / `recorder::discard_partial_recording`、
//! セッションごとゴミ箱へ移す削除＝`main`）、**自分が作ったと確認せず名前だけを頼りに走査して
//! 消す**のはここだけなので、範囲と時期の判断はそこの doc に集約する。
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

/// 掃除する一時ファイルの宛先名（`spawn_session_part_sweep`）。セッションディレクトリは
/// **ユーザーが中身を置ける場所**なので、アプリが `PartFile` 経由で書く宛先だけに絞る
/// （`mix.mp3` は `PartFile` を通さず直接書くので入れない）。絞る理由は
/// `atomic_replace::PartScope`。
///
/// 参照するのは**そのファイルを書くモジュールの定数**（この一覧の判定用に置いた `MIC_MP3` などの
/// 写しではない）。一時ファイルの名前は宛先の名前から作られる（`atomic_replace::PartFile`）ので、
/// 書き手が名前を変えたら掃除も自動で追い、取りこぼしが静かに生まれない。
///
/// システム音源は macOS 限定モジュール（`system_audio`）にあるので、他 OS では対象から落ちる
/// （そこにはシステム音源を書く経路も無い）。
const SWEPT_PART_DESTS: &[&str] = &[
    crate::recorder::MIC_FILENAME,
    #[cfg(target_os = "macos")]
    crate::system_audio::SYSTEM_FILENAME,
    crate::summarize::SUMMARY_FILENAME,
];

/// セッションディレクトリ名の日時フォーマット（`main.rs` の録音開始時の命名と一致させること）。
const DIR_DATETIME_FORMAT: &str = "%Y%m%d-%H%M%S";
/// 一覧に表示する日時フォーマット（カンプに合わせて分まで）。
/// 一覧の行と詳細ヘッダの時刻・日付（`14:02` / `Aug 10, 2026`）。**同じ組み合わせを両方で
/// 使う**——左右で日時の形が違うと、同じ録音を見ていることが読み取りにくい。
const DISPLAY_TIME_FORMAT: &str = "%H:%M";
const DISPLAY_DATE_FORMAT: &str = "%b %-d, %Y";

/// 1 つの録音セッション。ディレクトリと、含まれる音源・文字起こし・議事録要約の有無を持つ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordingSession {
    /// 日時（ディレクトリ名からパース）。並び順と、表示用の組み立て（`display_time` /
    /// `display_date` / `group_heading`）の両方がここから決まる。
    datetime: NaiveDateTime,
    /// セッションディレクトリの絶対/相対パス。
    pub dir: PathBuf,
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
    /// テスト用に、日時だけを持つセッションを作る（ファイルの有無は呼び出し側で足す）。
    ///
    /// **表示の組み立て**（見出し・行の文言）は日時とファイルの有無だけで決まるので、実ディスクを
    /// 用意せずに検証できる。
    #[cfg(test)]
    pub fn for_test(datetime: NaiveDateTime) -> Self {
        Self {
            datetime,
            dir: PathBuf::new(),
            has_mic: false,
            has_system: false,
            has_mix: false,
            has_transcript: false,
            has_summary: false,
        }
    }

    /// 録音した日（見出しのまとまりの判定に使う。**文言ではなく日付で比べる**ため）。
    pub fn date(&self) -> chrono::NaiveDate {
        self.datetime.date()
    }

    /// 一覧の行に出す時刻（`14:02`）。同じ日の中ではこれで見分けるので、行の中でいちばん大きく出す。
    pub fn display_time(&self) -> String {
        self.datetime.format(DISPLAY_TIME_FORMAT).to_string()
    }

    /// 一覧の行に出す日付（`Aug 10, 2026`）。見出しと重なるが、スクロールで見出しが流れても
    /// どの日か分かるように行にも残す。
    pub fn display_date(&self) -> String {
        self.datetime.format(DISPLAY_DATE_FORMAT).to_string()
    }

    /// 日付のまとまりの見出し（`Today` / `Yesterday` / `Aug 5, 2026`）。
    ///
    /// **相対の語を出すのは今日と昨日だけ**。「先週」のような幅のある語は、境界がいつ変わるのか
    /// （週の始まりは日曜か月曜か）を読み手が推測することになるので使わない。
    pub fn group_heading(&self, now: NaiveDateTime) -> String {
        let days = (now.date() - self.datetime.date()).num_days();
        match days {
            0 => "Today".to_owned(),
            1 => "Yesterday".to_owned(),
            _ => self.display_date(),
        }
    }

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
            (true, true) => "Mic + system",
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
/// **名前だけを頼りに走査して消す唯一の経路**なので（ほかの削除経路はモジュール doc）、
/// 範囲を 3 重に絞る:
///
/// 1. **時期**: ユーザーが Recordings ウィンドウを開いたときだけ（常駐の起動時には走らない。
///    #130 で「起動時にユーザーの選んだ保存先を走査するリスクは取らない」と見送った判断を、
///    「保存先を**丸ごと**走査しない」に狭めて保つ）。
/// 2. **場所**: `list_sessions` が返したセッション（日時形式の名前で音源を持つディレクトリ）の
///    直下だけ。保存先に置かれた無関係なフォルダには触れない（`recording_dir` は手編集されうる
///    設定由来）。セッションディレクトリ自体が**シンボリックリンク**なら掃除しない
///    （`sweep_orphaned_parts` が弾く）: `list_sessions` の `is_dir()` はリンクを辿るので、
///    日時形式の名前のリンクを置かれると掃除が録音ツリーの外へ出る。一覧に出す（読むだけ）の
///    は許し、消す側は辿らない。
/// 3. **名前**: `SWEPT_PART_DESTS`（アプリが `PartFile` で書く宛先）の一時ファイルだけ。
///    これで、たまたま `*.part.<数字>` という名前のユーザーのファイル（分割書庫など）には
///    原理的に触れない。
///
/// 消したファイルは**ゴミ箱へは送らない**（セッション削除との差）。成果物ではなく書き損じの
/// 断片で、復元する価値が無いため。
///
/// 走査は**バックグラウンドスレッドで行い、完了は待たない**（表示には使わない副作用なので待つ
/// 理由が無く、セッション数に比例する I/O を UI スレッドへ載せない。
/// `docs/rules/performance.md`）。掃除が終わる前にアプリが終了しても、次に開いたときにまた走る。
///
/// `now` を引数で取るのは、経過を作れるようにしてテストの継ぎ目にするため
/// （`sweep_orphaned_parts` と同じ流儀。本番の呼び出しは 1 箇所で常に `SystemTime::now()`）。
///
/// 音源が 1 つも無いディレクトリは一覧に出ないので掃除されない。一時ファイルを作る経路は
/// どれも音源が在る状態でしか走らない（録音そのものは `PartFile` を通さず直接書く）ので、
/// 作られた時点では必ず一覧に出る。あとで音源だけ消されたセッションの残骸は回収されないが、残るのは
/// 1〜2 ファイルなので許容する。
pub fn spawn_session_part_sweep(sessions: &[RecordingSession], now: SystemTime) {
    let dirs = session_dirs(sessions);
    let spawned = std::thread::Builder::new()
        .name("session-part-sweep".into())
        .spawn(move || sweep_session_dirs(&dirs, now));
    if let Err(err) = spawned {
        // 掃除は「次に開いたときにまた走る」ので、失敗しても機能に影響しない。
        eprintln!(
            "Skipping the cleanup of leftover temporary files because the sweep thread failed to start: {err}"
        );
    }
}

/// 掃除の対象になるディレクトリ（一覧に出たセッション）。
fn session_dirs(sessions: &[RecordingSession]) -> Vec<PathBuf> {
    sessions.iter().map(|session| session.dir.clone()).collect()
}

/// 掃除の本体（`spawn_session_part_sweep` が別スレッドで呼ぶ）。範囲の判断はそちらの doc。
fn sweep_session_dirs(dirs: &[PathBuf], now: SystemTime) {
    for dir in dirs {
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
        session_dirs, spawn_session_part_sweep, sweep_session_dirs,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// 掃除を同期で走らせる（本番は `spawn_session_part_sweep` が別スレッドで呼ぶ。テストは
    /// 決定的にしたいので本体を直接呼ぶ）。対象の組み立ても本番と同じ関数を通す。
    fn sweep(sessions: &[RecordingSession], now: SystemTime) {
        sweep_session_dirs(&session_dirs(sessions), now);
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
    fn session_part_sweep_only_touches_listed_sessions() {
        let root = unique_root("sweep");
        let _ = fs::remove_dir_all(&root);
        make_session(&root, "20260628-143025", &["mic.mp3"]);
        make_session(&root, "20260628-150000", &["system.mp3"]);
        // 一覧に出ないディレクトリ: 日時形式でない名前と、音源が 1 つも無いもの。
        make_session(&root, "notes", &["mic.mp3"]);
        make_session(&root, "20260628-160000", &["mic.json"]);

        let listed_old = root.join("20260628-143025").join("mic.mp3.part.123");
        let listed_summary = root.join("20260628-143025").join("summary.md.part.123");
        let listed_second = root.join("20260628-150000").join("system.mp3.part.456");
        let listed_other_name = root.join("20260628-143025").join("archive.zip.part.1");
        let listed_user_file = root.join("20260628-143025").join("notes.part.txt");
        let unlisted_by_name = root.join("notes").join("mic.mp3.part.789");
        let unlisted_by_content = root.join("20260628-160000").join("summary.md.part.789");
        // 保存先のルート自体（セッションではない）に置かれたもの。
        let in_the_root = root.join("mic.mp3.part.999");
        for path in [
            &listed_old,
            &listed_summary,
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
        sweep(&sessions, SystemTime::now() + STALE_SESSION_PART_AGE);
        assert!(!listed_old.exists(), "a leftover part file must be removed");
        assert!(
            !listed_summary.exists(),
            "the summary is written through PartFile too, so its leftovers must be removed"
        );
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
        fs::write(&listed_old, b"x").expect("writing the fixture succeeds in test");
        sweep(&sessions, SystemTime::now());
        assert!(
            listed_old.exists(),
            "a part file that is still being written must be kept"
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// 閾値の桁を上下から固定する。相対値のテスト（`now + AGE`）では、縮める変異も伸ばす変異も
    /// 素通りしてしまう。下限は書き込み中のファイルを消さない余裕（時計のずれ・mtime の粒度）、
    /// 上限は「開いたときに回収される」が実質的に機能する範囲。
    #[test]
    fn the_stale_threshold_stays_in_the_intended_order_of_magnitude() {
        assert!(STALE_SESSION_PART_AGE >= Duration::from_secs(30 * 60));
        assert!(STALE_SESSION_PART_AGE <= Duration::from_secs(3 * 60 * 60));
    }

    /// 一覧の判定に使う写しが、実際にそのファイルを書くモジュールの定数と一致していること
    /// （`MIC_MP3` などの doc が「一致させること」と言っている不変条件をここで固定する。
    /// ずれると一覧の判定と掃除の対象が静かに食い違う）。
    #[test]
    fn the_listed_filenames_match_the_modules_that_write_them() {
        assert_eq!(super::MIC_MP3, crate::recorder::MIC_FILENAME);
        assert_eq!(super::MIX_MP3, crate::mixdown::MIX_FILENAME);
        #[cfg(target_os = "macos")]
        assert_eq!(super::SYSTEM_MP3, crate::system_audio::SYSTEM_FILENAME);
    }

    /// 公開の入口（別スレッドで走らせる形）でも古い一時ファイルが消えること。上限つきポーリング
    /// で待つ（`docs/rules/testing.md`。超えたときの第一容疑者はスレッドの起動失敗）。
    #[test]
    fn the_public_entry_point_sweeps_in_the_background() {
        let root = unique_root("sweep-spawn");
        let _ = fs::remove_dir_all(&root);
        make_session(&root, "20260628-143025", &["mic.mp3"]);
        let leftover = root.join("20260628-143025").join("mic.mp3.part.123");
        fs::write(&leftover, b"x").expect("writing the fixture succeeds in test");

        let sessions = list_sessions(&root);
        spawn_session_part_sweep(&sessions, SystemTime::now() + STALE_SESSION_PART_AGE);
        let mut swept = false;
        for _ in 0..600 {
            if !leftover.exists() {
                swept = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            swept,
            "the background sweep should remove the leftover within 6s (did the thread start?)"
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
        sweep(&sessions, SystemTime::now() + STALE_SESSION_PART_AGE);
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
        assert_eq!(sessions[0].display_date(), "Jun 28, 2026");
        assert_eq!(sessions[0].display_time(), "14:30");
        assert_eq!(sessions[1].display_time(), "11:05");
        assert_eq!(sessions[2].display_date(), "Jun 27, 2026");
        // 音源・文字起こし・議事録要約の判定とサマリー。
        assert!(sessions[0].has_mic && sessions[0].has_system && sessions[0].has_transcript);
        assert!(sessions[0].has_summary);
        assert_eq!(sessions[0].source_summary(), "Mic + system");
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
        assert_eq!(sessions[0].display_date(), "Jun 28, 2026");
        assert_eq!(sessions[0].display_time(), "14:30");

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
        assert!(sessions[0].playback_path().is_some());
        // 両音源で mix が無ければ再生不可（選択時にその場ミックスはしない）。
        assert_eq!(sessions[1].playback_path(), None);
        assert!(sessions[1].playback_path().is_none());
        // 単一音源はその音源ファイルを直接再生する。
        assert_eq!(
            sessions[2].playback_path(),
            Some(root.join("20260627-164200").join("mic.mp3"))
        );
        assert!(sessions[2].playback_path().is_some());

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
