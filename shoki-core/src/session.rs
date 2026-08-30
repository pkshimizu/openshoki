//! 録音セッションの**事実**と、そこから決まる表示（#188）。
//!
//! 走査そのもの（`read_dir` して `stat` する）は shell の `recordings::list_sessions` が行い、
//! ここには**測り終わった結果**だけが入る。`view_*`（#188） がこの型を読んで一覧の行と
//! 詳細ヘッダを組むので、core 側に置いてある。
//!
//! # ファイル名は置かない
//!
//! **core にファイル名（`mic.mp3` などのリテラル）を置かない**。名前とパスの組み立ては shell
//! 側に閉じる。
//!
//! ここを破ると静かに壊れる: `clippy.toml` が塞いでいるのは `std::fs` の**呼び出し**で、
//! `PathBuf` を組み立てるだけのコードは通る。だから「core にファイル名の写しができ、名前が
//! 変わっても core が追わない」形は CI に一度も引っかからない。
//!
//! なお shell 側は既に写しを複数持っている（`recorder` / `system_audio` / `mixdown` が書く名前を、
//! 読む側の `recordings` と `transcript` がそれぞれ写している）。**この doc が約束しているのは
//! 「core には置かない」ことだけ**で、shell 側の写しを 1 つに畳む話ではない。

use std::path::PathBuf;
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};

/// 一覧の行と詳細ヘッダの時刻・日付（`14:02` / `Aug 10, 2026`）と、その 1 行版
/// （`Aug 10, 2026 · 14:02`）。
///
/// **3 つを揃える**——左右で日時の形が違うと、同じ録音を見ていることが読み取りにくい。1 行版は
/// 上の 2 つの連結なので、片方だけ変えると黙ってずれる。**ずれたら落ちるように**テストで
/// 縛ってある（`the_one_line_form_is_the_two_parts_joined`）。
const DISPLAY_TIME_FORMAT: &str = "%H:%M";
const DISPLAY_DATE_FORMAT: &str = "%b %-d, %Y";
/// 日付と時刻を 1 行に並べる形（`Aug 10, 2026 · 14:02`）。議事録の出典行が使う。
pub const DISPLAY_DATETIME_FORMAT: &str = "%b %-d, %Y · %H:%M";

/// セグメントの話者（どの音源の文字起こしか）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Speaker {
    Mic,
    System,
}

impl Speaker {
    /// 話者の英語ラベル。UI の話者バッジに出すほか、**議事録要約のプロンプトに渡す
    /// トランスクリプトの話者表記も兼ねる**（`src/summarize.rs` の `MINUTES_SYSTEM_*` /
    /// `NOTES_SYSTEM_*` が `Mic` / `System` という文字列を前提に書かれている）。
    /// 表記を変えるときはプロンプト側も同時に直すこと。
    pub fn label(self) -> &'static str {
        match self {
            Speaker::Mic => "Mic",
            Speaker::System => "System",
        }
    }
}

/// 走査で分かった、そのセッションに**何が在るか**（#188）。
///
/// **構造体で渡す**——`RecordingSession::new` に真偽値を 5 つ並べると、渡し違えても通る
/// （`docs/rules/coding-conventions.md`）。名前付きのフィールドなら位置で取り違えられない。
///
/// 長さ（`duration`）はここに持たない。**組み上がってから測る**ため——測る音源の選び方は
/// 組み上がった `RecordingSession` から決まる（shell の `duration_source`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskFacts {
    /// マイク音源が在るか。
    pub has_mic: bool,
    /// システム音源が在るか。
    pub has_system: bool,
    /// 録音後生成のミックス音声が在るか（両音源セッションの再生に使う）。
    pub has_mix: bool,
    /// 文字起こしが在るか（音源のどれか 1 つでも）。
    pub has_transcript: bool,
    /// 議事録が在るか。中身の妥当性は見ない（表示側が破損・空を縮退させる）。
    pub has_summary: bool,
}

/// 一覧に出す録音セッション 1 件。
///
/// **`Debug` は derive しない**: 保持しているのはフルパスで、`{:?}` でログへ出すとユーザー名が
/// 漏れる（`docs/rules/security.md`。ログに出すのはファイル名だけ）。`atomic_replace::PartFile`
/// と同じ理由だが、あちらは `Debug` を付けない側に倒している——こちらはテストの `assert_eq!` が
/// 要るので、**ファイル名だけ出す `Debug`** を手で書く。
#[derive(Clone, PartialEq, Eq)]
pub struct RecordingSession {
    /// 日時（ディレクトリ名からパース）。並び順と、表示用の組み立て（`display_time` /
    /// `display_date` / `group_heading`）の両方がここから決まる。
    ///
    /// **公開しない**——表示を日時から導く、という約束を破る書き込みを外から入れさせない
    /// （組み立ては `new`）。
    datetime: NaiveDateTime,
    /// セッションディレクトリの絶対/相対パス。
    ///
    /// **識別子であって表示用ではない**。フルパスはユーザー名を含むので、ログにも画面にも
    /// アクセシビリティラベルにも出さない（`docs/rules/security.md`）。出すのはファイル名だけ。
    pub dir: PathBuf,
    /// `DiskFacts::has_mic` と同じ。
    pub has_mic: bool,
    /// `DiskFacts::has_system` と同じ。
    pub has_system: bool,
    /// `DiskFacts::has_mix` と同じ。
    pub has_mix: bool,
    /// `DiskFacts::has_transcript` と同じ。
    pub has_transcript: bool,
    /// `DiskFacts::has_summary` と同じ。
    pub has_summary: bool,
    /// 録音の長さ（#162）。**音源のサイズから割り出した見積もり**（shell の `duration_source` /
    /// `duration_from_size`）。1 秒未満・読めないときは `None` で、一覧には段ごと出さない。
    ///
    /// **走査した時点のスナップショット**。録音中のセッションを一覧に出すと、その長さは止まった
    /// まま（かつ実際より短いまま）になる——一覧を作り直すのはウィンドウを開いたときだけ。
    pub duration: Option<Duration>,
}

impl std::fmt::Debug for RecordingSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // **分解してから組む**。フィールドを足すとここが割れるので、**載せ忘れ**が黙って通らない
        // （`src/slint_map.rs` の写像と同じ理由）。
        //
        // 分解が守るのはそこまで。`dir` をそのまま載せればフルパスは出るし、コンパイルも通る
        // ——そちらを止めているのは `debug_shows_the_file_name_not_the_whole_path`。
        let Self {
            datetime,
            dir,
            has_mic,
            has_system,
            has_mix,
            has_transcript,
            has_summary,
            duration,
        } = self;
        f.debug_struct("RecordingSession")
            .field("datetime", datetime)
            // **ファイル名だけ**（上の doc）。名前が取れないパス（末尾が `..` など）は伏せる。
            .field("dir", &dir.file_name())
            .field("has_mic", has_mic)
            .field("has_system", has_system)
            .field("has_mix", has_mix)
            .field("has_transcript", has_transcript)
            .field("has_summary", has_summary)
            .field("duration", duration)
            .finish()
    }
}

impl RecordingSession {
    /// 走査で分かった事実から組み立てる。長さは**組み上がってから**入れる（`DiskFacts` の doc）。
    pub fn new(datetime: NaiveDateTime, dir: PathBuf, facts: DiskFacts) -> Self {
        let DiskFacts {
            has_mic,
            has_system,
            has_mix,
            has_transcript,
            has_summary,
        } = facts;
        Self {
            datetime,
            dir,
            has_mic,
            has_system,
            has_mix,
            has_transcript,
            has_summary,
            duration: None,
        }
    }

    /// `RowKey` に載せるための日時（#188）。
    ///
    /// **表示には使わない**（`display_time` / `display_date` / `group_heading` を通すこと）。
    /// キーが要るのは「同じ行の表示が変わったか」を見るためで、そこには整形前の値が要る。
    pub(crate) fn started_for_key(&self) -> NaiveDateTime {
        self.datetime
    }

    /// 一覧の並び順（**新しい順**。同時刻はディレクトリ名で安定させる）。
    ///
    /// **生の日時を外へ出さず、順序そのものを渡す**。値で出すと「表示は `display_*` を通す」と
    /// いう約束が doc だけの守りになる（整形を呼び出し側で書けてしまい、同じ日時が画面ごとに
    /// 違う形で出る）。並べたいだけなら順序で足りる。
    pub fn newest_first(&self, other: &Self) -> std::cmp::Ordering {
        other
            .datetime
            .cmp(&self.datetime)
            .then_with(|| self.dir.cmp(&other.dir))
    }

    /// 録音した日（見出しのまとまりの判定に使う。**文言ではなく日付で比べる**ため）。
    pub fn date(&self) -> NaiveDate {
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
    ///
    /// **今日がいつかは引数で受ける**（#188）。現在時刻は外から来る事実で、core では取れない
    /// （`shoki-core/clippy.toml`）。
    pub fn group_heading(&self, now: NaiveDateTime) -> String {
        let days = (now.date() - self.datetime.date()).num_days();
        match days {
            0 => "Today".to_owned(),
            1 => "Yesterday".to_owned(),
            _ => self.display_date(),
        }
    }

    /// このセッションに在る音源（#175）。**文字起こしが揃っているかの判定に使う**——在る音源
    /// ごとに、読めて最後まで読み切った JSON があるかを見る（`transcript::load_transcript`）。
    ///
    /// **「在る音源」を言う場所はここ 1 つ**。ここが音源を取り落とすと、欠けた文字起こしが
    /// 完成品として画面に出る（`transcript::sources_shortfall` が数える対象そのもの）。
    pub fn speakers(&self) -> Vec<Speaker> {
        let mut speakers = Vec::new();
        if self.has_mic {
            speakers.push(Speaker::Mic);
        }
        if self.has_system {
            speakers.push(Speaker::System);
        }
        speakers
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

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(year, month, day)
            .expect("a real date")
            .and_hms_opt(hour, minute, 0)
            .expect("a real time")
    }

    /// 見出しが**相対の語になるのは今日と昨日だけ**。幅のある語（「先週」）を出さない、という
    /// 判断をここで固定する（境界がいつ変わるかを読み手に推測させない）。
    #[test]
    fn only_today_and_yesterday_get_a_relative_heading() {
        let now = at(2026, 8, 30, 9, 0);
        let heading = |day| {
            RecordingSession::new(
                at(2026, 8, day, 14, 2),
                PathBuf::new(),
                DiskFacts::default(),
            )
            .group_heading(now)
        };
        assert_eq!(heading(30), "Today");
        assert_eq!(heading(29), "Yesterday");
        assert_eq!(heading(28), "Aug 28, 2026");
        assert_eq!(heading(1), "Aug 1, 2026");
    }

    /// 見出しは**日付で比べる**（経過時間ではない）。
    ///
    /// **2 つの実装で答えが割れる取り合わせを選ぶ**。同じ日の朝と夜で比べても、日付差でも
    /// 経過時間でも 0 日になって区別がつかない——15 分前の録音が日をまたいでいる、という
    /// 実際に起きる場面なら、経過時間で比べる実装は "Today" と答えて落ちる
    /// （`docs/rules/testing.md` の「見ているように読めるが inert」）。
    #[test]
    fn the_heading_compares_dates_not_times() {
        let now = at(2026, 8, 30, 0, 5);
        let just_before_midnight = RecordingSession::new(
            at(2026, 8, 29, 23, 50),
            PathBuf::new(),
            DiskFacts::default(),
        );
        assert_eq!(just_before_midnight.group_heading(now), "Yesterday");
    }

    /// 1 行版（`DISPLAY_DATETIME_FORMAT`）は、日付と時刻を `·` で繋いだものと**同じ**。
    ///
    /// 詳細ヘッダは `display_date()` と `display_time()` を自分で繋ぎ、議事録の出典行は 1 行版を
    /// 使う。片方だけ形を変えると、同じ録音の日時が画面ごとに違って出る——doc の約束では守れない
    /// ので、ずれたら落ちるようにここで縛る。
    #[test]
    fn the_one_line_form_is_the_two_parts_joined() {
        let dt = at(2026, 8, 10, 14, 2);
        let session = RecordingSession::new(dt, PathBuf::new(), DiskFacts::default());
        assert_eq!(
            dt.format(DISPLAY_DATETIME_FORMAT).to_string(),
            format!("{} · {}", session.display_date(), session.display_time())
        );
    }

    /// **フルパスを `{:?}` で漏らさない**（`docs/rules/security.md`）。`dir` はユーザー名を含む
    /// ので、出すのはファイル名だけ。derive に戻した瞬間に落ちる。
    #[test]
    fn debug_shows_the_file_name_not_the_whole_path() {
        let session = RecordingSession::new(
            at(2026, 8, 10, 14, 2),
            PathBuf::from("/Users/someone/Recordings/20260810-140200"),
            DiskFacts::default(),
        );
        let shown = format!("{session:?}");
        assert!(
            shown.contains("20260810-140200"),
            "the file name is still there, got {shown}"
        );
        assert!(
            !shown.contains("someone"),
            "the rest of the path must not be shown, got {shown}"
        );
    }

    /// 在る音源だけを返す。**ここが取り落とすと、欠けた文字起こしが完成品として画面に出る**。
    #[test]
    fn the_sources_a_session_has_come_from_the_files_it_has() {
        let with = |has_mic, has_system| {
            RecordingSession::new(
                at(2026, 8, 10, 14, 2),
                PathBuf::new(),
                DiskFacts {
                    has_mic,
                    has_system,
                    ..DiskFacts::default()
                },
            )
        };
        assert_eq!(
            with(true, true).speakers(),
            vec![Speaker::Mic, Speaker::System]
        );
        assert_eq!(with(true, false).speakers(), vec![Speaker::Mic]);
        assert_eq!(with(false, true).speakers(), vec![Speaker::System]);
        assert!(with(false, false).speakers().is_empty());
    }

    /// 音源の語は**在る音源と 1 対 1**。`speakers()` と食い違うと、詳細ヘッダと削除の確認が
    /// 「在る」と言った音源を文字起こしが投げない、という形で割れる。
    #[test]
    fn the_source_word_matches_the_sources_that_are_there() {
        let with = |has_mic, has_system| {
            RecordingSession::new(
                at(2026, 8, 10, 14, 2),
                PathBuf::new(),
                DiskFacts {
                    has_mic,
                    has_system,
                    ..DiskFacts::default()
                },
            )
        };
        assert_eq!(with(true, true).source_summary(), "Mic + system");
        assert_eq!(with(true, false).source_summary(), "Mic only");
        assert_eq!(with(false, true).source_summary(), "System only");
        assert_eq!(with(false, false).source_summary(), "No audio");
    }
}
