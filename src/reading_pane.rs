//! 読む領域（Library ウィンドウの Transcript / Notes タブ）に出す文言と操作（#154 / #160）。
//!
//! **確認用バイナリと共有するために切り出してある**。`examples/transcript_view.rs` は
//! `#[path]` でこのファイルを取り込む——複製していたときは実際にずれた（#161 で
//! `Waiting to summarize…` と `Waiting to write notes…` に割れているのが見つかった）。
//! 目視で確認するのが出荷される文言でなくなると、確認そのものが意味を失う。
//!
//! そのため**このモジュールは crate 内の何にも依存しない**。使うのは Slint の生成型
//! （`TranscriptStatus` / `SummaryStatus` / `PaneAction` / `PaneActionKind`）と std だけ。
//! 失敗の種別（`TranscribeFailure` / `SummarizeFailure`）をここに置いているのもそのため——
//! 種別は「読む領域が説明できることの語彙」で、文言表の網羅 match のすぐ隣にある。
//! 時計表記（`format_elapsed`）と単複の揃え（`plural`）も同じ理由でここに住んでいる。読む領域だけのものではないが、
//! **依存を持てないこのモジュールが、実装を 1 つに保てる唯一の置き場所**だった（#164）。

use std::time::Duration;

// Slint の生成型。**`crate::` で引く**——bin でも確認用バイナリでも、`slint::include_modules!()`
// がクレート直下に置くので、同じ書き方で両方から通る。
use crate::{PaneAction, PaneActionKind, SummaryStatus, TranscriptStatus};

/// 文字起こしが失敗した理由（#159）。
///
/// **文言は持たない**。種別だけを持ち、文にするのは下の `transcribe_failure_text`。
/// ワーカー層が UI のコピーを持つと、状態→文言の対応表が 2 箇所に割れる
/// （`docs/rules/messages.md` の管轄）。種別を足せば網羅 match が割れて、書き忘れに気づける。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscribeFailure {
    /// モデルを取ってこられなかった。
    ModelDownload,
    /// モデルのファイルが無い。
    ModelMissing,
    /// モデルのパスを開けない（UTF-8 でない等）。
    ModelUnreadable,
    /// モデルは在るが読み込めなかった。
    ModelLoad,
    /// 音源の文字起こしが最後まで行かなかった。
    ///
    /// **`failed` は空にならない**——構築するのは `run_job` の 1 箇所で、1 本以上が最後まで
    /// 行かなかったときにしか作らない（空だと、理由が 1 文も出ないまま
    /// `kept_other_sources` の 1 文だけが残る）。
    Files {
        /// 最後まで行かなかった音源。
        failed: Vec<FailedSource>,
        /// 同じ実行で、**読める文字起こしを残した音源が他にあるか**。失敗した音源から
        /// 何も残らなくても、こちらが残っていれば読める（`kept_partial`）。
        ///
        /// **件数では持たない**——0 か否かしか意味を持たないので、数で持つと「何本？」を
        /// 表示できる値だと誤解させる。
        kept_other_sources: bool,
    },
    /// ワーカーがパニックした（**なぜかは分からない**）。
    Panicked,
}

/// 最後まで文字起こしできなかった音源 1 本（#164）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedSource {
    /// 音源の**ファイル名だけ**（パスは持たない。`docs/rules/security.md`）。名前を作るのは
    /// `transcribe::audio_display_name` だけで、そこが保証する。
    pub name: String,
    /// この音源から何が残ったか（#164 / #176）。
    pub kept: KeptFromSource,
}

/// 最後まで行かなかった音源から、何が残ったか（#164 / #176）。
///
/// **`Option<Duration>` では持たない**（#176）。「残っていない」と「残ったがどこまでかは
/// 言えない」を `None` に相乗りさせると、`Show partial` を出すかの判断
/// （`TranscribeFailure::kept_partial`）が「位置を言えるか」に化ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeptFromSource {
    /// 何も残らなかった——1 サンプルも読めなかった・推論や保存で落ちた・残す価値が
    /// なくて保存しなかった（`transcribe::write_decision`）のいずれでもここへ来る。
    /// 理由は分けず、「残っていない」だけを表す。
    Nothing,
    /// ここまでを保存した（#164）。
    Upto(Duration),
    /// 保存したが、**どこまで読めたかは言えない**（#176）。壊れたパケットを読み飛ばして
    /// いるので、残せた長さは読み飛ばしたぶん前へ詰まっていて音声の位置ではない。
    SomeWithGaps,
}

impl KeptFromSource {
    /// 開いて読む行が残っているか。**ワイルドカードを置かない**（残り方を足したら扱いを
    /// 書くまで通らない）。
    fn has_lines(self) -> bool {
        match self {
            Self::Nothing => false,
            Self::Upto(_) | Self::SomeWithGaps => true,
        }
    }
}

impl FailedSource {
    /// 音源 1 本ぶんの記録を組む。**確認用バイナリとテストも同じものを使う**（それぞれで
    /// 組み立てを書くと、フィールドを足した日に片方だけ古くなる）。
    pub fn new(name: impl Into<String>, kept: KeptFromSource) -> Self {
        Self {
            name: name.into(),
            kept,
        }
    }
}

impl TranscribeFailure {
    /// この失敗が、読める文字起こしを残したか（#164）。読む領域はこれで `Show partial` を
    /// 出すかどうかを決める。**この判断の理由の正はここ**（他は参照だけを置く）。
    ///
    /// **この実行が書いたものだけを数える**。前回の完成した文字起こしがディスクに残っている
    /// だけのとき（モデルのロードに失敗した再実行など）に「途中結果」と言うと、完成品を
    /// 途中結果として隠すことになる。
    ///
    /// **残っている＝読む行がある**。保存したのに 1 件も認識できていなければ、開いても何も
    /// 出ない（そういう音源は保存しない。`transcribe::write_decision`）。
    ///
    /// **ワイルドカードを置かない**（種別を足したら扱いを書くまで通らない）。
    pub fn kept_partial(&self) -> bool {
        match self {
            // モデルを用意できていないので、1 本も処理していない。
            Self::ModelDownload | Self::ModelMissing | Self::ModelUnreadable | Self::ModelLoad => {
                false
            }
            Self::Files {
                failed,
                kept_other_sources,
            } => *kept_other_sources || failed.iter().any(|source| source.kept.has_lines()),
            // どこで落ちたか分からないので、残っていると言い切らない。
            Self::Panicked => false,
        }
    }
}

/// 議事録の生成が失敗した理由（#159。文言は下の `summarize_failure_text` が正）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummarizeFailure {
    /// モデルを用意できなかった。
    ModelPrepare,
    /// モデルが走り切らなかった（メモリ不足が最も多い）。
    ModelRun,
    /// 走ったが何も返さなかった。
    EmptyOutput,
    /// 生成はできたが保存に失敗した。
    Save,
    /// ワーカーがパニックした。
    Panicked,
}

/// 「文字起こし中」の表示ラベル。状態テキストと Transcript の空表示の両方で同じ文言を
/// 使うため、1 箇所で管理する（片方だけ変えて食い違うのを防ぐ）。
const TRANSCRIBING_LABEL: &str = "Transcribing…";

/// 「要約生成中」の表示ラベル。状態テキストと Notes の空表示で同じ文言を使うため 1 箇所で
/// 管理する（`TRANSCRIBING_LABEL` と同じ理由）。
const SUMMARIZING_LABEL: &str = "Writing notes…";

/// 「キュー待ち」の表示ラベル。生成中と区別できる語にする: この間はまだ CPU を使っておらず、
/// 取り消せる（`SummarizeWorker::cancel`）。
///
/// **いまの参照は状態行（`summary_status_text`）だけ**。空表示の見出しは常に番号まで出すように
/// なった（#159 で順番が必ず分かるようになった。`SummaryPane::message`）。
const SUMMARY_QUEUED_LABEL: &str = "Waiting to write notes…";

/// 「止めています」の表示ラベル（#163）。状態テキストと Transcript の空表示で同じ文言を使う
/// ため 1 箇所で管理する（`TRANSCRIBING_LABEL` と同じ理由）。
const STOPPING_LABEL: &str = "Stopping…";

/// 読む領域に出す 1 タブ分の中身（#154）。見出し・理由・次の操作の 3 つで 1 組。
///
/// **3 つをまとめて返す**のは、状態ごとに別々の関数で組み立てると「見出しは失敗なのに
/// ボタンは Transcribe now」のような食い違いを作れてしまうため。
//
// `Eq` だけ無いのは、`PaneAction` が Slint 由来の struct で `PartialEq` までしか持たないため。
#[derive(Debug, Clone, PartialEq)]
pub struct PaneMessage {
    pub heading: String,
    /// 見出しの下の 1〜2 文。空なら段ごと出さない。
    pub body: String,
    /// 並べるボタン（最大 2 つ。主操作は 1 つだけ）。
    pub actions: Vec<PaneAction>,
}

impl PaneMessage {
    /// 見出しと理由だけの土台。操作は `with_primary` / `with_secondary` で足す。
    fn new(heading: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            heading: heading.into(),
            body: body.into(),
            actions: Vec::new(),
        }
    }

    /// 主操作を 1 つ添える（並ぶのは最大 2 つで、主はこれ 1 つだけ）。
    fn with_primary(mut self, label: &str, kind: PaneActionKind) -> Self {
        self.actions.push(PaneAction {
            label: label.into(),
            kind,
            primary: true,
        });
        self
    }

    /// 補助の操作を添える（`with_action` の後ろに並ぶ）。
    fn with_secondary(mut self, label: &str, kind: PaneActionKind) -> Self {
        self.actions.push(PaneAction {
            label: label.into(),
            kind,
            primary: false,
        });
        self
    }
}

/// 保存済みの文字起こしが、録音とどう食い違っているか（#164 / #176）。
///
/// **食い違いが無いことは `Option::None` で表す**。「最後まで読めたか」と「途中を読み飛ばして
/// いないか」を別々の真偽値で持ち回ると、両方欠けた組み合わせの扱いを決め忘れられる
/// （`docs/rules/coding-conventions.md` の「状態は『status + Option の袋』にしない」）。
///
/// **文言を持たない**のは `TranscribeFailure` と同じ理由——種別だけを持ち、文にするのは
/// `TranscriptPane::message` の網羅 match。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptShortfall {
    /// 途中で終わっている（#164 / #175）。音源が途中で読めなくなった・在る音源の片方ぶんが
    /// 無い（一方だけ失敗した・途中で止めた）のどちらでもここへ来る。
    StopsPartway,
    /// 中が抜けている（#176）。壊れたパケットを読み飛ばしたので、**抜けたぶん以降の時刻は
    /// 本来より早い**（読み飛ばしたサンプルのぶん前へ詰まる）。
    HasGaps,
    /// 途中で終わっていて、その手前にも抜けがある（#176）。
    StopsPartwayWithGaps,
}

impl TranscriptShortfall {
    /// いまの食い違いに「途中で終わっている」を重ねる。
    ///
    /// **真偽値を 2 つ並べたコンストラクタにしない**（`docs/rules/coding-conventions.md` の
    /// 「同型の引数を並べた関数に切り出さない」）——`of(stops, gaps)` の形にすると、渡し違えて
    /// も通るうえ、`complete` と `gapped` は極性が逆なので揃えたくなる力まで働く。
    ///
    /// **ワイルドカードを置かない**（種別を足したら扱いを書くまで通らない）。
    pub fn adding_stop(current: Option<Self>) -> Self {
        match current {
            None | Some(Self::StopsPartway) => Self::StopsPartway,
            Some(Self::HasGaps) | Some(Self::StopsPartwayWithGaps) => Self::StopsPartwayWithGaps,
        }
    }

    /// いまの食い違いに「中が抜けている」を重ねる（`adding_stop` と対）。
    pub fn adding_gaps(current: Option<Self>) -> Self {
        match current {
            None | Some(Self::HasGaps) => Self::HasGaps,
            Some(Self::StopsPartway) | Some(Self::StopsPartwayWithGaps) => {
                Self::StopsPartwayWithGaps
            }
        }
    }

    /// 途中で終わっているか。
    pub fn stops_partway(self) -> bool {
        match self {
            Self::StopsPartway | Self::StopsPartwayWithGaps => true,
            Self::HasGaps => false,
        }
    }

    /// 中が抜けているか。
    pub fn has_gaps(self) -> bool {
        match self {
            Self::HasGaps | Self::StopsPartwayWithGaps => true,
            Self::StopsPartway => false,
        }
    }

    /// 2 つの食い違いを重ねる（音源ごとの食い違いを、セッション 1 つぶんへまとめる）。
    ///
    /// **空の列に対する答えはここでは決めない**。`None` を種に畳むと「食い違い無し」＝欠けた
    /// 文字起こしを完成品として見せる側へ倒れるので、空をどう扱うかは畳み込みに入る前に
    /// 呼び出し側が決める（`transcript::sources_shortfall` の `is_empty` ガード。
    /// `docs/rules/coding-conventions.md` の空真の罠）。
    pub fn join(left: Option<Self>, right: Option<Self>) -> Option<Self> {
        let Some(right) = right else { return left };
        let mut merged = left;
        if right.stops_partway() {
            merged = Some(Self::adding_stop(merged));
        }
        if right.has_gaps() {
            merged = Some(Self::adding_gaps(merged));
        }
        merged
    }
}

/// 読む領域が出す文字起こしの状態と、そこに出す中身（#154）。
///
/// **`TranscriptStatus` はここから導出する**（`status`）。状態 enum と説明を別々に組み立てて
/// 渡すと、片方だけ更新した瞬間にありえない組み合わせができる（`docs/rules/slint.md`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptPane {
    /// まだ走らせていない。`auto_on` は設定の自動文字起こし（なぜ無いのかの説明が変わる）。
    NotTranscribed { auto_on: bool },
    Transcribing {
        model: String,
        /// whisper が返した進捗（返し始めるまでは `None`）。
        percent: Option<u8>,
    },
    /// 止めるよう伝えたが、ワーカーがまだ降りていない（#163）。**割合は持たない**——止めると
    /// 決めた後の進捗は読み手の判断に何も足さない。
    Stopping { model: String },
    /// 走り終わっている。**この空表示が出るのは JSON が読めなかったときだけ**
    /// （セグメントがあれば一覧が出るので、ここへは来ない）。
    Done,
    /// 走り終わっているが、**録音と食い違っている**（#175 / #176）。ディスクに残った印から
    /// 分かるので、**再起動しても消えない**——`Failed` はメモリだけの記録で、再起動で消える。
    ///
    /// **どう食い違っているかまでは持つが、原因は断定しない**（文言も同じ）。
    NotWhole { shortfall: TranscriptShortfall },
    /// **理由は必ずある**（失敗の記録は理由と一緒にしか作られない。#159）。文言はここで組む。
    Failed { reason: TranscribeFailure },
}

impl TranscriptPane {
    /// Slint へ渡す状態 enum。文言と同じ値から作るので、両者が食い違わない。
    pub fn status(&self) -> TranscriptStatus {
        match self {
            Self::NotTranscribed { .. } => TranscriptStatus::NotTranscribed,
            Self::Transcribing { .. } => TranscriptStatus::Transcribing,
            Self::Stopping { .. } => TranscriptStatus::Stopping,
            // **一覧と共用の状態は増やさない**（#175）。一覧は全セッションぶんの JSON を読めない
            // ので、この区別を出せない（理由は `docs/CONTEXT.md`）。状態行の文言は
            // `status_text` が pane から出す。
            Self::Done | Self::NotWhole { .. } => TranscriptStatus::Done,
            Self::Failed { .. } => TranscriptStatus::Failed,
        }
    }

    /// いま読めるものが**途中結果**か（#164）。読む領域は、これが立っている間だけ一覧を
    /// 伏せて失敗の理由を先に出し、`Show partial` で開かせる。
    ///
    /// 何を「残した」と数えるかは `TranscribeFailure::kept_partial` が正（理由もそちら）。
    ///
    /// **走っている間は立たない**。開いた状態を畳むのは `main::fold_partial_transcript`
    /// （理由もそちら）。
    ///
    /// **ワイルドカードを置かない**（状態を足したら扱いを書くまで通らない）。
    pub fn shows_partial(&self) -> bool {
        match self {
            Self::Failed { reason } => reason.kept_partial(),
            Self::NotWhole { .. } => true,
            Self::NotTranscribed { .. }
            | Self::Transcribing { .. }
            | Self::Stopping { .. }
            | Self::Done => false,
        }
    }

    /// 詳細ペインの状態行に出す文言（#175）。**状態 enum からは出せない**——一覧と共用なので
    /// 録音との食い違いを持てず、そのままだと状態行と空表示が同じペインの中で食い違う。
    ///
    /// **ワイルドカードを置かない**（状態を足したら文言を決めるまで通らない）。
    pub fn status_text(&self) -> &'static str {
        match self {
            // **届いていないことを先に言う**（#176）。抜けているかより、読める範囲そのものが
            // 足りないほうが読み手の判断を変える。
            Self::NotWhole { shortfall } => match shortfall {
                TranscriptShortfall::StopsPartway | TranscriptShortfall::StopsPartwayWithGaps => {
                    "Transcribed in part"
                }
                TranscriptShortfall::HasGaps => "Transcribed with gaps",
            },
            Self::NotTranscribed { .. }
            | Self::Transcribing { .. }
            | Self::Stopping { .. }
            | Self::Done
            | Self::Failed { .. } => transcript_status_text(self.status()),
        }
    }

    /// 状態 → 読む領域の見出し・理由・次の操作。**ワイルドカードを置かない**（状態を足したら
    /// ここが割れて気づく。`docs/rules/slint.md`）。
    pub fn message(&self) -> PaneMessage {
        match self {
            Self::NotTranscribed { auto_on: false } => PaneMessage::new(
                "No transcript yet",
                "Automatic transcription is off, so this recording was kept as audio only.",
            )
            .with_primary("Transcribe now", PaneActionKind::Transcribe),
            Self::NotTranscribed { auto_on: true } => PaneMessage::new(
                "No transcript yet",
                "Automatic transcription is on, but this recording has not been through it.",
            )
            .with_primary("Transcribe now", PaneActionKind::Transcribe),
            Self::Transcribing { model, percent } => PaneMessage::new(
                match percent {
                    Some(percent) => format!("Transcribing — {percent}%"),
                    None => TRANSCRIBING_LABEL.to_owned(),
                },
                format!(
                    "{model} is running on this Mac. Finished lines appear here as they are \
                     recognized."
                ),
            )
            // 空表示側の Stop。詳細ヘッダの状態行にも同じ操作があり
            // （`ui/library-window.slint`）、こちらはセグメントが 0 行のときだけ出る。
            // **主操作にはしない**——押しに来る人より、進み具合を見に来る人のほうが多い。
            .with_secondary("Stop", PaneActionKind::StopTranscription),
            // **言えることだけを言う**。「何も保存されない」は嘘（音源は 1 本ずつ保存されるので、
            // mic を終えて system の途中で止めれば `mic.json` は残る。ただし止めたジョブは
            // 失敗にしないので、#164 の途中結果としては出ない——記録ごと消えて、表示は
            // JSON の有無ベースへ戻る）。
            // 「いま仕上げている最中」も嘘になりうる——モデルの取得や推論スロットの待ちで
            // 止めた場合、まだ何も処理していない（そこは待つだけで、降りるのは待ちが明けた
            // とき。`TranscribeState::Stopping` の doc）。
            Self::Stopping { model } => PaneMessage::new(
                STOPPING_LABEL,
                format!("Waiting for {model} to stop. The part it is on will not be saved."),
            ),
            Self::Done => PaneMessage::new(
                "No transcript to show",
                "The transcript file is missing or could not be read. Transcribing again will \
                 rebuild it.",
            )
            .with_primary("Transcribe again", PaneActionKind::Transcribe),
            // **開く手を必ず添える**——伏せた一覧を出す口はこれだけなので、落とすとセグメントが
            // 在るのに永久に読めなくなる。
            //
            // **操作は 3 つとも同じ並び**（#176）。押す位置が食い違いの種別で入れ替わると、
            // 画面からは見えない区別でボタンが動くことになる。
            Self::NotWhole { shortfall } => match shortfall {
                TranscriptShortfall::StopsPartway => PaneMessage::new(
                    "This transcript stops partway",
                    "It covers only part of this recording. Transcribing again will try the rest.",
                ),
                // **やり直しで直るとも直らないとも言い切らない**（#176）。読み飛ばしは音源が
                // 壊れているときにも、読み取りが一時的に失敗したときにも起きる
                // （`transcribe::decode_mp3_stream`）ので、断定するとどちらかで嘘になる。
                TranscriptShortfall::HasGaps => PaneMessage::new(
                    "This transcript has gaps",
                    "Parts of the audio could not be read, so some speech is missing and the \
                     times after the gaps are earlier than they should be. If the recording \
                     itself is damaged, transcribing again will not fill them in.",
                ),
                TranscriptShortfall::StopsPartwayWithGaps => PaneMessage::new(
                    "This transcript stops partway",
                    "It covers only part of this recording, and parts of what was read are \
                     missing as well. Transcribing again will try the rest.",
                ),
            }
            .with_primary("Transcribe again", PaneActionKind::Transcribe)
            .with_secondary("Show partial", PaneActionKind::ShowPartialTranscript),
            Self::Failed { reason } => {
                let message =
                    PaneMessage::new("Transcription failed", transcribe_failure_text(reason))
                        .with_primary("Try again", PaneActionKind::Transcribe);
                // 残っているときだけ開く手を出す（#164）。残っていないのに出すと、押しても
                // 何も現れないボタンになる。
                if reason.kept_partial() {
                    message.with_secondary("Show partial", PaneActionKind::ShowPartialTranscript)
                } else {
                    message
                }
            }
        }
    }
}

/// 文字起こしの表示状態 → 状態テキスト。
///
/// **直に呼ぶのは `TranscriptPane::status_text` だけ**（#175）。詳細ペインはそちらを通す——状態
/// enum は一覧と共用で録音との食い違いを持てないので、直に引くと同じペインの中で状態行が
/// `Transcribed`、空表示が「録音と食い違っている」と食い違う。一覧の行は別の語を使う
/// （`session_transcript_word`）。
pub fn transcript_status_text(display_status: TranscriptStatus) -> &'static str {
    match display_status {
        TranscriptStatus::NotTranscribed => "Not transcribed",
        TranscriptStatus::Transcribing => TRANSCRIBING_LABEL,
        TranscriptStatus::Stopping => STOPPING_LABEL,
        TranscriptStatus::Done => "Transcribed",
        TranscriptStatus::Failed => "Transcription failed",
    }
}

/// 走っているジョブがある間、**中身を作り直す操作を出さない**。
///
/// ワーカーがこのセッションの JSON / `summary.md` を読み書きしている最中に別のジョブを重ねると、
/// 書き換え途中の内容を読ませてしまう。詳細ヘッダの Transcribe / Summarize が同じ理由で無効に
/// なるので、押す場所が増えた空表示にも同じゲートを掛ける（取り消しと窓を開くのは残す）。
pub fn actions_allowed_while_busy(actions: Vec<PaneAction>, jobs_pending: bool) -> Vec<PaneAction> {
    if !jobs_pending {
        return actions;
    }
    actions
        .into_iter()
        .filter(|action| match action.kind {
            PaneActionKind::Transcribe
            | PaneActionKind::WriteNotes
            | PaneActionKind::TranscribeThenNotes => false,
            // 止める操作は**走っている間しか出ない**ので、ここで落とすと出す先が無くなる
            // （`docs/rules/slint.md`。取り消しと窓を開くのは残すのと同じ理由）。途中結果を
            // 開く操作は、すでにディスクに在るものを見せるだけなので重ねようがない。
            PaneActionKind::StopTranscription
            | PaneActionKind::CancelNotes
            | PaneActionKind::ShowPartialTranscript
            | PaneActionKind::OpenTranscription
            | PaneActionKind::OpenNotes => true,
        })
        .collect()
}

/// 一覧の行に出す文字起こしの状態（**網羅 match**。状態を足したら語を決めるまで通らない）。
pub fn session_transcript_word(status: TranscriptStatus, percent: Option<u8>) -> String {
    match status {
        TranscriptStatus::NotTranscribed => "not transcribed".to_owned(),
        // 割合が来ていれば出す（#162）。読む領域を開かなくても、どれが動いているか分かる。
        TranscriptStatus::Transcribing => match percent {
            Some(percent) => format!("transcribing {percent}%"),
            None => "transcribing".to_owned(),
        },
        TranscriptStatus::Stopping => "stopping".to_owned(),
        TranscriptStatus::Done => "transcribed".to_owned(),
        TranscriptStatus::Failed => "transcription failed".to_owned(),
    }
}

/// 議事録生成の表示状態 → 詳細ペインの状態テキスト。
pub fn summary_status_text(display_status: SummaryStatus) -> &'static str {
    match display_status {
        SummaryStatus::NotSummarized => "No notes",
        SummaryStatus::Queued => SUMMARY_QUEUED_LABEL,
        SummaryStatus::Summarizing => SUMMARIZING_LABEL,
        SummaryStatus::Done => "Notes ready",
        SummaryStatus::Failed => "Notes failed",
    }
}

/// ディスクに残っている文字起こしの様子（#175）。**真偽値を並べない**——「在るか」と「最後まで
/// 読み切れているか」を別々に渡すと、渡し違えてもコンパイルが通る
/// （`docs/rules/coding-conventions.md`）。
///
/// 組み立てるのは `main::LoadedTranscript::stored`（`transcript::Transcript` を見るので、
/// crate に依存できないこのモジュールには置けない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoredTranscript {
    /// 文字起こしが無い。
    None,
    /// 在って、**食い違いは分かっていない**（#176 で名前を意味に合わせた）。最後まで読み
    /// 切れているか、読み直しの最中で分からないか、**読める行が無い**（読めなかった JSON。
    /// 押しても何も現れない `Show partial` を出さないよう、ここへ落とす。
    /// `main::LoadedTranscript::stored`）。「完成品と分かっている」ではない。
    NoKnownShortfall,
    /// 在って読める行もあるが、**録音と食い違っている**。**原因は断定しない**。
    NotWhole { shortfall: TranscriptShortfall },
}

/// 議事録タブから見た**入力（文字起こし）の様子**（#165）。
///
/// **議事録タブが要るのはこれだけ**なので、`has_transcript` と状態 enum を別々に渡さず
/// 1 つの値にまとめる（真偽値を並べると、渡し違えてもコンパイルが通る。
/// `docs/rules/coding-conventions.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptInput {
    /// 読める文字起こしが在り、**食い違いは分かっていない**
    /// （`StoredTranscript::NoKnownShortfall` と同じ範囲。読めなかった JSON もここへ来る
    /// ——そのときは Transcript タブが「読めなかった」の空表示を出す）。
    Ready,
    /// 読める文字起こしが在るが、**録音と食い違っている**（#175 / #176）。議事録は書けるが、
    /// 欠けた入力から書いたものになる。
    ///
    /// **どう食い違っているかは持たない**（#176）。議事録タブの言い分は 3 種類とも同じで、
    /// 内訳を言うのは Transcript タブの仕事。持たせると、同じことを 2 箇所で言い分ける
    /// ことになる。
    NotWhole,
    /// まだ無いが、いま作っている。
    Running,
    /// まだ無く、作ろうとして失敗した。
    Failed,
    /// まだ無く、作ってもいない。
    Missing,
}

impl TranscriptInput {
    /// 文字起こしの状態と「読める文字起こしが在るか」から、議事録タブの見方を決める（#165）。
    ///
    /// **ディスクに在るものが先**（#175）——作り直している最中でも、議事録が読むのはディスクに
    /// 在るものだから。入力が無いときだけ、なぜ無いのかをワーカーの記録で言い分ける。
    ///
    /// **ワイルドカードを置かない**（状態を足したら扱いを書くまで通らない）。
    pub fn of(transcript: &TranscriptPane, stored: StoredTranscript) -> Self {
        match stored {
            // **ディスクが答えを持っている**（#175）。ワーカーの記録は再起動で消えるので、
            // そちらで見分けると同じセッションが再起動の前後で違うことを言う。
            StoredTranscript::NoKnownShortfall => Self::Ready,
            StoredTranscript::NotWhole { .. } => Self::NotWhole,
            // 入力が無いときだけ、なぜ無いのかをワーカーの記録で言い分ける。
            StoredTranscript::None => match transcript.status() {
                TranscriptStatus::Transcribing | TranscriptStatus::Stopping => Self::Running,
                TranscriptStatus::Failed => Self::Failed,
                TranscriptStatus::NotTranscribed | TranscriptStatus::Done => Self::Missing,
            },
        }
    }

    /// 議事録がまだ無いときに出す状態（#165）。
    ///
    /// **本番も確認用バイナリもここを通す**（`docs/rules/testing.md` の「確認用バイナリでも、
    /// 状態は 1 つの値から出す」）。別々に選ぶと、本番では作れない組み合わせを目視してしまう。
    pub fn pane_when_no_notes(self, auto_on: bool) -> SummaryPane {
        match self {
            Self::Ready => SummaryPane::NotSummarized { auto_on },
            Self::NotWhole => SummaryPane::NotesFromPartialTranscript,
            Self::Running => SummaryPane::WaitingForTranscript,
            Self::Failed => SummaryPane::TranscriptFailed,
            Self::Missing => SummaryPane::Blocked,
        }
    }
}

/// 読む領域が出す議事録の状態と、そこに出す中身（#154。`TranscriptPane` と対称）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SummaryPane {
    /// **文字起こしが無いので動けない**。議事録は文字起こしを入力にするので、ここだけは
    /// 「まだ書いていない」ではなく「まだ書けない」。
    Blocked,
    /// 文字起こしを待っている（#165）。続けて書く依頼を出した直後もここに来る——依頼は
    /// 文字起こしジョブにぶら下がっていて、要約ワーカーにはまだ積まれていない
    /// （だから `status()` は「未生成」のまま。取り消せる相手がいない）。
    WaitingForTranscript,
    /// 文字起こしが失敗したので、議事録は始まらなかった（#165）。**入力が欠けたまま
    /// 完成品に見える議事録を作らない**という判断の結果なので、そう言う。
    TranscriptFailed,
    /// 入力の文字起こしが**録音と食い違っている**（#175 / #176。途中で終わっている／中が
    /// 抜けている）。書けるが、欠けたまま完成品に見える議事録になる——**止めはせず、そうなると
    /// 先に言う**（文字起こしが失敗した経路だけは #164 が止めている）。
    NotesFromPartialTranscript,
    /// 文字起こしはあるが、まだ書いていない。
    NotSummarized { auto_on: bool },
    Queued {
        /// キュー待ちの中で何番目か（1 始まり）。**必ず分かる**（キュー待ちの記録は順番と
        /// 一緒にしか作られない。#159）。
        position: usize,
    },
    Summarizing {
        model: String,
        /// 始めてからの経過（読める粒度に丸めた文字列）。**必ず分かる**（生成中の記録は開始
        /// 時刻と一緒にしか作られない）。
        started_ago: String,
    },
    /// 書き終わっている。**この空表示が出るのは `summary.md` が読めなかったときだけ**。
    Done,
    /// **理由は必ずある**（失敗の記録は理由と一緒にしか作られない。#159）。
    Failed { reason: SummarizeFailure },
}

impl SummaryPane {
    /// Slint へ渡す状態 enum。`Blocked` と `NotSummarized` はどちらも「未生成」に落ちる
    /// （読む領域の説明は違うが、ボタンの活性や一覧の見え方は同じ）。
    pub fn status(&self) -> SummaryStatus {
        match self {
            // 待っている間・入力が失敗した後も「未生成」。要約ワーカーには何も積まれて
            // いないので、キュー待ちや生成中と同じ扱いにはできない（削除・再実行のゲートは
            // 文字起こし側が握っている）。
            Self::Blocked
            | Self::WaitingForTranscript
            | Self::TranscriptFailed
            | Self::NotesFromPartialTranscript
            | Self::NotSummarized { .. } => SummaryStatus::NotSummarized,
            Self::Queued { .. } => SummaryStatus::Queued,
            Self::Summarizing { .. } => SummaryStatus::Summarizing,
            Self::Done => SummaryStatus::Done,
            Self::Failed { .. } => SummaryStatus::Failed,
        }
    }

    /// 状態 → 読む領域の見出し・理由・次の操作（`TranscriptPane::message` と同じ流儀）。
    pub fn message(&self) -> PaneMessage {
        match self {
            Self::Blocked => PaneMessage::new(
                "No notes yet",
                "Notes are written from the transcript, and this recording has none. \
                 Transcribing it first will let notes run.",
            )
            // **1 回の操作で議事録まで行く**（#165）。ここまで来た人が欲しいのは議事録で、
            // 文字起こしはその途中でしかない（文字起こしだけ回す口は Transcript タブ）。
            .with_primary(
                "Transcribe, then write notes",
                PaneActionKind::TranscribeThenNotes,
            )
            .with_secondary("Open transcription", PaneActionKind::OpenTranscription),
            Self::WaitingForTranscript => PaneMessage::new(
                "Waiting for the transcript",
                "Notes are written from the transcript, so they start once it finishes. \
                 The Transcript tab shows its progress.",
            ),
            // **押せる手を出す**。入力から作り直すしかないので、行き先は続けて書く依頼と同じ。
            Self::TranscriptFailed => PaneMessage::new(
                "No notes yet",
                "The transcription did not finish, so notes did not start. Notes are written \
                 from the transcript, and an incomplete one would leave the notes incomplete \
                 too.",
            )
            .with_primary("Try again", PaneActionKind::TranscribeThenNotes)
            .with_secondary("Open transcription", PaneActionKind::OpenTranscription),
            // **どう食い違っているかは言い分けない**（#176）。途中で終わっていても中が抜けて
            // いても、議事録にとっては「入力が欠けている」の一言で足り、内訳は Transcript
            // タブが言う。
            Self::NotesFromPartialTranscript => PaneMessage::new(
                "No notes yet",
                "The transcript is missing parts of this recording. Notes written from it \
                 would be missing them too.",
            )
            .with_primary("Write notes anyway", PaneActionKind::WriteNotes)
            .with_secondary("Open transcription", PaneActionKind::OpenTranscription),
            Self::NotSummarized { auto_on: false } => PaneMessage::new(
                "No notes yet",
                "Notes are not written automatically, so this recording does not have any.",
            )
            .with_primary("Write notes", PaneActionKind::WriteNotes),
            Self::NotSummarized { auto_on: true } => PaneMessage::new(
                "No notes yet",
                "Automatic notes are on, but this recording has not been through them.",
            )
            .with_primary("Write notes", PaneActionKind::WriteNotes),
            Self::Queued { position } => PaneMessage::new(
                format!("Waiting to start — number {position} in the queue"),
                "Notes start once the work ahead of this recording finishes. Nothing is running \
                 for it yet, so it can still be canceled.",
            )
            .with_secondary("Cancel", PaneActionKind::CancelNotes),
            Self::Summarizing { model, started_ago } => PaneMessage::new(
                SUMMARIZING_LABEL,
                format!(
                    "{model} is running on this Mac, started {started_ago} ago. Re-transcribing \
                     is unavailable until this finishes, because it would change the input."
                ),
            ),
            Self::Done => PaneMessage::new(
                "No notes to show",
                "The notes file is missing or could not be read. Writing them again will \
                 rebuild it.",
            )
            .with_primary("Write notes again", PaneActionKind::WriteNotes),
            Self::Failed { reason } => {
                PaneMessage::new("Notes could not be written", summarize_failure_text(reason))
                    .with_primary("Try again", PaneActionKind::WriteNotes)
                    .with_secondary("Open meeting notes", PaneActionKind::OpenNotes)
            }
        }
    }
}

/// 文字起こしが失敗した理由 → 読む領域に出す 1 文（#159）。
///
/// **ワイルドカードを置かない**。種別を足したらここが割れて、書き忘れに気づける
/// （文言をワーカー層に置かないのはこのため。`docs/rules/messages.md` の管轄が割れる）。
pub fn transcribe_failure_text(reason: &TranscribeFailure) -> String {
    match reason {
        TranscribeFailure::ModelDownload => {
            "The transcription model could not be downloaded.".to_owned()
        }
        TranscribeFailure::ModelMissing => "The transcription model file is missing.".to_owned(),
        TranscribeFailure::ModelUnreadable => {
            "The transcription model file could not be opened.".to_owned()
        }
        TranscribeFailure::ModelLoad => "The transcription model could not be loaded.".to_owned(),
        // **件数で文の形は変えない**（`docs/rules/messages.md`）ので、1 本でも複数でも音源
        // ごとに 1 文ずつ並べる。どこまで読めたかは、読めた音源だけが言う（#164）。
        // 最後の 1 文は `kept_other_sources` も見る（`kept_partial`）ので、ここで束ねない。
        TranscribeFailure::Files { failed, .. } => {
            let mut text = failed
                .iter()
                .map(|source| match source.kept {
                    KeptFromSource::Upto(upto) => format!(
                        "{} could not be read past {}.",
                        source.name,
                        format_elapsed(upto)
                    ),
                    // **位置は言わない**（#176）。読み飛ばしたぶん時刻が前へ詰まっているので、
                    // 残せた長さは「そこまで読めた」を意味しない。
                    KeptFromSource::SomeWithGaps => format!(
                        "{} could not be read to the end, and parts of what was read are missing.",
                        source.name
                    ),
                    KeptFromSource::Nothing => {
                        format!("{} could not be transcribed.", source.name)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            if reason.kept_partial() {
                // `failed` が空のときに先頭が空白にならないようにする（型では空を禁止でき
                // ないので、文の組み立て側で壊れないようにしておく）。
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str("Everything that was read is kept.");
            }
            text
        }
        // **なぜ止まったかは分からない**ので、分かったふりをしない。
        TranscribeFailure::Panicked => {
            "Transcribing this recording stopped unexpectedly.".to_owned()
        }
    }
}

/// 議事録の生成が失敗した理由 → 読む領域に出す 1 文（#159。`transcribe_failure_text` と対称）。
pub fn summarize_failure_text(reason: &SummarizeFailure) -> String {
    match reason {
        SummarizeFailure::ModelPrepare => {
            "The meeting notes model could not be prepared.".to_owned()
        }
        // **なぜ落ちたかは分からない**（llama.cpp は理由を返さないことが多い）ので、断定せずに
        // 「いちばんよくある原因」と、そこから取れる手を添える。
        SummarizeFailure::ModelRun => "The model could not finish. It may need more \
             free memory than this Mac has right now — closing other apps, or choosing a smaller \
             model, can let it run."
            .to_owned(),
        SummarizeFailure::EmptyOutput => "The model returned nothing to write.".to_owned(),
        SummarizeFailure::Save => "The notes could not be saved.".to_owned(),
        SummarizeFailure::Panicked => "Writing notes stopped unexpectedly.".to_owned(),
    }
}

/// 経過時間を時計表記にする。既定は `mm:ss`、1 時間以上は `h:mm:ss`。
///
/// **表記の正はここ 1 つ**。録音中のメニューバー表示（`tray::format_elapsed` は再エクスポート）・
/// 再生位置・トランスクリプトの時刻・文字起こしが失敗した理由（#164）が同じ関数を通る。
/// このモジュールに置いてあるのは、確認用バイナリと共有できる唯一の場所だから——逆向きに
/// 依存させると、確認用バイナリが crate 全体を引き込むことになる（このファイル冒頭の doc）。
pub fn format_elapsed(elapsed: Duration) -> String {
    const SECS_PER_MINUTE: u64 = 60;
    const SECS_PER_HOUR: u64 = 60 * SECS_PER_MINUTE;

    let total = elapsed.as_secs();
    let hours = total / SECS_PER_HOUR;
    let minutes = (total % SECS_PER_HOUR) / SECS_PER_MINUTE;
    let seconds = total % SECS_PER_MINUTE;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

/// 経過を読める粒度へ丸める（`40 seconds` / `3 minutes`）。**秒まで出すのは 1 分未満だけ**——
/// 分オーダーの処理で秒を刻んでも読めないうえ、100ms の tick で数字が動き続ける。
pub fn elapsed_text(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        return plural(seconds, "second");
    }
    plural(seconds / 60, "minute")
}

/// 数と単位を英語として揃える（`1 minute` / `3 minutes`）。**そのまま画面に出る**文なので、
/// 単複が崩れると読みにくい（`docs/rules/messages.md`）。ログにも出る（#178 の
/// `recordings::scan_sessions`）——同じ揃え方の実装を 2 つ持たない。
pub fn plural(count: u64, unit: &str) -> String {
    if count == 1 {
        format!("1 {unit}")
    } else {
        format!("{count} {unit}s")
    }
}
