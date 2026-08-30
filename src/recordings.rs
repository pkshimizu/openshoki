//! 録音セッションの探索。設定の保存先（`recording_dir`）配下にある `<%Y%m%d-%H%M%S>` 形式の
//! セッションディレクトリを列挙し、含まれる音源（mic / system）・文字起こし・議事録要約の
//! 有無を調べて新しい順に並べる。Library ウィンドウの一覧表示に使う。
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
/// セッション側の一時ファイルは**寿命が短い**: 書き手（`mixdown::normalize_if_quiet` /
/// `mixdown::generate_mix` / `summarize::write_summary`）はいずれも中身を先にメモリで作り、
/// 一時ファイルへは 1 回書いてすぐ rename するので、通常は 1 秒に満たない（モデル取得の `STALE_MODEL_PART_AGE` が 3 時間
/// なのは、受信そのものが数十分かかることに由来する別の理由）。時計のずれ・mtime の粒度ぶんの
/// 余裕もこの 1 時間に含む。
///
/// 走っている書き込みを消さない保証そのものは mtime が更新され続けることで足りる
/// （`atomic_replace::sweep_orphaned_parts` の doc）。ただし**書き出し中のスリープ**を挟むと
/// 経過は伸びうる（mtime は止まり「今」が進む）。その場合に失われるのは走っていた書き出し
/// 1 回ぶんで、rename が失敗してログに出るだけ（成果物は元のまま壊れない）。
const STALE_SESSION_PART_AGE: Duration = Duration::from_secs(60 * 60);

/// 掃除する一時ファイルの宛先名（`spawn_session_part_sweep`）。セッションディレクトリは
/// **ユーザーが中身を置ける場所**なので、アプリが `PartFile` 経由で書く宛先だけに絞る。
/// 絞る理由は `atomic_replace::PartScope`。
///
/// **書き手が増えたらここへ足す**。足し忘れると、その断片だけどの経路でも回収されない
/// （#162 で `mix.mp3` を `PartFile` 経由に変えたとき、実際に穴が空いた）。
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
    crate::mixdown::MIX_FILENAME,
    crate::summarize::SUMMARY_FILENAME,
];

/// セッションディレクトリ名の日時フォーマット（`main.rs` の録音開始時の命名と一致させること）。
const DIR_DATETIME_FORMAT: &str = "%Y%m%d-%H%M%S";
// **セッションの事実と表示は core に置いてある**（#188 の PR-3a）。走査（`list_sessions`）と
// **パスの組み立て**はこちらに残る——core にファイル名を置かない（`shoki_core::session` の doc）。
//
// `RecordingSession` だけ再エクスポートするのは `recordings::RecordingSession` という既存の
// 呼び名を保つため。`DiskFacts` は今回できた型で保つべき呼び名が無く、使うのもこのモジュールの
// 中だけなので、公開しない（`pub` にすると、使われなくなっても dead_code で気づけない）。
use shoki_core::DiskFacts;
pub use shoki_core::RecordingSession;

/// 音源ごとの mp3 名。**`Speaker` からファイル名を引く唯一の場所**——mp3 を書くのは録音側
/// なので、名前の対応もこちら（`transcript` 側は JSON 名だけを知っていればよい）。
fn audio_file_name(speaker: crate::transcript::Speaker) -> &'static str {
    match speaker {
        crate::transcript::Speaker::Mic => MIC_MP3,
        crate::transcript::Speaker::System => SYSTEM_MP3,
    }
}

/// 再生対象ファイルのパス。両音源のセッションは録音後生成の `mix.mp3`（まだ無ければ再生不可で
/// `None`）、単一音源のセッションはその音源ファイルそのもの。音源なしは `None`。
///
/// 両音源で `mix.mp3` を再生対象にするのは、選択時に毎回デコード＋ミックスすると UI が固まる
/// ため（重い処理は録音直後の生成へ移す。`src/mixdown.rs`）。
///
/// **メソッドではなく関数**（#188）。`RecordingSession` は core にあり、そちらはファイル名を
/// 知らない層なので、ここへ置くしかない。
pub fn playback_path(session: &RecordingSession) -> Option<PathBuf> {
    match (session.has_mic, session.has_system) {
        (true, true) => session.has_mix.then(|| session.dir.join(MIX_MP3)),
        (true, false) => Some(session.dir.join(MIC_MP3)),
        (false, true) => Some(session.dir.join(SYSTEM_MP3)),
        (false, false) => None,
    }
}

/// 長さを測る音源（#162）。**再生できるかとは別の関心事**——両音源セッションは `mix.mp3` が
/// 出来るまで再生できないが、長さは片方の音源から分かる（同時に録っているので同じ長さ）。
///
/// これを `playback_path` に合わせてしまうと、**録音を止めた直後**——いちばん一覧を見に行く
/// 瞬間——に長さが出ない。
fn duration_source(session: &RecordingSession) -> Option<PathBuf> {
    playback_path(session).or_else(|| {
        if session.has_mic {
            Some(session.dir.join(MIC_MP3))
        } else if session.has_system {
            Some(session.dir.join(SYSTEM_MP3))
        } else {
            None
        }
    })
}

/// 文字起こしの対象となる音源ファイル（存在する `mic.mp3` / `system.mp3`）。
/// 手動再実行（Library ウィンドウの Transcribe ボタン）の投入対象に使う。
///
/// **`speakers()` から導く**（#175）。「どの音源が在るか」の分岐を 2 つ持つと、片方だけ
/// 直した日に「文字起こしは投げるのに、揃ったかを数えない音源」が生まれる。
pub fn audio_source_paths(session: &RecordingSession) -> Vec<PathBuf> {
    session
        .speakers()
        .into_iter()
        .map(|speaker| session.dir.join(audio_file_name(speaker)))
        .collect()
}

/// テスト用に、日時だけを持つセッションを作る（ファイルの有無は呼び出し側で足す）。
///
/// **表示の組み立て**（見出し・行の文言）は日時とファイルの有無だけで決まるので、実ディスクを
/// 用意せずに検証できる。
#[cfg(test)]
pub fn session_for_test(datetime: NaiveDateTime) -> RecordingSession {
    RecordingSession::new(datetime, PathBuf::new(), DiskFacts::default())
}

/// MPEG-1 / MPEG-2 / MPEG-2.5 の Layer III のビットレート表（kbps）。添字はヘッダのビットレート
/// インデックス。`0`（自由形式）と `15`（不正）は使わない。
const MPEG1_BITRATES: [u32; 16] = [
    0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
];
const MPEG2_BITRATES: [u32; 16] = [
    0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
];

/// MP3 の**先頭フレームヘッダ**（4 バイト）から 1 秒あたりのバイト数を読む。
///
/// **設定した値を信じない**（#162）。LAME は入力のサンプルレートが低いと MPEG-2 / 2.5 になり、
/// 頼んだ 128kbps を**エラーにせず落とす**（8kHz のマイク——macOS の Bluetooth ヘッドセットが
/// 報告しうる——では 64kbps になり、見積もりが 2 倍ずれる）。書かれた値を読めば前提が要らない。
///
/// **読んでいるのは先頭のタグフレームのヘッダ**（上記のとおり枠は書かれる）。そこに実際の
/// ビットレートが入るのは **CBR のときだけ**——LAME は `vbr_off` のときに `avg_bitrate`（低い
/// サンプルレートで落とされた後の値）をこのヘッダへ写す。VBR に切り替えると、このヘッダは
/// 128kbps を名乗るようになり、この関数は黙って倍の見積もりを返す。**そのときはここごと
/// 見直すこと**（`recorder::BITRATE` の doc も）。
///
/// 同期語が無い・自由形式なら `None`。
fn bytes_per_sec_from_header(header: [u8; 4]) -> Option<u64> {
    // 同期語（11 bit）が無ければ MP3 のフレームではない。
    if header[0] != 0xFF || (header[1] & 0xE0) != 0xE0 {
        return None;
    }
    // Layer III だけ扱う（このアプリが書くのはそれだけ）。
    if (header[1] >> 1) & 0b11 != 0b01 {
        return None;
    }
    let mpeg1 = (header[1] >> 3) & 0b11 == 0b11;
    let table = if mpeg1 {
        MPEG1_BITRATES
    } else {
        MPEG2_BITRATES
    };
    let kbps = table[usize::from(header[2] >> 4)];
    (kbps > 0).then(|| u64::from(kbps) * 1000 / 8)
}

/// MP3 のファイルサイズと 1 秒あたりのバイト数から再生時間を割り出す。
///
/// 余剰は **LAME が先頭に置くタグフレーム 1 つ＋末尾フレームの端数**。`lame_get_lametag_frame`
/// を呼んでいないので中身は埋まらないが、`write_lame_tag` は既定で有効なので**枠は書かれる**
/// （128kbps/44.1kHz で 417 バイト）。合わせても 0.1 秒未満なので、秒に丸める表示には効かない。
///
/// **1 秒に満たないものは長さ不明にする**。録音に失敗してヘッダだけが残ったファイルで `00:00`
/// と出しても、`—:—` と同じくらい情報が無い。
fn duration_from_size(bytes: u64, bytes_per_sec: u64) -> Option<Duration> {
    if bytes_per_sec == 0 {
        return None;
    }
    let seconds = bytes / bytes_per_sec;
    (seconds > 0).then(|| Duration::from_secs(seconds))
}

/// 音源の長さを測った結果（#178）。**「測れない」を 2 つに分ける**——理由が違えば、読み手へ
/// 言うことも変わる（片方は待てば直り、もう片方は直らない）。
///
/// **`dataless::ReadFailure` とは別系統**（#182）。あちらは本文を読む経路（検索・表示）が
/// 「何を読めたことにするか」を決めるための分類で、ログを出すかまで持つ。こちらは長さ専用で、
/// 読むのはヘッダ 4 バイトだけ・ログは走査の終わりにまとめて 1 行。共有しているのは見分け
/// （`dataless::is_not_downloaded`）だけにしてある。
#[derive(Debug)]
enum Measured {
    /// 測れた。
    Length(Duration),
    /// **実体がこの Mac に無い**ので測れない。再生や文字起こしで実体が要るときには従来どおり
    /// 取り寄せられ、そのあとは測れる（見分け方は `measured_from_read_error` の doc）。
    NotDownloaded,
    /// 長さが決められない（開けない・属性が読めない・ヘッダが MP3 でない・1 秒未満）。
    /// **`NotDownloaded` と違って待っても直らない。**
    Unknown,
}

/// 読み取りの失敗を、長さの結果へ分類する（#178）。**分類だけでログは出さない**——名前どおりの
/// 純関数にしておくと、継ぎ目としても素直に読める。
///
/// 見分け方の正は `dataless::is_not_downloaded`（#182 で検索側と共有した）。
///
/// macOS 以外では `dataless::without_downloads` が何もしないので、この分岐へは来ない想定
/// （来たとしても長さが出ないだけで、表示は `Unknown` と同じ）。
fn measured_from_read_error(kind: std::io::ErrorKind) -> Measured {
    if crate::dataless::is_not_downloaded(kind) {
        Measured::NotDownloaded
    } else {
        Measured::Unknown
    }
}

/// 先頭 4 バイトのフレームヘッダと全体のサイズから長さを決める。
///
/// **中身を読むので証（`NoDownloads`）を要求する**。ここだけ要求しないでおくと、`File::open` を
/// 手で書いて繋ぐことで囲いの外から読めてしまう（`measure_duration` と同じ理由）。
///
/// **読み取りを引数で受ける**のは、退避されたファイルをテストから作れないため（クラウドの管理下に
/// 置くしかない）。`docs/rules/testing.md` の「重い処理そのものを引数で受ける」に従って、
/// 「読み取りが `EDEADLK` で失敗したら `NotDownloaded` になる」という**繋ぎ**をここで固定できる
/// ようにしてある。**この理由の正はここ**（`scan_sessions` が測り方を受けるのも同じ理由）。
fn measure_from_header(
    mut reader: impl std::io::Read,
    bytes: u64,
    _downloads_off: &crate::dataless::NoDownloads,
) -> Measured {
    let mut header = [0u8; 4];
    if let Err(err) = reader.read_exact(&mut header) {
        let measured = measured_from_read_error(err.kind());
        if matches!(measured, Measured::Unknown) {
            // 退避は件数をまとめて 1 行にするが（`scan_sessions`）、こちらは異常なので音源ごとに
            // 残す。**パスは出さない**（`docs/rules/security.md`）。
            eprintln!(
                "Skipping the length of a recording because its audio could not be read: {}",
                err.kind()
            );
        }
        return measured;
    }
    match bytes_per_sec_from_header(header).and_then(|rate| duration_from_size(bytes, rate)) {
        Some(length) => Measured::Length(length),
        None => Measured::Unknown,
    }
}

/// 音源の長さを測る。**ヘッダを 4 バイトだけ読む**——デコードはしない（1 本で数百 ms かかり、
/// 一覧の全件では開いた瞬間に固まる。#152）。
///
/// 保存先の音源は差し替えられうる信頼境界外の入力なので、**開いたハンドルの `fstat` で通常
/// ファイルであることを確かめる**（FIFO 等は読み終わらないことがある。`docs/rules/security.md`。
/// `open` 自体が塞がる可能性までは塞げない）。
///
/// **証（`NoDownloads`）を要求する**のは、取り寄せを止めた中でしか読ませないため（#178）。
/// 囲いの外から呼ぶ書き方はコンパイルを通らない（理由は `dataless::NoDownloads` の doc）。
fn measure_duration(path: &Path, downloads_off: &crate::dataless::NoDownloads) -> Measured {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) => {
            // 取り寄せない設定では `open` が通って `read` で返るのが実測の挙動だが、`open` 側で
            // 返す環境もありうるので同じ見分けを通す。**パスは出さない**。
            let measured = measured_from_read_error(err.kind());
            if matches!(measured, Measured::Unknown) {
                eprintln!(
                    "Skipping the length of a recording because its audio could not be opened: {}",
                    err.kind()
                );
            }
            return measured;
        }
    };
    let metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(err) => {
            eprintln!(
                "Skipping the length of a recording because its attributes could not be read: {}",
                err.kind()
            );
            return Measured::Unknown;
        }
    };
    if !metadata.is_file() {
        eprintln!("Skipping the length of a recording because its audio is not a regular file");
        return Measured::Unknown;
    }
    measure_from_header(file, metadata.len(), downloads_off)
}

/// `recording_dir` を走査して録音セッションを新しい順（日時降順）で返す。
///
/// ディレクトリが無い・読めないときは空一覧を返す（縮退。ログを残す）。名前が日時形式でない
/// エントリ、ディレクトリでないエントリ、音源が 1 つも無いセッションはスキップする。
///
/// **退避された音源の中身は読まない**（#178。理由は `crate::dataless` のモジュール doc）。
/// 取り寄せられなかった音源は `duration: None` になり、一覧は「長さが分からない録音では区切り
/// ごと出さない」既存の形で出る（#162）。
pub fn list_sessions(recording_dir: &Path) -> Vec<RecordingSession> {
    crate::dataless::without_downloads(|downloads_off| scan_sessions(recording_dir, downloads_off))
}

/// 測った結果を、セッションと集計へ振り分ける（#178）。
///
/// **ここを切り出してあるのは、測り方を差し替えずに振り分けをテストするため**。測り方ごと
/// 差し替えられるようにすると、証の壁を素通りする閉包を書けてしまう（`scan_sessions` の doc）。
fn apply_measured(session: &mut RecordingSession, measured: Measured, not_downloaded: &mut u64) {
    match measured {
        Measured::Length(length) => session.duration = Some(length),
        // **数えて後でまとめて言う**（#178）。音源ごとに出すと、退避された録音が並ぶ保存先で
        // 開くたびに十数行の同じログが流れる。
        Measured::NotDownloaded => *not_downloaded += 1,
        Measured::Unknown => {}
    }
}

/// 走査そのもの。
///
/// **測り方を差し替えられるようにしない**（#178）。閉包で受けると、その中で直接ファイルを読む
/// 書き方ができてしまい、証（`dataless::NoDownloads`）の壁を素通りする——レビューで実際にその形の
/// 穴が出た。差し替えたいのは「測った結果をどう振り分けるか」だけなので、そちらを
/// `apply_measured` に切り出してテストする。
fn scan_sessions(
    recording_dir: &Path,
    downloads_off: &crate::dataless::NoDownloads,
) -> Vec<RecordingSession> {
    let entries = match std::fs::read_dir(recording_dir) {
        Ok(entries) => entries,
        Err(err) => {
            // 保存先が未作成（まだ一度も録音していない）なども含む。落とさず空一覧にする。
            eprintln!("Skipping the recordings scan because the folder could not be read: {err}");
            return Vec::new();
        }
    };

    let mut sessions: Vec<RecordingSession> = Vec::new();
    // 実体が無くて長さを測れなかった録音の数（#178。`plural` へ渡すためだけの値）。
    let mut not_downloaded = 0u64;
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
        let mut session = RecordingSession::new(
            datetime,
            dir,
            DiskFacts {
                has_mic,
                has_system,
                has_mix,
                has_transcript,
                has_summary,
            },
        );
        // **組み上がってから長さを入れる**（#162）。選び方を素の `bool` で受ける関数に切り出すと、
        // 引数の順序を取り違えても通ってしまう——同じ `RecordingSession` の `duration_source` を
        // 通すことで、再生対象と同じ選び方であることが構造で決まる。
        if let Some(path) = duration_source(&session) {
            apply_measured(
                &mut session,
                measure_duration(&path, downloads_off),
                &mut not_downloaded,
            );
        }
        sessions.push(session);
    }

    if not_downloaded > 0 {
        // **黙って消さない**（#178）。長さの段が出ないのは異常ではないが、理由がどこにも無いと
        // 「表示が壊れた」に見える。
        eprintln!(
            "Not showing the length of {} because the audio has not been downloaded to this Mac",
            shoki_core::plural(not_downloaded, "recording")
        );
    }

    // 並び順の判断は core が持つ（`RecordingSession::newest_first`）。
    sessions.sort_by(RecordingSession::newest_first);
    sessions
}

/// 一覧に出たセッションの直下に取り残された一時ファイル（`*.part.<pid>`）を回収する
/// （Library ウィンドウを開くたびに、`list_sessions` の結果を渡して呼ぶ）。
///
/// `PartFile` の Drop が走らない終わり方（`abort`・強制終了・電源喪失）で残ったものが対象。
/// 発話由来の派生物（正規化中の音声・議事録）なので、ユーザーが気づかないまま録音フォルダに
/// 残り続けないようにする（`docs/review-perspectives/security.md`）。古さの判定と、そこから
/// 来る限界（強制終了の直後は回収されない）は `atomic_replace::sweep_orphaned_parts` の doc。
///
/// **名前だけを頼りに走査して消す唯一の経路**なので（ほかの削除経路はモジュール doc）、
/// 範囲を 3 重に絞る:
///
/// 1. **時期**: ユーザーが Library ウィンドウを開いたときだけ（常駐の起動時には走らない。
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
        DiskFacts, Measured, RecordingSession, STALE_SESSION_PART_AGE, apply_measured,
        audio_source_paths, bytes_per_sec_from_header, duration_from_size, list_sessions,
        measure_duration, measure_from_header, measured_from_read_error, parse_session_datetime,
        playback_path, session_dirs, session_for_test, spawn_session_part_sweep,
        sweep_session_dirs,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, SystemTime};

    /// 「在る音源」を言うのはここ 1 つ（#175）。**取り落とすと、欠けた文字起こしが完成品として
    /// 画面に出る**（`transcript::sources_shortfall` が数える対象そのもの）——本番では
    /// `spawn_session_load` が渡すだけなので、壊れても症状が出るまでに何段も挟まる。
    #[test]
    fn the_paths_to_transcribe_come_from_the_sources_that_are_there() {
        use crate::transcript::Speaker;

        let session = |has_mic: bool, has_system: bool| {
            let mut session = session_for_test(
                chrono::NaiveDate::from_ymd_opt(2026, 8, 10)
                    .expect("a real date")
                    .and_hms_opt(14, 2, 0)
                    .expect("a real time"),
            );
            session.dir = PathBuf::from("/recordings/20260810-140200");
            session.has_mic = has_mic;
            session.has_system = has_system;
            session
        };

        assert_eq!(
            session(true, true).speakers(),
            [Speaker::Mic, Speaker::System]
        );
        assert_eq!(session(true, false).speakers(), [Speaker::Mic]);
        assert_eq!(session(false, true).speakers(), [Speaker::System]);
        assert_eq!(session(false, false).speakers(), []);

        // **投入先と数える対象は同じ並び**（`audio_source_paths` はここから導いている）。
        for (has_mic, has_system) in [(true, true), (true, false), (false, true), (false, false)] {
            let session = session(has_mic, has_system);
            assert_eq!(
                audio_source_paths(&session),
                session
                    .speakers()
                    .into_iter()
                    .map(|speaker| session.dir.join(super::audio_file_name(speaker)))
                    .collect::<Vec<_>>(),
                "the files we transcribe and the sources we count must line up"
            );
        }
        assert_eq!(
            audio_source_paths(&session(true, true)),
            [
                PathBuf::from("/recordings/20260810-140200/mic.mp3"),
                PathBuf::from("/recordings/20260810-140200/system.mp3"),
            ]
        );
    }

    /// 長さは**サイズから割り出す**（デコードしない。#162）。
    #[test]
    fn duration_from_size_divides_by_the_rate_it_is_given() {
        assert_eq!(
            duration_from_size(16_000 * 60, 16_000),
            Some(Duration::from_secs(60))
        );
        // 端数は切り捨てる（見積もりなので、長めに出すより短めに倒す）。
        assert_eq!(
            duration_from_size(16_000 * 60 + 15_999, 16_000),
            Some(Duration::from_secs(60))
        );
        // 1 秒に満たないものは長さ不明（`00:00` は `—:—` と同じくらい情報が無い）。
        assert_eq!(duration_from_size(0, 16_000), None);
        assert_eq!(duration_from_size(15_999, 16_000), None);
    }

    /// ビットレートは**書かれた値を読む**（#162）。LAME はサンプルレートが低いと頼んだ 128kbps を
    /// エラーにせず落とすので、設定値を信じると見積もりが倍ずれる。
    #[test]
    fn bytes_per_sec_comes_from_the_frame_header() {
        // MPEG-1 Layer III, 128kbps（`0xFB` = MPEG-1 / Layer III、`0x9` = 128kbps）。
        assert_eq!(
            bytes_per_sec_from_header([0xFF, 0xFB, 0x90, 0x00]),
            Some(16_000)
        );
        // **MPEG-2.5 Layer III, 64kbps**——8kHz のマイクで LAME が落とす先。同じインデックス
        // `0x8` でも表が違い、MPEG-1 なら 112kbps・MPEG-2 系なら 64kbps。取り違えると倍ずれる。
        assert_eq!(
            bytes_per_sec_from_header([0xFF, 0xE3, 0x80, 0x00]),
            Some(8_000)
        );
        assert_eq!(
            bytes_per_sec_from_header([0xFF, 0xFB, 0x80, 0x00]),
            Some(14_000),
            "the same index means a different rate on MPEG-1"
        );
        // 同期語が無い（ID3 タグなど）。
        assert_eq!(bytes_per_sec_from_header([0x49, 0x44, 0x33, 0x04]), None);
        // 自由形式（インデックス 0）と不正（15）は使わない。
        assert_eq!(bytes_per_sec_from_header([0xFF, 0xFB, 0x00, 0x00]), None);
        assert_eq!(bytes_per_sec_from_header([0xFF, 0xFB, 0xF0, 0x00]), None);
    }

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

    /// サイズ付きでセッションを作る（長さの見積もりを見るテスト用）。`secs` は CBR から逆算した
    /// バイト数を書き込む。
    fn make_sized_session(root: &Path, name: &str, files: &[(&str, u64)]) {
        let dir = root.join(name);
        fs::create_dir_all(&dir).expect("creating the session dir succeeds in test");
        for (file, secs) in files {
            // **本物のフレームヘッダで始める**（MPEG-1 Layer III / 128kbps）。長さはここから
            // 読んだビットレートで割るので、先頭が無いと測れない。
            let mut bytes = vec![0xFFu8, 0xFB, 0x90, 0x00];
            bytes.resize((secs * 16_000) as usize, 0);
            fs::write(dir.join(file), &bytes).expect("writing the sized file succeeds in test");
        }
    }

    /// **長さを測る対象は再生の対象と同じ選び方**（両音源なら `mix.mp3`）。ここが取り違わると、
    /// 一覧に別の音源の長さが出る（#162）。
    #[test]
    fn duration_comes_from_the_source_that_playback_uses() {
        let root = unique_root("duration");
        let _ = fs::remove_dir_all(&root);
        // 両音源＋ミックス済み。**それぞれ別の長さ**にして、どれを見たか分かるようにする。
        make_sized_session(
            &root,
            "20260810-140200",
            &[("mic.mp3", 60), ("system.mp3", 120), ("mix.mp3", 180)],
        );
        // 両音源だがミックス未生成。**再生はできないが長さは出す**（録音直後がこの状態）。
        make_sized_session(
            &root,
            "20260810-130200",
            &[("mic.mp3", 90), ("system.mp3", 240)],
        );
        // 単一音源。
        make_sized_session(&root, "20260810-120200", &[("mic.mp3", 30)]);

        let sessions = list_sessions(&root);
        let duration_of = |name: &str| {
            sessions
                .iter()
                .find(|session| session.dir.ends_with(name))
                .unwrap_or_else(|| panic!("{name} should be listed"))
                .duration
        };
        assert_eq!(
            duration_of("20260810-140200"),
            Some(Duration::from_secs(180)),
            "a mixed session measures the file that playback uses"
        );
        assert_eq!(
            duration_of("20260810-130200"),
            Some(Duration::from_secs(90)),
            "without a mix, the length still comes from one of the sources"
        );
        assert_eq!(
            duration_of("20260810-120200"),
            Some(Duration::from_secs(30))
        );

        let _ = fs::remove_dir_all(&root);
    }

    /// 「測れない」を理由で分ける（#178）。**実体が無いだけ**なら待てば直るので、壊れている
    /// のと同じ扱いにしない——一覧はまとめて 1 行ログを出し、それ以外は音源ごとに出す。
    #[test]
    fn a_length_that_is_missing_says_why() {
        use std::io::ErrorKind;

        assert!(matches!(
            measured_from_read_error(ErrorKind::Deadlock),
            Measured::NotDownloaded
        ));
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::UnexpectedEof,
            ErrorKind::InvalidData,
        ] {
            let measured = measured_from_read_error(kind);
            assert!(
                matches!(measured, Measured::Unknown),
                "{kind} is not a file waiting to be downloaded, got {measured:?}"
            );
        }
    }

    /// **読み取りの失敗がそのまま結果に流れる**（#178）。失敗する読み取りを流し込む理由は
    /// `measure_from_header` の doc。
    ///
    /// ここが `Unknown` に丸まると、`Show`（長さの段）が消える理由が「壊れている」に化けて、
    /// 一覧のまとめログも出なくなる。
    #[test]
    fn a_read_that_cannot_be_served_becomes_not_downloaded() {
        /// いつも同じ失敗を返す読み取り。
        struct AlwaysFails(std::io::ErrorKind);

        impl std::io::Read for AlwaysFails {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(self.0))
            }
        }

        crate::dataless::without_downloads(|downloads_off| {
            let measured = measure_from_header(
                AlwaysFails(std::io::ErrorKind::Deadlock),
                1_000_000,
                downloads_off,
            );
            assert!(
                matches!(measured, Measured::NotDownloaded),
                "a read the OS refuses to serve means the audio is not here, got {measured:?}"
            );

            let measured = measure_from_header(
                AlwaysFails(std::io::ErrorKind::InvalidData),
                1_000_000,
                downloads_off,
            );
            assert!(
                matches!(measured, Measured::Unknown),
                "any other read failure is just a length we cannot work out, got {measured:?}"
            );

            // 読めれば、ヘッダとサイズから長さになる（128kbps = 16000 バイト/秒）。
            let header: &[u8] = &[0xFF, 0xFB, 0x90, 0x00];
            assert!(matches!(
                measure_from_header(header, 16_000 * 42, downloads_off),
                Measured::Length(length) if length == Duration::from_secs(42)
            ));
        });
    }

    /// 取り寄せられなかった音源は、**長さを入れずに数えるだけ**（#178）。ここが緩むと、実体が
    /// 無いのに長さが入る（あるいは数えられず、まとめログが出ない）。
    ///
    /// 測り方そのものは差し替えない（差し替えられる形にすると、証の壁を素通りする閉包を書ける。
    /// `scan_sessions` の doc）ので、**振り分けだけ**を直接呼んで固定する。
    #[test]
    fn audio_that_is_not_here_is_counted_instead_of_measured() {
        let mut session = RecordingSession::new(
            parse_session_datetime("20260810-140200").expect("a valid session name"),
            PathBuf::from("/does/not/matter"),
            DiskFacts {
                has_mic: true,
                ..DiskFacts::default()
            },
        );
        let mut not_downloaded = 0u64;

        apply_measured(&mut session, Measured::NotDownloaded, &mut not_downloaded);
        assert_eq!(
            session.duration, None,
            "no length is shown for audio that is not here"
        );
        assert_eq!(not_downloaded, 1, "it is counted for the summary line");

        apply_measured(&mut session, Measured::Unknown, &mut not_downloaded);
        assert_eq!(session.duration, None);
        assert_eq!(
            not_downloaded, 1,
            "a length we cannot work out is not the same thing"
        );

        apply_measured(
            &mut session,
            Measured::Length(Duration::from_secs(7)),
            &mut not_downloaded,
        );
        assert_eq!(session.duration, Some(Duration::from_secs(7)));
        assert_eq!(not_downloaded, 1);
    }

    /// 読める音源は長さになり、ヘッダが壊れていれば長さ不明になる（#178 で 3 択に割った分）。
    ///
    /// **囲いの中でしか呼べない**（`measure_duration` が証を要求する）ので、テストも本番と同じ
    /// 通り道になる。
    #[test]
    fn measuring_reads_the_header_and_falls_back_to_unknown() {
        let root = unique_root("measure");
        let _ = fs::remove_dir_all(&root);
        make_sized_session(&root, "20260810-140200", &[("mic.mp3", 42)]);
        let audio = root.join("20260810-140200").join("mic.mp3");

        crate::dataless::without_downloads(|downloads_off| {
            assert!(matches!(
                measure_duration(&audio, downloads_off),
                Measured::Length(length) if length == Duration::from_secs(42)
            ));

            // MP3 のフレーム同期が無いファイル（差し替え・破損）。
            fs::write(&audio, b"not an mp3 header at all").expect("writing should succeed");
            assert!(matches!(
                measure_duration(&audio, downloads_off),
                Measured::Unknown
            ));

            // そもそも開けない。
            fs::remove_file(&audio).expect("removing should succeed");
            assert!(matches!(
                measure_duration(&audio, downloads_off),
                Measured::Unknown
            ));
        });

        let _ = fs::remove_dir_all(&root);
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
        // **ミックスの断片も回収する**（#162 で `PartFile` 経由の書き手になった）。1 時間の録音
        // なら数十 MB の発話がそのまま残るので、宛先一覧から漏れると気づかないまま溜まる。
        let listed_mix = root.join("20260628-143025").join("mix.mp3.part.123");
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
            &listed_mix,
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
            !listed_mix.exists(),
            "the mix is written through PartFile too, so its leftovers must be removed"
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
            playback_path(&sessions[0]),
            Some(root.join("20260628-143025").join("mix.mp3"))
        );
        assert!(playback_path(&sessions[0]).is_some());
        // 両音源で mix が無ければ再生不可（選択時にその場ミックスはしない）。
        assert_eq!(playback_path(&sessions[1]), None);
        assert!(playback_path(&sessions[1]).is_none());
        // 単一音源はその音源ファイルを直接再生する。
        assert_eq!(
            playback_path(&sessions[2]),
            Some(root.join("20260627-164200").join("mic.mp3"))
        );
        assert!(playback_path(&sessions[2]).is_some());

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
