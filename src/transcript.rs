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
//! （見出し・理由・次の操作の対応表は `shoki_core::TranscriptPane::message` が正。欠落・破損は状態
//! `Done` のままセグメントだけ空になるので、未実施とは違う文が出る）。

use crate::dataless::{Fetch, ReadFailure};
use shoki_core::TranscriptShortfall;
use std::path::Path;
use std::time::Duration;

// **話者は core に置いてある**（#188 の PR-3a）。ここから再エクスポートするのは、
// `transcript::Speaker` という既存の呼び名を保つため（文字起こしの語彙としてはここが自然）。
pub use shoki_core::Speaker;

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

/// この音源の文字起こし JSON のファイル名。`transcribe.rs` が `<音源名>.json` で保存する
/// 名前と一致させること。
///
/// **`Speaker` のメソッドにしない**（#188）。`Speaker` は `shoki-core` にあり、そちらには
/// ファイル名を置かない（`shoki_core::session` の doc）。
fn json_name(speaker: Speaker) -> &'static str {
    match speaker {
        Speaker::Mic => MIC_JSON,
        Speaker::System => SYSTEM_JSON,
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
#[derive(Debug, Deserialize)]
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
    /// **`true` は「デコーダがストリーム終端まで到達した」という意味**でしかない。壊れた
    /// パケットを読み飛ばして中抜けした音源も `true` になるので、そちらは `gapped` が持つ（#176）。
    #[serde(
        default = "complete_by_default",
        deserialize_with = "deserialize_complete"
    )]
    complete: bool,
    /// 壊れたパケットを**読み飛ばした**か（#176）。`true` は中身が抜けていて、抜けたぶん以降の
    /// 時刻が本来より早いことを表す（`transcribe::decode_mp3_stream`）。
    ///
    /// **欠けている JSON は `false`**（`gapped_by_default`）。**在って読めない値なら `true`**
    /// ——`complete` と極性は逆だが、倒す先の理屈は同じで「分からないときは欠けている側」。
    #[serde(default = "gapped_by_default", deserialize_with = "deserialize_gapped")]
    gapped: bool,
}

impl TranscriptFile {
    /// 2 つの欄を、**下流が取り違えられない 1 つの値**へ畳む（#176）。
    fn shortfall(&self) -> Option<TranscriptShortfall> {
        ShortfallMarks {
            reached_the_end: self.complete,
            gapped: self.gapped,
        }
        .shortfall()
    }
}

/// 文字起こし JSON に残す 2 つの印（#176）。**保存形式の語彙なのでこのモジュールが持つ**
/// （`shoki_core::reading_pane` は読む領域が説明できることの語彙だけを持つ）。書く側
/// （`transcribe::Transcription`）と読む側（`TranscriptFile`）が同じ型を通る。
///
/// **名前付きのフィールドで受ける**ので、位置で取り違えられない
/// （`docs/rules/coding-conventions.md` の「同型の引数を並べた関数に切り出さない」）。極性は
/// 欄の意味に合わせてあり、揃っていない——`reached_the_end` は「食い違い無し」が `true`、
/// `gapped` は `false`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortfallMarks {
    /// デコーダがストリーム終端まで到達したか（JSON の `complete`）。
    pub reached_the_end: bool,
    /// 壊れたパケットを読み飛ばしたか（JSON の `gapped`）。
    pub gapped: bool,
}

impl ShortfallMarks {
    /// 保存欄から食い違いを組む。**書く側と読む側が同じ関数を通る**ので、片方だけ極性が
    /// 反転する壊れ方が無い。
    pub fn shortfall(self) -> Option<TranscriptShortfall> {
        let mut shortfall = None;
        if !self.reached_the_end {
            shortfall = Some(TranscriptShortfall::adding_stop(shortfall));
        }
        if self.gapped {
            shortfall = Some(TranscriptShortfall::adding_gaps(shortfall));
        }
        shortfall
    }

    /// 食い違いを保存欄へ戻す（`shortfall` の逆）。
    pub fn of(shortfall: Option<TranscriptShortfall>) -> Self {
        Self {
            reached_the_end: !shortfall.is_some_and(TranscriptShortfall::stops_partway),
            gapped: shortfall.is_some_and(TranscriptShortfall::has_gaps),
        }
    }
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
    deserialize_bool_or(deserializer, false)
}

/// 真偽値を寛容に読む共通部。**倒す先だけを呼び出し側が決める**——同じ実装を 2 本置くと、
/// 片方だけ直して食い違う（この機能そのものが「極性の逆な 2 欄を取り違えない」ための
/// ものなので、なおさら分けない）。
///
/// **この 1 欄のために JSON 全体のパースを失敗させない**（`docs/rules/error-handling.md`）。
fn deserialize_bool_or<'de, D>(deserializer: D, fallback: bool) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value.as_bool().unwrap_or(fallback))
}

/// `gapped` が欠けている JSON の既定（#176）。
///
/// **#176 より前に書かれた JSON は、読み飛ばしがあっても取り逃す**——その頃の出力は読み飛ばしを
/// 数えてはいたがログに出すだけで、欄に残していない。まだ配布していない（`docs/CONTEXT.md` の
/// 配布の決定記録）ので手元のデータにしか無く、やり直せば直る。`complete_by_default` と
/// 同じ性質の割り切り。
fn gapped_by_default() -> bool {
    false
}

/// `gapped` を寛容に読む。**欄が在って読めない値なら「抜けている」へ倒す**
/// （`deserialize_complete` と極性は逆だが、理屈は同じ——分からないときは守りたい側）。
fn deserialize_gapped<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    deserialize_bool_or(deserializer, true)
}

/// JSON の 1 セグメント。`text` は欠けていても既定値で読めるようにする（前方互換）。
/// `end` は使わないため読まない（未知フィールドとして無視される）。
#[derive(Debug, Deserialize)]
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
    /// **在る音源ぶんをまとめた、録音との食い違い**（#175 / #176）。`None` は食い違いが
    /// 見つからなかったこと。
    ///
    /// 音源が 2 本あって片方の JSON が無いセッション（一方だけ失敗した・途中で止めた）も
    /// `StopsPartway` になる。「読めた JSON がすべて揃っている」にすると、そこが `None` に化ける。
    ///
    /// 読めなかった JSON（欠落・破損・過大）も `StopsPartway`。読めない以上「揃っている」とは
    /// 言えない。
    pub shortfall: Option<TranscriptShortfall>,
}

/// セッションの文字起こしを読み、話者ラベル付きで開始秒の昇順にマージする（#175）。
///
/// **本文は在る JSON を全部読む**（`read_all`）。**`sources` は食い違いの判定にだけ使う**
/// （`sources_shortfall`）。このセッションに在る音源を渡すこと——食い違いは「在る音源ごとに、
/// 読めて食い違いの無い JSON があるか」で決まる。
pub fn load_transcript(session_dir: &Path, sources: &[Speaker], fetch: Fetch) -> Transcript {
    let read = read_all(session_dir, fetch);
    let shortfall = sources_shortfall(&read, sources);
    Transcript {
        segments: merged_segments(read),
        shortfall,
    }
}

/// 文字起こしのセグメントだけを読む（**揃っているかは見ない**）。
///
/// 議事録の生成と検索が使う——どちらも本文しか要らない。揃っているかを判断したい呼び出し側は
/// `load_transcript` を使うこと（#175）。
///
/// **読み方は `load_transcript` と同じ 1 本**（`read_all` → `merged_segments`）。
/// **`load_transcript` へ空の `sources` を渡す形にはしない**——理由は `sources_shortfall`。
pub fn load_segments(session_dir: &Path, fetch: Fetch) -> Segments {
    segments_from(read_all(session_dir, fetch))
}

/// 読めたものと読めなかったものを、検索が使える 1 つの値へまとめる（#182）。
///
/// **繋ぎを純関数にしてある**——退避されたファイルを用意できない環境で「読めなかった」の
/// 扱いを検査できる唯一の場所（`docs/rules/testing.md` の「テストが見ている入口と、本番が
/// 通る入口をずらさない」）。ここが `NotDownloaded` を落とすと、検索は退避された録音を
/// 黙って対象から外し、画面には理由も出ない。
fn segments_from(read: Vec<(Speaker, ReadOutcome)>) -> Segments {
    Segments {
        not_downloaded: read
            .iter()
            .any(|(_, outcome)| matches!(outcome, ReadOutcome::NotDownloaded)),
        segments: merged_segments(read),
    }
}

/// 読めた本文と、**実体が無くて読めなかったものがあったか**（#182）。
///
/// 本文だけ見て「無い」と決めると、退避された録音を検索が黙って対象から外したことに
/// 気づけない。2 つを 1 つの値で返して、呼び出し側が取り違えられないようにする。
///
/// **`Transcript` とは別物**——あちらは「在る音源ぶんが揃っているか」（#175）を持ち、
/// 読めなかった理由は問わない。こちらは本文だけを見て「読めなかったものがあるか」を持つ。
#[derive(Debug)]
pub struct Segments {
    pub segments: Vec<TranscriptSegment>,
    /// この録音の JSON に、実体がこの Mac に無くて読めなかったものがあった。
    /// **`Fetch::allowed()` では常に `false`**（取り寄せが走るので、遅いだけで読める）。
    pub not_downloaded: bool,
}

/// 在りうる音源ぶんの JSON を読む。**在る音源ではなく全部読む**——音源を消して文字起こしだけ
/// 残したセッションでも、読めるものは読ませるため（絞ると「検索では当たるのに開くと出て
/// こない」という食い違いになる）。
fn read_all(session_dir: &Path, fetch: Fetch) -> Vec<(Speaker, ReadOutcome)> {
    ALL_SPEAKERS
        .iter()
        .map(|&speaker| {
            (
                speaker,
                read_guarded(&session_dir.join(json_name(speaker)), fetch),
            )
        })
        .collect()
}

/// 読めたぶんを話者ラベル付きで開始秒の昇順にマージする。
fn merged_segments(read: Vec<(Speaker, ReadOutcome)>) -> Vec<TranscriptSegment> {
    let mut segments: Vec<TranscriptSegment> = read
        .into_iter()
        .filter_map(|(speaker, guarded)| match guarded {
            ReadOutcome::Read(parsed) => Some((speaker, parsed)),
            ReadOutcome::Unusable | ReadOutcome::NotDownloaded => None,
        })
        .flat_map(|(speaker, parsed)| to_segments(parsed, speaker))
        .collect();
    sort_by_start(&mut segments);
    segments
}

/// 在る音源ぶんをまとめた、録音との食い違い（#175 / #176）。
///
/// **読めなかった JSON も食い違い側**（読めない以上そうとは言えない）。そちらは
/// 「途中で終わっている」として数える——中が抜けているかは、読めない以上言えない。
///
/// **音源を 1 つも渡さないときは「食い違い無し」と言わない**。畳み込みは空だと種の値
/// （＝食い違い無し）を返すので、音源を取り落とす壊れ方が「欠けた文字起こしを完成品として
/// 出す」といういちばん危険な側へ落ちてしまう（`docs/rules/coding-conventions.md` の空真の罠。
/// #175 では `all()` の空真として同じ穴が開いていた）。音源ゼロのセッションは一覧に載らない
/// （`list_sessions` が飛ばす）ので、空が来るのは渡し間違いのときだけ——そのときは伏せる側で
/// 止める。
fn sources_shortfall(
    read: &[(Speaker, ReadOutcome)],
    sources: &[Speaker],
) -> Option<TranscriptShortfall> {
    if sources.is_empty() {
        return Some(TranscriptShortfall::StopsPartway);
    }
    sources.iter().fold(None, |merged, source| {
        let of_source = match read.iter().find(|(speaker, _)| speaker == source) {
            Some((_, ReadOutcome::Read(parsed))) => parsed.shortfall(),
            // 読めなかった／そもそも無い（`read_all` は在りうる音源ぶんを必ず並べるので、
            // `None` は渡し間違いのときだけ）。どちらも「届いていない」側で数える。
            Some((_, ReadOutcome::Unusable | ReadOutcome::NotDownloaded)) | None => {
                Some(TranscriptShortfall::StopsPartway)
            }
        };
        TranscriptShortfall::join(merged, of_source)
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
/// 途中結果を保存してよいかの判断に使う（`transcribe::write_decision`）。長さだけでは
/// 「最後まで読めた完成品」と「たまたま同じ長さの途中結果」を見分けられないので、印も一緒に返す。
pub fn stored_reach(path: &Path) -> Option<StoredReach> {
    let ReadOutcome::Read(parsed) = read_guarded(path, Fetch::allowed()) else {
        return None;
    };
    Some(StoredReach {
        // 信頼境界外の値なので、意味のある正の秒でなければ「分からない」に落とす。
        // **印まで一緒に捨てない**——長さが読めない古い JSON にも、最後まで読めた印はありうる。
        duration_secs: (parsed.duration_secs.is_finite() && parsed.duration_secs > 0.0)
            .then_some(parsed.duration_secs),
        shortfall: parsed.shortfall(),
        has_lines: !parsed.segments.is_empty(),
    })
}

/// 保存済みの文字起こしが届いている範囲（#175 / #176）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StoredReach {
    /// どこまでの音源から作られたか（秒）。読めなければ `None`。
    ///
    /// **読み飛ばしがあると、これは音声の位置ではない**（#176）。書く側は得られたサンプル数
    /// から出す（`transcribe::transcribe_file`）ので、抜けたぶん短くなる。だから比較に使う
    /// 前に `shortfall` で場合分けすること（`transcribe::write_decision`）。
    pub duration_secs: Option<f64>,
    /// その音源と録音の食い違い（`None` は食い違い無し）。
    pub shortfall: Option<TranscriptShortfall>,
    /// 開いて読む行が在るか（#176）。**「読める行を、読める行の無い結果で潰さない」を決める
    /// のに使う**（`transcribe::write_decision`）。すでにパースした結果を数えるだけなので、
    /// 読み取りは増えない。
    pub has_lines: bool,
}

/// 1 つの文字起こし JSON を、信頼境界外の入力として読む共通部（読む側の唯一の入口）。
///
/// **写像はここ 1 箇所**（#182）。読み取り側が失敗の理由を返し、それを読んだ結果へ落とすのを
/// ここだけにしてある——`ReadOutcome::Unusable` を直接返す経路を読み取りの途中に置くと、
/// 実体が無いだけのファイルが「読めなかった」に化けて検索から静かに消える。
fn read_guarded(path: &Path, fetch: Fetch) -> ReadOutcome {
    match read_transcript_file(path, fetch) {
        Ok(parsed) => ReadOutcome::Read(parsed),
        Err(failure) => read_outcome_from(failure),
    }
}

/// `read_guarded` の読み取り本体（#182 で失敗の理由を返すようにした）。
///
/// **失敗はすべて `ReadFailure` で返す**。ログを出すかもここで決めるが、判断そのものは
/// `ReadFailure::should_report` が持つ（頼まれていない読み取りでは 1 行も出さない）。
///
/// 欠落（未生成）は静かに、読み取り失敗・過大・破損はログして縮退する（アプリは落とさない）。
/// ログにはどちらのファイルで起きたかが分かるようファイル名（`mic.json` 等）だけを含める
/// （フルパス＝保存先や発話内容の機微情報は出さない。`docs/rules/security.md`）。
fn read_transcript_file(path: &Path, fetch: Fetch) -> Result<TranscriptFile, ReadFailure> {
    use std::io::Read;

    // 名前が取れない異常時も固定文字列へ落とす（`summarize::read_summary_text` と同じ理由。
    // フルパスをログへ混ぜない）。
    let name = path
        .file_name()
        .map_or(std::borrow::Cow::Borrowed("unknown"), |name| {
            name.to_string_lossy()
        });
    // **失敗の理由とログを 1 箇所で決める**。経路ごとに `eprintln!` を書くと、頼まれて
    // いない読み取りで黙らせる約束が片方だけ守られる。
    let report = |failure: ReadFailure, reason: std::fmt::Arguments| {
        if failure.should_report(fetch) {
            eprintln!("Skipping the transcript {name} because it {reason}");
        }
        failure
    };

    let file = std::fs::File::open(path).map_err(|err| {
        // **開くときも読むときも同じ見分けを通す**（`Fetch::classify` の doc）。
        report(
            fetch.classify(err.kind()),
            format_args!("could not be opened: {err}"),
        )
    })?;
    // 信頼境界外の入力（手で置換されうる）なので、開いたハンドルの fstat で通常ファイルであることを
    // 確認し（FIFO 等は読み終わらないことがある）、サイズ上限は読み込みそのものに掛ける
    // （事前の metadata 判定だけでは差し替えに追従できない。`docs/rules/security.md`）。
    if let Ok(meta) = file.metadata()
        && !meta.is_file()
    {
        return Err(report(
            ReadFailure::Failed,
            format_args!("is not a regular file"),
        ));
    }
    let mut limited = file.take(MAX_TRANSCRIPT_BYTES + 1);
    let mut text = String::new();
    if let Err(err) = limited.read_to_string(&mut text) {
        // 実測では、退避されたファイルは `open` が通ってここで返る（見分けは
        // `dataless::is_not_downloaded` の doc）。
        return Err(report(
            fetch.classify(err.kind()),
            format_args!("could not be read: {err}"),
        ));
    }
    // 上限＋1 バイトまで読み切った（limit が尽きた）なら上限超過。
    if limited.limit() == 0 {
        return Err(report(ReadFailure::Failed, format_args!("is too large")));
    }
    serde_json::from_str(&text).map_err(|err| {
        // エラーの Display は JSON 中の値（＝発話テキスト）を含みうるため出さず、位置だけログする
        // （録音由来の機微データをログへ漏らさない。`docs/rules/security.md`）。
        report(
            ReadFailure::Failed,
            format_args!(
                "could not be parsed (line {}, column {})",
                err.line(),
                err.column()
            ),
        )
    })
}

/// 読み取りに失敗したときに、何を読めたことにするか（#182）。
///
/// ここを通さずに直接組み立てると、実体が無いだけのファイルが「読めなかった」に化けて、
/// 検索から静かに消える（議事録側の対は `summarize::Summary::from_failure`。判断の正は
/// `dataless::ReadFailure`）。
fn read_outcome_from(failure: ReadFailure) -> ReadOutcome {
    match failure {
        // どちらも「本文は無い」側（未生成・破損・権限）。
        ReadFailure::NotCreated | ReadFailure::Failed => ReadOutcome::Unusable,
        // 取り寄せれば読める。
        ReadFailure::NotDownloaded => ReadOutcome::NotDownloaded,
    }
}

/// 1 つの JSON を読もうとした結果（#182）。**「無い・読めない」と「実体がここに無い」を
/// 分ける**——一緒にすると、検索が黙って対象から外した録音を「本文が無い」と扱ってしまい、
/// 「検索に出てこない＝無い」という読み違いを画面が誘発する。
#[derive(Debug)]
enum ReadOutcome {
    Read(TranscriptFile),
    /// 本文が使えない（未生成・破損・過大・権限）。**待っても直らない**。
    Unusable,
    /// 実体がこの Mac に無い（**取り寄せれば読める**。`Fetch::Blocked` のときだけ来る）。
    NotDownloaded,
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
    use super::{
        Fetch, Speaker, TranscriptShortfall, current_index, load_segments, load_transcript,
    };
    use std::fs;
    use std::path::PathBuf;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("shoki-transcript-{tag}-{}", std::process::id()))
    }

    /// 実体が無いだけのファイルを「読めなかった」に丸めないこと（#182）。
    ///
    /// 退避されたファイルは CI でも手元でも作れないので、`Deadlock` からここまでの繋ぎは
    /// 実測でしか確かめられない。**分類から結果、結果から検索が見る値まで**は繋いで検査
    /// できる——ここが丸まると、検索は退避された録音を黙って対象から外し、画面には理由も
    /// 出ない（`docs/rules/testing.md` の「テストが見ている入口と、本番が通る入口を
    /// ずらさない」）。
    #[test]
    fn a_body_that_is_only_elsewhere_is_not_lost() {
        use super::{ReadOutcome, read_outcome_from, segments_from};
        use crate::dataless::ReadFailure;

        // 分類 → 1 つの JSON を読んだ結果。
        assert!(matches!(
            read_outcome_from(ReadFailure::NotDownloaded),
            ReadOutcome::NotDownloaded
        ));
        // 待っても直らないものは、どちらも「本文は無い」側。
        assert!(matches!(
            read_outcome_from(ReadFailure::NotCreated),
            ReadOutcome::Unusable
        ));
        assert!(matches!(
            read_outcome_from(ReadFailure::Failed),
            ReadOutcome::Unusable
        ));

        // 読んだ結果 → 検索が見る値。**1 つでも実体が無ければそう言う**（片方の音源だけが
        // 退避されている録音を「当たらなかった」に丸めない）。
        let read = |first: ReadOutcome, second: ReadOutcome| {
            vec![(Speaker::Mic, first), (Speaker::System, second)]
        };
        assert!(
            segments_from(read(ReadOutcome::NotDownloaded, ReadOutcome::Unusable)).not_downloaded
        );
        assert!(
            segments_from(read(ReadOutcome::Unusable, ReadOutcome::NotDownloaded)).not_downloaded
        );
        assert!(
            !segments_from(read(ReadOutcome::Unusable, ReadOutcome::Unusable)).not_downloaded,
            "nothing to read is not the same as being unable to reach it"
        );
    }

    /// **音源を 1 つも渡さなければ「揃っている」とは言わない**（#175）。`all()` の空真に頼ると、
    /// 音源を取り落とす壊れ方が「欠けた文字起こしを完成品として出す」といういちばん危険な側へ
    /// 落ちる。本番で空が来る経路は無い（音源ゼロのセッションは一覧に載らない）ので、これは
    /// 壊れたときだけ効くガード——だからテストで留める。
    #[test]
    fn no_sources_never_counts_as_whole() {
        let dir = unique_dir("no-sources");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("mic.json"),
            r#"{"complete":true,"segments":[{"start":0.0,"end":1.0,"text":"hi"}]}"#,
        )
        .unwrap();

        // 読める・食い違いの無い JSON が在っても、数える対象が無ければ「食い違い無し」に
        // しない（#176 で `all()` の空真から畳み込みへ形が変わっても、倒れる先は同じ）。
        assert_eq!(
            load_transcript(&dir, &[], Fetch::allowed()).shortfall,
            Some(TranscriptShortfall::StopsPartway)
        );
        assert_eq!(
            load_transcript(&dir, &[Speaker::Mic], Fetch::allowed()).shortfall,
            None
        );
        // 本文だけ要る呼び出しは、そもそも空の並びを渡す形にしない。
        assert_eq!(load_segments(&dir, Fetch::allowed()).segments.len(), 1);

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

        let segments = load_segments(&dir, Fetch::allowed()).segments;
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
        assert!(load_segments(&dir, Fetch::allowed()).segments.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_transcript_empty_when_no_files() {
        let dir = unique_dir("none").join("missing");
        assert!(load_segments(&dir, Fetch::allowed()).segments.is_empty());
    }

    /// **揃っているかは「在る音源ごとに、読めて最後まで読み切った JSON があるか」**（#175）。
    ///
    /// 「読めた JSON がすべて complete」にすると、**片方の JSON が丸ごと無いセッション**
    /// （一方だけ失敗した・途中で止めた）が `true` に化ける。#164 の途中結果でいちばん普通の形。
    #[test]
    fn a_transcript_is_whole_only_when_every_source_has_one() {
        let dir = unique_dir("complete");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let both = [Speaker::Mic, Speaker::System];
        let mic = [Speaker::Mic];
        let shortfall =
            |sources: &[Speaker]| load_transcript(&dir, sources, Fetch::allowed()).shortfall;

        let done = r#"{"complete":true,"segments":[{"start":0.0,"text":"a"}]}"#;
        fs::write(dir.join("mic.json"), done).unwrap();
        fs::write(dir.join("system.json"), done).unwrap();
        assert_eq!(shortfall(&both), None);

        // **片方の JSON が無い**。音源は 2 本あるので届いていない。
        fs::remove_file(dir.join("system.json")).unwrap();
        assert_eq!(
            shortfall(&both),
            Some(TranscriptShortfall::StopsPartway),
            "a source with no transcript is missing, not whole"
        );
        // 音源が mic だけのセッションなら、同じディスクの中身でも食い違わない。
        assert_eq!(shortfall(&mic), None);

        // 最後まで読めなかった印が立っていれば、読めても届いていない。
        fs::write(
            dir.join("mic.json"),
            r#"{"complete":false,"segments":[{"start":0.0,"text":"a"}]}"#,
        )
        .unwrap();
        assert_eq!(shortfall(&mic), Some(TranscriptShortfall::StopsPartway));

        // **読み飛ばしは別の食い違い**（#176）。最後までは読めているので「途中で終わって
        // いる」とは言わない——ここが `StopsPartway` に化けると、読む領域が事実と違う文を出す。
        fs::write(
            dir.join("mic.json"),
            r#"{"complete":true,"gapped":true,"segments":[{"start":0.0,"text":"a"}]}"#,
        )
        .unwrap();
        assert_eq!(shortfall(&mic), Some(TranscriptShortfall::HasGaps));

        // 1 本の音源で両方起きた。
        fs::write(
            dir.join("mic.json"),
            r#"{"complete":false,"gapped":true,"segments":[{"start":0.0,"text":"a"}]}"#,
        )
        .unwrap();
        assert_eq!(
            shortfall(&mic),
            Some(TranscriptShortfall::StopsPartwayWithGaps)
        );

        // **音源をまたいでも重なる**（#176）。mic は抜けていて、system は途中で終わっている。
        fs::write(
            dir.join("mic.json"),
            r#"{"complete":true,"gapped":true,"segments":[{"start":0.0,"text":"a"}]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("system.json"),
            r#"{"complete":false,"segments":[{"start":0.0,"text":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(
            shortfall(&both),
            Some(TranscriptShortfall::StopsPartwayWithGaps),
            "one source stopping and another having gaps is both, not either"
        );

        // **読めない JSON も届いていない側**（破損・過大。読めない以上そうとは言えない）。
        fs::remove_file(dir.join("system.json")).unwrap();
        fs::write(dir.join("mic.json"), b"{ this is not json").unwrap();
        let broken = load_transcript(&dir, &mic, Fetch::allowed());
        assert!(broken.segments.is_empty());
        assert_eq!(broken.shortfall, Some(TranscriptShortfall::StopsPartway));

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
        let read = |json: &str| {
            fs::write(dir.join("mic.json"), json).unwrap();
            load_transcript(&dir, &mic, Fetch::allowed())
        };

        // どちらの欄も無い（#164 より前の出力）。食い違い無しとして読む。
        let old = read(r#"{"segments":[{"start":0.0,"text":"a"}]}"#);
        assert_eq!(old.segments.len(), 1);
        assert_eq!(old.shortfall, None);

        // `complete` の型が違う（手編集）。**セグメントは残し、印は守りたい側へ倒す**。
        let edited = read(r#"{"complete":"yes","segments":[{"start":0.0,"text":"a"}]}"#);
        assert_eq!(
            edited.segments.len(),
            1,
            "one bad field must not drop the segments"
        );
        assert_eq!(
            edited.shortfall,
            Some(TranscriptShortfall::StopsPartway),
            "a flag we cannot read is not a promise that the audio was read to the end"
        );

        // `gapped` は欠落と壊れた値で倒し先が逆（#176）。**極性が逆でも理屈は同じ**——
        // 欠落には互換の根拠があり、壊れた値には無いので守りたい側へ倒す。
        let missing_gapped = read(r#"{"complete":true,"segments":[{"start":0.0,"text":"a"}]}"#);
        assert_eq!(missing_gapped.shortfall, None);
        let broken_gapped =
            read(r#"{"complete":true,"gapped":3,"segments":[{"start":0.0,"text":"a"}]}"#);
        assert_eq!(
            broken_gapped.segments.len(),
            1,
            "one bad field must not drop the segments"
        );
        assert_eq!(
            broken_gapped.shortfall,
            Some(TranscriptShortfall::HasGaps),
            "a flag we cannot read is not a promise that nothing was skipped"
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
        let segments = load_segments(&dir, Fetch::allowed()).segments;

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
