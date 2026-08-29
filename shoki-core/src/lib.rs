// **クレート属性は item より前に置く**（`//!` の後ろでも通るが、item の後ろだと
// 「inner attribute is not permitted in this context」で落ちる）。doc と混ぜず先頭に固めて
// おけば、#188 で `pub mod` をどこに足しても位置を考えなくて済む。
//
// FFI も `unsafe` も shell の仕事なので、`allow` で穴を開けられない `forbid` にする。
#![forbid(unsafe_code)]

//! shoki の純粋な層（`core`）。
//!
//! **この層は状態（`AppState`）・判断（`update` / `Effect`）・状態から文言への表だけを持つ。**
//! ファイルを読む・スレッドを立てる・画面を触るコードは `shoki` 側（`shell`）に置く。
//!
//! ```text
//! shell（副作用がある層）
//!   UI（Slint 配線）／ job runtime ／ adapters
//!         ↓ Command / Event        ↑ Effect / View
//! core（このクレート）
//!   update(&mut AppState, Msg) -> Vec<Effect>
//!   view_*(&AppState) -> ...
//! ```
//!
//! 状態を持つ場所を 1 つにし、判断を副作用から引き剥がすのが狙い。背景と計測は
//! `docs/plans/done/20260829-core-shell-layers.md`、今後も効く結論は `docs/CONTEXT.md` の
//! 「主要な設計判断」が正。
//!
//! # 何が守られていて、何が守られていないか
//!
//! **クレート境界が禁じるのは `shoki` 側の型に触ること**（Slint の生成型・whisper・adapter・
//! `private_file` などのヘルパー）。依存が `shoki-core` → `shoki` の向きに無いので、参照した
//! 時点でコンパイルが通らない。
//!
//! **`std` の I/O は止まらない。** 依存ゼロのクレートでも `std::fs::read_to_string` は書けるし、
//! `#![forbid(unsafe_code)]` も I/O は止めない。ここを「コンパイラが守ってくれる」と思い込むと、
//! `private_file`（0600 で作る保証。`docs/rules/security.md`）を通らない書き出しが core 側に
//! 生まれる。
//!
//! 代わりに `clippy.toml` の `disallowed-types` / `disallowed-methods` で、よくある入り口
//! （`std::fs` / `std::thread::spawn` / 時刻の取得）を CI で弾いている。**網羅ではないので、
//! レビューの代わりにはならない。**
//!
//! # いまの状態
//!
//! **器だけ**（#187）。中身は #188（文字起こしの状態）から順に移す。

pub mod reading_pane;

pub use reading_pane::*;

/// 文字起こしの表示状態（一覧の行と詳細ペインが共用する）。
///
/// **UI の型を core が持つ**（#188）。Slint の生成型（`ui/library-window.slint` の
/// 同名 enum）へ写すのは shell の仕事（`shoki::slint_map`）。写像は網羅 match なので、
/// 変種を足したらコンパイラが写し忘れを教える。
///
/// **一覧と共用なので、録音との食い違いは持てない**（一覧は全セッションぶんの JSON を
/// 読めない。理由は `docs/CONTEXT.md`）。詳細ペインの状態行は
/// `TranscriptPane::status_text` が出す。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TranscriptStatus {
    #[default]
    NotTranscribed,
    Transcribing,
    /// 止めるよう伝えたが、ワーカーがまだ降りていない（#163）。降りたら未実施／生成済みへ戻る。
    Stopping,
    Done,
    Failed,
}

/// 議事録の表示状態（`TranscriptStatus` と同じ流儀。先頭が未生成＝既定値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SummaryStatus {
    #[default]
    NotSummarized,
    /// 投入済みで、ワーカーが取り出すのを待っている（まだ CPU を使っていない）。この間だけ
    /// 取り消せる。
    Queued,
    Summarizing,
    Done,
    Failed,
}

/// 読む領域の空表示から起こせる操作（#154）。
///
/// **enum で渡す**——ラベルの文字列で分岐すると、文言を直した日に操作が静かに壊れる。
/// どれを出すかは `TranscriptPane::message` / `SummaryPane::message` の網羅 match が決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneActionKind {
    /// 文字起こしを始める / やり直す。
    Transcribe,
    /// 議事録を書く / やり直す。
    WriteNotes,
    /// キュー待ちの議事録を取り消す。
    CancelNotes,
    /// 走っている（またはキュー待ちの）文字起こしを止める。
    StopTranscription,
    /// 文字起こしの設定ウィンドウを開く。
    OpenTranscription,
    /// 議事録の設定ウィンドウを開く。
    OpenNotes,
    /// 文字起こしを走らせ、成功したら続けて議事録を書く（#165）。
    TranscribeThenNotes,
    /// 途中までの文字起こしを開く（#164）。失敗の理由を伏せて一覧に切り替えるだけで、
    /// ディスクには何も起こさない。
    ShowPartialTranscript,
}

/// 空表示に並べるボタン 1 つ分。**ラベルも core が組む**（状態→文言の対応表は網羅 match が正）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneAction {
    pub label: String,
    pub kind: PaneActionKind,
    /// 主操作か。並ぶのは最大 2 つで、主は 1 つだけ（`PaneMessage::with_primary` が保証する）。
    pub primary: bool,
}
