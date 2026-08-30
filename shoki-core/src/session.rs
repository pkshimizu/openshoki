//! 録音セッションの**事実**と、そこから決まる表示（#188 の PR-3a）。
//!
//! 走査そのもの（`read_dir` して `stat` する）は shell の `recordings::list_sessions` が行い、
//! ここには**測り終わった結果**だけが入る。`view_*` がこの型を読んで一覧の行と詳細ヘッダを組む
//! ので、core 側に置く必要がある。
//!
//! # ディスクレイアウトは持たない
//!
//! ファイル名（`mic.mp3` / `system.mp3` / `mix.mp3` / `*.json` / `summary.md`）を**この層に
//! 持ち込まない**。名前を知っているのは書き手のモジュール（`recorder` / `mixdown` / `transcribe`
//! / `summarize`）で、パスを組むのは shell の仕事。
//!
//! ここを破ると静かに壊れる: `clippy.toml` が塞いでいるのは `std::fs` の**呼び出し**で、
//! `PathBuf` を組み立てるだけのコードは通る。だから「core にファイル名の写しができ、書き手が
//! 名前を変えても core が追わない」形は CI に一度も引っかからない。

use std::path::PathBuf;
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};

/// 一覧の行に出す時刻（`14:02`）。
const DISPLAY_TIME_FORMAT: &str = "%H:%M";
/// 一覧の行と見出しに出す日付（`Aug 10, 2026`）。
const DISPLAY_DATE_FORMAT: &str = "%b %-d, %Y";

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
    /// 録音の長さ（#162）。**音源のサイズから割り出した見積もり**（shell の `duration_source` /
    /// `duration_from_size`）。1 秒未満・読めないときは `None` で、一覧には段ごと出さない。
    ///
    /// **走査した時点のスナップショット**。録音中のセッションを一覧に出すと、その長さは止まった
    /// まま（かつ実際より短いまま）になる——一覧を作り直すのはウィンドウを開いたときだけ。
    pub duration: Option<Duration>,
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

    /// 録音した日時。**並び順に使う**（新しい順。同時刻はディレクトリ名で安定させる）。
    ///
    /// 表示に使うときは `display_time` / `display_date` / `group_heading` を通すこと——
    /// 整形を呼び出し側で書くと、同じ日時が画面ごとに違う形で出る。
    pub fn started(&self) -> NaiveDateTime {
        self.datetime
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

    /// 見出しは**日付で比べる**（時刻ではない）。同じ日の朝と夜が別のまとまりに割れない。
    #[test]
    fn the_heading_compares_dates_not_times() {
        let now = at(2026, 8, 30, 0, 5);
        let late = RecordingSession::new(
            at(2026, 8, 30, 23, 50),
            PathBuf::new(),
            DiskFacts::default(),
        );
        assert_eq!(late.group_heading(now), "Today");
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
