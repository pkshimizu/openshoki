//! 録音一覧（Library ウィンドウ）の下端と空表示に出す文（#161 / #182）。
//!
//! **本番と確認用バイナリで同じ文を出すために切り出してある**（`docs/rules/testing.md` の
//! 「確認用バイナリに文言を複製しない」）。`examples/transcript_view.rs` が `#[path]` で
//! そのまま取り込むので、**crate 内の何にも依存させない**——例外は同じやり方で共有して
//! いる `shoki_core::reading_pane`（単複の言い回しをそこに寄せてある）。

/// 一覧が空のとき、**なぜ空なのか**（#161 / #181 / #182）。
///
/// **真偽値を並べない**——「走査中か」「絞り込み中か」を別々の bool で持つと、両方立った
/// 組み合わせの文言を決め忘れられる（`docs/rules/coding-conventions.md`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmptyList {
    /// まだ走査が終わっていない（#181）。件数は 0 だが、録音が無いとは限らない。
    Scanning,
    /// 走査は終わっていて、録音が 1 件も無い。
    NoRecordings,
    /// 絞り込んだ結果 0 件。`not_downloaded` は本文を読めなかった録音の数（#182）。
    NoMatches { not_downloaded: usize },
}

/// 一覧の下端に出す合計。**件数だけ**にする——容量を出すには全セッションのファイルを開く必要が
/// あり、一覧を開くたびに走らせるには重い。
pub fn library_summary(count: usize) -> String {
    match count {
        1 => "1 recording".to_owned(),
        count => format!("{count} recordings"),
    }
}

/// 絞り込み中に一覧の下へ出す件数。**解除の手を文に入れる**（0 件のときは本文側で出す）。
///
/// **読めなかったぶんも言う**（#182）。黙って対象から外すと「検索に出てこない＝無い」と
/// 読める。語は一覧側（`recordings::scan_sessions` の「has not been downloaded to this Mac」）に
/// 揃える。出す場所が省略されうる 1 行なので、**節は短く保つ**こと。
pub fn search_summary_text(matched: usize, total: usize, not_downloaded: usize) -> String {
    // 件数で**文の形**は変えない（0 件でも同じ言い方）が、名詞の単複は揃える
    // （`library_summary` が `1 recording` と分けているのと同じ）。
    let mentions = if total == 1 {
        format!("{matched} of 1 recording mentions it")
    } else {
        format!("{matched} of {total} recordings mention it")
    };
    if not_downloaded == 0 {
        return mentions;
    }
    format!("{mentions} · {not_downloaded} not downloaded")
}

/// 一覧が空のときに出す見出しと本文（#182 で「読めなかった」を足した）。
///
/// **状態から文への対応はここ 1 つ**（Slint の三項連鎖にしない。`docs/rules/slint.md`）。
/// 空表示は**録音が無いのか、絞り込んで消えたのか**で言い分ける。同じ文にすると、検索語を
/// 消せば戻ることが分からない。
///
/// **絞り込んで 0 件のときこそ理由を言う**。一覧の下端の 1 行は省略されうるので、退避されて
/// 読めなかったことを伝える場所としてはここがいちばん見える。
pub fn empty_list_message(state: EmptyList) -> (&'static str, String) {
    let not_downloaded = match state {
        // **「録音が無い」とは言わない**（#181）。まだ数えていないだけで、在るかどうかは
        // 分かっていない。走査は 0.6ms（#178）なので速い保存先ではまず目に入らないが、
        // 遅いボリュームではここが数秒出る——そこで嘘をつくと、開いた人は録音を失ったと思う。
        EmptyList::Scanning => {
            return (
                "Looking for recordings…",
                "Reading the save location. This can take a moment on a network or external \
                 volume."
                    .to_owned(),
            );
        }
        EmptyList::NoMatches { not_downloaded } => not_downloaded,
        EmptyList::NoRecordings => {
            return (
                "No recordings yet",
            "Start one from the shoki icon in the menu bar, or turn on Record automatically in \
             Settings so meetings are captured for you."
                .to_owned(),
            );
        }
    };
    let mut body = "No transcript or notes mention it. Recordings that have not been \
                    transcribed are not searched."
        .to_owned();
    if not_downloaded > 0 {
        // **独立した文にする**。カンマで繋ぐと、件数が直前の「文字起こしされていない録音」に
        // 係って読める（実際は別の集合）。単複は `plural` に任せて言い回しを一覧側と揃える。
        let recordings = shoki_core::plural(not_downloaded as u64, "recording");
        let verb = if not_downloaded == 1 { "is" } else { "are" };
        body.push_str(&format!(
            " {recordings} {verb} not downloaded to this Mac, so what they say could not be \
             searched."
        ));
    }
    ("No matches", body)
}
