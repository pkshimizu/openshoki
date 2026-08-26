//! 録音セッションの文字起こし（`mic.json` / `system.json`）を読み込み、話者ラベル付きの 1 本の
//! トランスクリプトへマージする。
//!
//! JSON は #30（`src/transcribe.rs`）が生成する。本モジュールは**読むだけ**で、生成には関与しない。
//! 再生はミックスの単一タイムライン（`src/player.rs` / `src/mixdown.rs`）なので、各音源の秒はその
//! まま共通タイムラインに対応する。話者は JSON 内の値でなくファイル名（`mic.json` / `system.json`）で
//! 区別する。追加フィールド（`language` 等）は無視して読める（`deny_unknown_fields` を付けない）。
//!
//! 文字起こしが未生成・欠落・破損のセッションは空のトランスクリプトとして扱い、落とさない
//! （`docs/rules/error-handling.md`）。呼び出し側は空なら状態依存の空表示に落とす
//! （見出し・理由・次の操作の対応表は `reading_pane::TranscriptPane::message` が正。欠落・破損は状態
//! `Done` のままセグメントだけ空になるので、未実施とは違う文が出る）。

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

/// 文字起こしがありうる音源。**読む側は全部見る**（音源が消えても文字起こしは読ませる）ので、
/// 種類を足したらここに足す。
const ALL_SPEAKERS: [Speaker; 2] = [Speaker::Mic, Speaker::System];

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

    /// この音源の文字起こし JSON のファイル名。`transcribe.rs` が `<音源名>.json` で保存する
    /// 名前と一致させること。
    fn json_name(self) -> &'static str {
        match self {
            Speaker::Mic => MIC_JSON,
            Speaker::System => SYSTEM_JSON,
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
    /// 元になった音源の長さ（秒）。**どこまでの音源から作られたか**を表すので、途中で
    /// 読めなくなった音源から作った JSON では、その打ち切り位置になる（#164）。欠けている
    /// 古い JSON は 0 として読む（`stored_reach` が「分からない」に落とす）。
    #[serde(default)]
    duration_secs: f64,
    /// 音源を**最後まで読めたか**（#175）。`false` は `duration_secs` までで打ち切った途中結果。
    ///
    /// **欠けていても・型が違っても `true` に落とす**。保存先は手編集されうる信頼境界外なので、
    /// この 1 欄のせいで JSON 全体のパースが失敗し、セグメントごと消えるのを避ける
    /// （`docs/rules/error-handling.md` の「寛容にデシリアライズし既定へ丸める」）。
    ///
    /// **`true` は「デコーダがストリーム終端まで到達した」という意味**でしかない。壊れたパケットを
    /// 読み飛ばして中抜けした音源も `true` になる（`transcribe::decode_mp3_stream`。扱いは #176）。
    #[serde(
        default = "complete_by_default",
        deserialize_with = "deserialize_complete"
    )]
    complete: bool,
}

/// `complete` が欠けている JSON の既定。
///
/// **#164 から #175 の間に書かれた途中結果は取り逃す**——その頃の出力は欄を持たないまま
/// `complete:false` 相当を保存していた。まだ配布していない（`docs/CONTEXT.md` の配布の決定記録）
/// ので手元のデータにしか無く、やり直せば直る。
fn complete_by_default() -> bool {
    true
}

/// `complete` を寛容に読む。**欄が在って読めない値なら「最後まで読めていない」へ倒す**。
///
/// 欠落を `true` にするのは互換の根拠がある（欄が無い＝#164 以前の出力）が、**壊れた値には
/// その根拠が無い**。この機能は「欠けた文字起こしが完成品に見えるのを防ぐ」ためのものなので、
/// 分からないときは守りたい側へ倒す。
fn deserialize_complete<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(false))
}

/// JSON の 1 セグメント。`text` は欠けていても既定値で読めるようにする（前方互換）。
/// `end` は使わないため読まない（未知フィールドとして無視される）。
#[derive(Deserialize)]
struct RawSegment {
    start: f64,
    #[serde(default)]
    text: String,
}

/// セッションの文字起こし（#175）。**セグメントと「揃っているか」を 1 つの値で返す**——別々に
/// 読むと、片方だけ古い組み合わせを作れてしまう。
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub segments: Vec<TranscriptSegment>,
    /// **在る音源ぶんの文字起こしが、すべて最後まで読めているか**（#175）。
    ///
    /// 音源が 2 本あって片方の JSON が無いセッション（一方だけ失敗した・途中で止めた）も
    /// `false` になる。「読めた JSON がすべて `complete`」にすると、そこが `true` に化ける。
    ///
    /// 読めなかった JSON（欠落・破損・過大）も `false`。読めない以上「揃っている」とは言えない。
    pub complete: bool,
}

/// セッションの文字起こしを読み、話者ラベル付きで開始秒の昇順にマージする（#175）。
///
/// **本文は在る JSON を全部読む**（`read_all`）。**`sources` は「揃っているか」の判定にだけ
/// 使う**（`all_sources_are_complete`）。このセッションに在る音源を渡すこと——揃っているかは
/// 「在る音源ごとに、読めて最後まで読み切った JSON があるか」で決まる。
pub fn load_transcript(session_dir: &Path, sources: &[Speaker]) -> Transcript {
    let read = read_all(session_dir);
    let complete = all_sources_are_complete(&read, sources);
    Transcript {
        segments: merged_segments(read),
        complete,
    }
}

/// 文字起こしのセグメントだけを読む（**揃っているかは見ない**）。
///
/// 議事録の生成と検索が使う——どちらも本文しか要らない。揃っているかを判断したい呼び出し側は
/// `load_transcript` を使うこと（#175）。
///
/// **読み方は `load_transcript` と同じ 1 本**（`read_all` → `merged_segments`）。
/// **`load_transcript` へ空の `sources` を渡す形にはしない**——理由は
/// `all_sources_are_complete`。
pub fn load_segments(session_dir: &Path) -> Vec<TranscriptSegment> {
    merged_segments(read_all(session_dir))
}

/// 在りうる音源ぶんの JSON を読む。**在る音源ではなく全部読む**——音源を消して文字起こしだけ
/// 残したセッションでも、読めるものは読ませるため（絞ると「検索では当たるのに開くと出て
/// こない」という食い違いになる）。
fn read_all(session_dir: &Path) -> Vec<(Speaker, Option<TranscriptFile>)> {
    ALL_SPEAKERS
        .iter()
        .map(|&speaker| {
            (
                speaker,
                read_guarded(&session_dir.join(speaker.json_name())),
            )
        })
        .collect()
}

/// 読めたぶんを話者ラベル付きで開始秒の昇順にマージする。
fn merged_segments(read: Vec<(Speaker, Option<TranscriptFile>)>) -> Vec<TranscriptSegment> {
    let mut segments: Vec<TranscriptSegment> = read
        .into_iter()
        .filter_map(|(speaker, parsed)| Some((speaker, parsed?)))
        .flat_map(|(speaker, parsed)| to_segments(parsed, speaker))
        .collect();
    sort_by_start(&mut segments);
    segments
}

/// 在る音源ぶんが、すべて読めて最後まで読み切っているか（#175）。
///
/// **読めなかった JSON も揃っていない側**（読めない以上そうとは言えない）。
///
/// **音源を 1 つも渡さないときは「揃っている」と言わない**。`all()` は空だと真になるので、
/// 音源を取り落とす壊れ方が「欠けた文字起こしを完成品として出す」といういちばん危険な側へ
/// 落ちてしまう。音源ゼロのセッションは一覧に載らない（`list_sessions` が飛ばす）ので、
/// 空が来るのは渡し間違いのときだけ——そのときは伏せる側で止める。
fn all_sources_are_complete(
    read: &[(Speaker, Option<TranscriptFile>)],
    sources: &[Speaker],
) -> bool {
    !sources.is_empty()
        && sources.iter().all(|source| {
            read.iter()
                .find(|(speaker, _)| speaker == source)
                .and_then(|(_, parsed)| parsed.as_ref())
                .is_some_and(|parsed| parsed.complete)
        })
}

/// 開始秒で安定ソート（同秒は mic→system の追加順を保つ）。NaN は来ない想定だが total_cmp で安全に。
fn sort_by_start(segments: &mut [TranscriptSegment]) {
    segments.sort_by(|a, b| a.start_secs.total_cmp(&b.start_secs));
}

/// 読めた JSON を話者ラベル付きのセグメント列にする。
fn to_segments(parsed: TranscriptFile, speaker: Speaker) -> Vec<TranscriptSegment> {
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

/// 保存済みの文字起こしが**どこまで届いているか**（#175）。読めない・欠けている・長さが入って
/// いないときは `None`（＝分からない）。
///
/// 途中結果を保存してよいかの判断に使う（`transcribe::partial_is_worth_keeping`）。長さだけでは
/// 「最後まで読めた完成品」と「たまたま同じ長さの途中結果」を見分けられないので、印も一緒に返す。
pub fn stored_reach(path: &Path) -> Option<StoredReach> {
    let parsed = read_guarded(path)?;
    Some(StoredReach {
        // 信頼境界外の値なので、意味のある正の秒でなければ「分からない」に落とす。
        // **印まで一緒に捨てない**——長さが読めない古い JSON にも、最後まで読めた印はありうる。
        duration_secs: (parsed.duration_secs.is_finite() && parsed.duration_secs > 0.0)
            .then_some(parsed.duration_secs),
        complete: parsed.complete,
    })
}

/// 保存済みの文字起こしが届いている範囲（#175）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoredReach {
    /// どこまでの音源から作られたか（秒）。読めなければ `None`。
    pub duration_secs: Option<f64>,
    /// その音源を最後まで読めたか。
    pub complete: bool,
}

/// 1 つの文字起こし JSON を、信頼境界外の入力として読む共通部（読む側の唯一の入口）。
///
/// 欠落（未生成）は静かに、読み取り失敗・過大・破損はログして、いずれも `None` を返す
/// （縮退。アプリは落とさない）。ログにはどちらのファイルで起きたかが分かるようファイル名
/// （`mic.json` 等）だけを含める（フルパス＝保存先や発話内容の機微情報は出さない）。
fn read_guarded(path: &Path) -> Option<TranscriptFile> {
    use std::io::Read;

    let name = path
        .file_name()
        .unwrap_or(path.as_os_str())
        .to_string_lossy();
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        // 未生成（ファイルが無い）は正常な縮退。ログもしない。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return None,
        // 権限・I/O エラーなどは異常なので、調査の手掛かりを残す。
        Err(err) => {
            eprintln!("Skipping the transcript {name} because it could not be opened: {err}");
            return None;
        }
    };
    // 信頼境界外の入力（手で置換されうる）なので、開いたハンドルの fstat で通常ファイルであることを
    // 確認し（FIFO 等は読み終わらないことがある）、サイズ上限は読み込みそのものに掛ける
    // （事前の metadata 判定だけでは差し替えに追従できない。`docs/rules/security.md`）。
    if let Ok(meta) = file.metadata()
        && !meta.is_file()
    {
        eprintln!("Skipping the transcript {name} because it is not a regular file");
        return None;
    }
    let mut limited = file.take(MAX_TRANSCRIPT_BYTES + 1);
    let mut text = String::new();
    if let Err(err) = limited.read_to_string(&mut text) {
        eprintln!("Skipping the transcript {name} because it could not be read: {err}");
        return None;
    }
    // 上限＋1 バイトまで読み切った（limit が尽きた）なら上限超過。
    if limited.limit() == 0 {
        eprintln!("Skipping the transcript {name} because it is too large");
        return None;
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
            return None;
        }
    };
    Some(parsed)
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
    use super::{Speaker, current_index, load_segments, load_transcript};
    use std::fs;
    use std::path::PathBuf;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shoki-transcript-{tag}-{}", std::process::id()))
    }

    /// **音源を 1 つも渡さなければ「揃っている」とは言わない**（#175）。`all()` の空真に頼ると、
    /// 音源を取り落とす壊れ方が「欠けた文字起こしを完成品として出す」といういちばん危険な側へ
    /// 落ちる。本番で空が来る経路は無い（音源ゼロのセッションは一覧に載らない）ので、これは
    /// 壊れたときだけ効くガード——だからテストで留める。
    #[test]
    fn no_sources_never_counts_as_complete() {
        let dir = unique_dir("no-sources");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("mic.json"),
            r#"{"complete":true,"segments":[{"start":0.0,"end":1.0,"text":"hi"}]}"#,
        )
        .unwrap();

        // 読める・最後まで読み切っている JSON が在っても、数える対象が無ければ真にしない。
        assert!(!load_transcript(&dir, &[]).complete);
        assert!(load_transcript(&dir, &[Speaker::Mic]).complete);
        // 本文だけ要る呼び出しは、そもそも空の並びを渡す形にしない。
        assert_eq!(load_segments(&dir).len(), 1);

        let _ = fs::remove_dir_all(&dir);
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

        let segments = load_segments(&dir);
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
        assert!(load_segments(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_transcript_empty_when_no_files() {
        let dir = unique_dir("none").join("missing");
        assert!(load_segments(&dir).is_empty());
    }

    /// **揃っているかは「在る音源ごとに、読めて最後まで読み切った JSON があるか」**（#175）。
    ///
    /// 「読めた JSON がすべて complete」にすると、**片方の JSON が丸ごと無いセッション**
    /// （一方だけ失敗した・途中で止めた）が `true` に化ける。#164 の途中結果でいちばん普通の形。
    #[test]
    fn a_transcript_is_complete_only_when_every_source_has_one() {
        let dir = unique_dir("complete");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let both = [Speaker::Mic, Speaker::System];

        let done = r#"{"complete":true,"segments":[{"start":0.0,"text":"a"}]}"#;
        fs::write(dir.join("mic.json"), done).unwrap();
        fs::write(dir.join("system.json"), done).unwrap();
        assert!(load_transcript(&dir, &both).complete);

        // **片方の JSON が無い**。音源は 2 本あるので揃っていない。
        fs::remove_file(dir.join("system.json")).unwrap();
        assert!(
            !load_transcript(&dir, &both).complete,
            "a source with no transcript is missing, not complete"
        );
        // 音源が mic だけのセッションなら、同じディスクの中身でも揃っている。
        assert!(load_transcript(&dir, &[Speaker::Mic]).complete);

        // 最後まで読めなかった印が立っていれば、読めても揃っていない。
        fs::write(
            dir.join("mic.json"),
            r#"{"complete":false,"segments":[{"start":0.0,"text":"a"}]}"#,
        )
        .unwrap();
        assert!(!load_transcript(&dir, &[Speaker::Mic]).complete);

        // **読めない JSON も揃っていない側**（破損・過大。読めない以上そうとは言えない）。
        fs::write(dir.join("mic.json"), b"{ this is not json").unwrap();
        let broken = load_transcript(&dir, &[Speaker::Mic]);
        assert!(broken.segments.is_empty());
        assert!(!broken.complete);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 印は**寛容に読む**（#175）。ただし**欠落と壊れた値で丸め先が違う**——欠落には互換の
    /// 根拠がある（欄が無い＝#164 以前の出力）が、壊れた値には無いので守りたい側へ倒す。
    ///
    /// どちらの場合も、その 1 欄のために JSON 全体のパースが失敗してセグメントごと消える、
    /// という形にはしない。
    #[test]
    fn a_missing_flag_reads_as_read_to_the_end_but_a_broken_one_does_not() {
        let dir = unique_dir("complete-flag");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let mic = [Speaker::Mic];

        // 欄が無い（#164 より前の出力）。最後まで読めたものとして読む。
        fs::write(
            dir.join("mic.json"),
            r#"{"segments":[{"start":0.0,"text":"a"}]}"#,
        )
        .unwrap();
        let old = load_transcript(&dir, &mic);
        assert_eq!(old.segments.len(), 1);
        assert!(old.complete);

        // 型が違う（手編集）。**セグメントは残し、印は守りたい側へ倒す**。
        fs::write(
            dir.join("mic.json"),
            r#"{"complete":"yes","segments":[{"start":0.0,"text":"a"}]}"#,
        )
        .unwrap();
        let edited = load_transcript(&dir, &mic);
        assert_eq!(
            edited.segments.len(),
            1,
            "one bad field must not drop the segments"
        );
        assert!(
            !edited.complete,
            "a flag we cannot read is not a promise that the audio was read to the end"
        );

        let _ = fs::remove_dir_all(&dir);
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
        let segments = load_segments(&dir);

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
