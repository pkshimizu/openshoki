//! 退避されたファイル（クラウド管理で実体がディスクに無いもの）を、**取り寄せずに扱う**ための
//! スレッド設定（#178）。**この判断の背景の正はここ**（他は参照だけを置く）。
//!
//! 語は 1 つに読み替えてよい: OS の言う **materialize**、この doc の**取り寄せ**、ログの
//! **download** は同じことを指す（API 名が materialize、ユーザーに見える現象が download）。
//!
//! macOS は iCloud Drive などのファイルプロバイダが管理するファイルを `dataless` として残し、
//! `open` / `read` した時点で**同期で実体をダウンロード**する。ヘッダを数バイト読むだけの処理でも
//! ファイル全体が落ちてくるので、一覧の走査のように「触るだけで中身は要らない」経路では致命的に
//! なる——#178 では 11 セッション・82 MB を取り寄せて 97 秒かかり、その間ウィンドウが出なかった。
//!
//! **効くのは macOS だけ**。使うのが macOS 固有の I/O ポリシーなので、Windows の Files On-Demand や
//! Linux のクラウドマウントでは従来どおり取り寄せが走る（そちらを塞ぐ手は別に要る）。
//!
//! `stat`（`metadata` / `is_file`）は取り寄せを起こさないので、有無やサイズの判定はそのままでよい。
//! 止めたいのは中身を読む経路だけ。実体が要る取り寄せ（再生・文字起こし）は別スレッドで走るので、
//! そちらは従来どおり落ちてくる。

/// 退避されたファイルを取り寄せない設定で `body` を走らせる（#178）。
///
/// **番人を呼び出し側に持たせない**。`let _ = MaterializationOff::for_this_thread();` と書くと
/// その場で落ちて何も止まらず、しかもコンパイルは通る（実際、レビュー前のミューテーションで
/// テストが素通りした）。閉包で受ければ、**効いている範囲が構造で決まる**。
///
/// `body` には証（`NoDownloads`）を渡す。中身を読む関数にこれを要求させれば、囲いの外から
/// 読む書き方がコンパイルを通らなくなる（理由は `NoDownloads` の doc）。
///
/// 設定できなかったときも `body` は走る（取り寄せが起きて遅いだけで、結果は正しい）。macOS 以外は
/// 常にこの形になる。
///
/// **効くのは呼んだスレッドと、`body` が返るまでの間だけ**。呼び出し側の義務が 2 つある:
///
/// - `body` の中で別スレッドへ投げた読み取りには**効かない**（子スレッドはこの設定を継承しない。
///   実測で確認した）。走査を並列化するなら、各スレッドがそれぞれ通すこと。
/// - `body` は読み取りを**中で終わらせる**こと。開いた `File` や遅延イテレータを返すと、実際に
///   読むのは設定が戻った後になり、そこで取り寄せが起きる。
pub fn without_downloads<T>(body: impl FnOnce(&NoDownloads) -> T) -> T {
    let _guard = MaterializationOff::for_this_thread();
    body(&NoDownloads(std::marker::PhantomData))
}

/// 「取り寄せを止めた中にいる」ことの証（#178）。**`without_downloads` の中でしか作れない**
/// （フィールドが非公開なので、このモジュールの外では構築できない）。
///
/// 中身を読む関数にこれを要求させると、**囲いの外から呼ぶ書き方がコンパイルを通らなくなる**。
/// テストで守ろうとすると、テストが見る入口と本番が通る入口がずれた瞬間に素通りする——#178 の
/// レビューでは、その形の穴が 4 度続けて残った（束縛の書き方・囲いを剥がす・番人を通らない
/// 双子・走査の外で測る）。
///
/// **保証するのは「頼んだこと」だけ**。OS が実際に止めたかは別で、macOS 以外や設定に失敗した
/// ときは何も止まっていない（`MaterializationOff`）。それでも「囲いの外で読まない」という
/// 呼び出し側の規律は、この型が守る。
///
/// **スレッドをまたげない**（`PhantomData<*const ()>` で `Send` も `Sync` も付かない）。設定は
/// スレッド単位なので、子スレッドへ証を持ち込めると「囲いの中のつもりで、実際は止まっていない」
/// 読み取りが書けてしまう。
pub struct NoDownloads(std::marker::PhantomData<*const ()>);

/// 読み取りに取り寄せを許すかどうか（#182）。**呼び出し側が必ずどちらかを選ぶ**——
/// 「止める版」と「止めない版」の双子を作ると、片方だけが守られて、もう片方を呼ぶ 1 行で
/// 囲いを素通りできる（`docs/rules/testing.md` の「テストが見ている入口と、本番が通る入口を
/// ずらさない」。#178 で実際に起きた形）。
///
/// **選び間違いは、選んだ側では止められない**。`blocked` は証を要求するので囲いの外からは
/// 作れないが、`allowed` はどこからでも書ける——囲いの中の読み取り 1 つだけをそちらへ
/// 差し替える形は、型でも警告でも止まらない。**だから `allowed` 自身がスレッドを見る**:
/// 取り寄せを止めているスレッドで作られたものは、証が無くても止まっている側として扱う。
/// 間違えても実害（退避された録音が「壊れている」に化けて、打鍵のたびにログが埋まり、
/// 読めなかった件数が 0 になる）は出ない。
#[derive(Clone, Copy)]
pub struct Fetch<'a>(Mode<'a>);

#[derive(Clone, Copy)]
enum Mode<'a> {
    /// ユーザーが明示的に頼んだ読み取り（＝下の 2 つ以外すべて）。退避されていれば取り寄せる。
    Allowed,
    /// **頼まれていない読み取り**。退避されたものは読まずに諦める。いまは検索
    /// （`search_sessions`）だけ——一覧の走査（`recordings::scan_sessions`）は同じ判断だが、
    /// `Fetch` を通さず証を直接受ける（`recordings::Measured` の doc）。
    ///
    /// 証は**値としては使わない**。効くのは型で、`'a` が証の借用に縛られること自体が
    /// 「囲いの中にいる」を意味する（証は `without_downloads` の中でしか作れず、`!Send`
    /// なので子スレッドへも持ち込めない）。
    Blocked(
        #[expect(dead_code, reason = "the proof is a type-level witness, never read")]
        &'a NoDownloads,
    ),
    /// 「頼んだつもり」で作られたが、**スレッドが取り寄せを止めている**。証は無いので
    /// `Blocked` とは別に持つが、扱いは同じ（`Fetch::allowed` の doc）。
    BlockedByThread,
}

impl Fetch<'_> {
    /// ユーザーが明示的に頼んだ読み取り（再生・文字起こし・議事録の生成・録音を選ぶ）。
    ///
    /// **取り寄せを止めているスレッドでは、頼んだつもりでも止まっている**。そこで作られた
    /// ものは `BlockedByThread` になる（理由は `Fetch` の doc）。判定はここで 1 度だけ——
    /// 読み取りのたびに OS へ聞くと、打鍵ごとに全件を読む経路で無駄が積む。
    pub fn allowed() -> Fetch<'static> {
        if downloads_are_off() {
            return Fetch(Mode::BlockedByThread);
        }
        Fetch(Mode::Allowed)
    }
}

impl<'a> Fetch<'a> {
    /// 頼まれていない読み取り（証が要るので、囲いの外からは作れない）。
    pub fn blocked(downloads_off: &'a NoDownloads) -> Self {
        Self(Mode::Blocked(downloads_off))
    }

    /// 取り寄せが止まっているか。
    fn blocks_downloads(self) -> bool {
        match self.0 {
            Mode::Allowed => false,
            Mode::Blocked(_) | Mode::BlockedByThread => true,
        }
    }
}

/// このスレッドが取り寄せを止めているか（#182）。macOS 以外と、設定を読めないときは `false`
/// ——止まっていないものとして扱う（読めなかった理由を「実体が無い」と決めつけない）。
fn downloads_are_off() -> bool {
    #[cfg(target_os = "macos")]
    {
        current_policy() == sys::IOPOL_MATERIALIZE_DATALESS_FILES_OFF
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// 読み取りの失敗が「実体がこの Mac に無いから」か（#178 で見分け方を確かめ、#182 で
/// 共有した）。**この見分け方の正はここ**（他は参照だけを置く）。
///
/// `EDEADLK`（`Deadlock`）は「取り寄せない設定なので実体を用意できない」という macOS の
/// 返し方。実測では、退避された音源は `open` が通って `read` がこれで返る。**`open` 側で
/// 返す環境もありうる**ので、開くときも読むときも同じ見分けを通すこと。
///
/// **`Fetch::Allowed` の読み取りでは起きない**（取り寄せが走るので、遅いだけで読める）。
/// ただし `MaterializationOff` の復元に失敗したスレッドは取り寄せ off のままなので、
/// その後の通常の読み取りもこれで返りうる（`Drop` の分岐）。
///
/// **これで拾えない退避の形がある**。ファイルプロバイダが実体を `.icloud` などの別名の
/// プレースホルダへ置き換える運用では、元の名前は `NotFound` になる——「まだ作っていない」と
/// 区別が付かない。そこは拾えないものとして諦める（`stat` で有無を見ている走査
/// (`recordings::scan_sessions`) も同じ前提に立っている）。
pub fn is_not_downloaded(kind: std::io::ErrorKind) -> bool {
    kind == std::io::ErrorKind::Deadlock
}

impl Fetch<'_> {
    /// 読み取りの失敗を 3 つに分ける（#182）。**ログするかどうかまでここで決まる**。
    ///
    /// 分けそこねると、退避された保存先で打鍵のたびに全件ぶんのログが出るか、実体が無い
    /// ことが「読めなかった」に化けて検索から静かに消えるかの、どちらかになる。
    ///
    /// **取り寄せが止まっていなければ `NotDownloaded` は返さない**。取り寄せが走るので、
    /// その理由では失敗しない——例外は `MaterializationOff` の復元に失敗したスレッドだが、
    /// そこは `Fetch::allowed` がスレッドを見て `BlockedByThread` に倒す。
    pub fn classify(self, kind: std::io::ErrorKind) -> ReadFailure {
        if kind == std::io::ErrorKind::NotFound {
            return ReadFailure::NotCreated;
        }
        if self.blocks_downloads() && is_not_downloaded(kind) {
            return ReadFailure::NotDownloaded;
        }
        ReadFailure::Failed
    }
}

/// 読み取りが失敗した理由（#182）。**「待てば読める」と「待っても直らない」を分ける**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadFailure {
    /// まだ作られていない（未生成。正常な縮退なのでログしない）。
    NotCreated,
    /// 実体がこの Mac に無い。**ログしない**——退避された保存先では全件が該当するので、
    /// 打鍵のたびにログが埋まる。何件諦めたかは呼び出し側がまとめて 1 行にする。
    NotDownloaded,
    /// 権限・I/O・破損など。**ログする**（待っても直らないので、調査の手掛かりを残す）。
    Failed,
}

impl ReadFailure {
    /// この失敗をログに残すか（#182）。
    ///
    /// **残すのは待っても直らないものだけ**——未生成は正常な縮退で、実体が無いのは退避された
    /// 保存先では全件が該当する（何件諦めたかは呼び出し側がまとめて 1 行にする）。
    ///
    /// **頼まれていない読み取りでは、待っても直らないものも残さない**。検索は打鍵のたびに
    /// 全件を開き直すので、壊れた JSON が 1 つあるだけでログが埋まる。同じファイルは
    /// ユーザーがその録音を開いたときに取り寄せありで読まれ、そこで 1 度だけ残る
    /// ——調べたい人が見るのはそちら。
    pub fn should_report(self, fetch: Fetch) -> bool {
        match self {
            Self::NotCreated | Self::NotDownloaded => false,
            Self::Failed => !fetch.blocks_downloads(),
        }
    }
}

/// 取り寄せを止めている間だけ生きる番人。
///
/// スレッド単位なので、実体が要る操作（再生・文字起こし）が別スレッドで走っている間は影響しない。
///
/// **`Drop` で元の設定へ戻そうとする**。戻せなかったときの挙動と、それがまず起きない理由は
/// `Drop` の分岐にある。
///
/// 作れなかったときは `None`（設定できないだけで、走査自体は従来どおり動く）。macOS 以外は
/// 常に `None` ——取り寄せという概念が無いので、止めるものが無い。
struct MaterializationOff {
    #[cfg(target_os = "macos")]
    previous: std::ffi::c_int,
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::c_int;

    // `sys/resource.h` の値（公開 API。`docs/rules/ffi.md` の「private API は使わない」）。
    // **ヘッダの行をそのまま引く**——後ろ 2 つは値が同じ 1 なので、取り違えてもコンパイルも
    // テストも通る。
    //
    //     #define IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES 3
    //     #define IOPOL_SCOPE_THREAD    1
    //     #define IOPOL_MATERIALIZE_DATALESS_FILES_OFF     1
    pub const IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES: c_int = 3;
    pub const IOPOL_SCOPE_THREAD: c_int = 1;
    pub const IOPOL_MATERIALIZE_DATALESS_FILES_OFF: c_int = 1;

    unsafe extern "C" {
        /// いまの設定を返す。失敗時は負値（`errno` に理由が入る）。
        pub fn getiopolicy_np(iotype: c_int, scope: c_int) -> c_int;
        /// 設定する。成功で 0、失敗で非 0（`errno` に理由が入る）。
        pub fn setiopolicy_np(iotype: c_int, scope: c_int, policy: c_int) -> c_int;
    }
}

#[cfg(target_os = "macos")]
impl MaterializationOff {
    /// このスレッドで、退避されたファイルの取り寄せを止める。
    ///
    /// **元の設定を控えてから変える**。既定へ決め打ちで戻すと、呼び出し側より外で設定されていた
    /// 値を踏み潰す。
    fn for_this_thread() -> Option<Self> {
        // Safety: 引数は上の定数だけで、ポインタを渡さない。どちらも libSystem の公開 API。
        let previous = unsafe {
            sys::getiopolicy_np(
                sys::IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                sys::IOPOL_SCOPE_THREAD,
            )
        };
        if previous < 0 {
            eprintln!(
                "Continuing without the no-download policy because the current I/O policy could \
                 not be read: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        // Safety: 同上。
        let result = unsafe {
            sys::setiopolicy_np(
                sys::IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                sys::IOPOL_SCOPE_THREAD,
                sys::IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
            )
        };
        if result != 0 {
            eprintln!(
                "Continuing without the no-download policy because it could not be set: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        Some(Self { previous })
    }
}

#[cfg(not(target_os = "macos"))]
impl MaterializationOff {
    /// 取り寄せという概念が無いので、止めるものが無い。
    fn for_this_thread() -> Option<Self> {
        None
    }
}

#[cfg(target_os = "macos")]
impl Drop for MaterializationOff {
    fn drop(&mut self) {
        // Safety: `for_this_thread` と同じ公開 API を、控えておいた値で呼び戻すだけ。
        let result = unsafe {
            sys::setiopolicy_np(
                sys::IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                sys::IOPOL_SCOPE_THREAD,
                self.previous,
            )
        };
        if result != 0 {
            // 戻せないと、**このスレッドで後から走る実体の読み取りが縮退する**（一覧を開いた
            // 後の再生対象のロードなどが `EDEADLK` で静かに失敗する）。握りつぶさない
            // （`docs/rules/error-handling.md`）。`previous` は直前に読んだ値なので、実際に
            // ここへ来ることはまず無い。
            eprintln!(
                "Leaving the no-download policy on this thread because it could not be restored: \
                 {}",
                std::io::Error::last_os_error()
            );
        }
    }
}

/// いまのスレッドの設定を読む。
///
/// **取り寄せが止まっているかを確かめる側もここを通す**——`Fetch::allowed` が「頼んだつもり」を
/// 見分けるのに使い、テスト（走査が本当に止めた状態で測っているか。`recordings`）からも要る。
#[cfg(target_os = "macos")]
pub fn current_policy() -> std::ffi::c_int {
    // Safety: 引数は定数だけで、ポインタを渡さない（`for_this_thread` と同じ）。
    unsafe {
        sys::getiopolicy_np(
            sys::IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
            sys::IOPOL_SCOPE_THREAD,
        )
    }
}

/// 取り寄せが止まっている状態の値（テストの期待値）。
#[cfg(all(test, target_os = "macos"))]
pub const DOWNLOADS_OFF: std::ffi::c_int = sys::IOPOL_MATERIALIZE_DATALESS_FILES_OFF;

#[cfg(test)]
mod tests {
    use super::without_downloads;
    #[cfg(target_os = "macos")]
    use super::{DOWNLOADS_OFF, current_policy};
    use super::{Fetch, ReadFailure};

    /// 閉包の中だけ取り寄せが止まり、**抜けると元へ戻る**。
    ///
    /// 戻らないと、同じスレッドで後から走る処理（テストランナーは 1 スレッドに複数のテストを
    /// 載せる）まで巻き込む。
    #[cfg(target_os = "macos")]
    #[test]
    fn downloads_are_off_inside_and_back_to_normal_outside() {
        let before = current_policy();
        assert!(before >= 0, "the current policy should be readable");
        let inside = without_downloads(|_| current_policy());
        assert_eq!(
            inside, DOWNLOADS_OFF,
            "downloads are off while the body runs"
        );
        assert_eq!(current_policy(), before, "the previous policy comes back");
    }

    /// 中で panic しても元へ戻す（番人が `Drop` で戻すので、巻き戻しでも効く）。
    #[cfg(target_os = "macos")]
    #[test]
    fn a_panic_inside_still_restores_the_policy() {
        let before = current_policy();
        let caught = std::panic::catch_unwind(|| without_downloads(|_| panic!("boom")));
        assert!(caught.is_err(), "the panic should come through");
        assert_eq!(
            current_policy(),
            before,
            "the previous policy comes back anyway"
        );
    }

    /// 「実体が無い」の見分けは、**取り寄せを止めているときだけ**（#182）。
    ///
    /// `Allowed` でも `Deadlock` を「実体が無い」と読むと、取り寄せを頼んだ読み取りの失敗が
    /// 黙って対象外へ落ちる（画面には「読めなかった」も出ない）。逆に `Blocked` で見分けを
    /// やめると、退避された録音が「当たらなかった」に化けて、検索から静かに消える。
    ///
    /// **未生成はどちらでも同じ**（作っていないだけなので、ログもしない）。
    #[test]
    fn only_a_blocked_read_can_blame_a_missing_body() {
        use std::io::ErrorKind;

        // 頼まれていない読み取り（囲いの中でしか作れない）。
        without_downloads(|downloads_off| {
            let blocked = Fetch::blocked(downloads_off);
            assert_eq!(
                blocked.classify(ErrorKind::Deadlock),
                ReadFailure::NotDownloaded
            );
            // 他の失敗は「実体が無い」ではない（待っても直らないのでログする）。
            assert_eq!(
                blocked.classify(ErrorKind::PermissionDenied),
                ReadFailure::Failed
            );
            assert_eq!(
                blocked.classify(ErrorKind::InvalidData),
                ReadFailure::Failed
            );
            // 未生成は静かな縮退。
            assert_eq!(
                blocked.classify(ErrorKind::NotFound),
                ReadFailure::NotCreated
            );
        });

        // ユーザーが頼んだ読み取りでは、どの失敗も「実体が無い」とは言わない。
        assert_eq!(
            Fetch::allowed().classify(ErrorKind::Deadlock),
            ReadFailure::Failed
        );
        assert_eq!(
            Fetch::allowed().classify(ErrorKind::PermissionDenied),
            ReadFailure::Failed
        );
        assert_eq!(
            Fetch::allowed().classify(ErrorKind::NotFound),
            ReadFailure::NotCreated
        );
    }

    /// **「頼んだつもり」でも、スレッドが止めているなら止まっている**（#182）。
    ///
    /// `Fetch::blocked` は証が要るので囲いの外からは作れないが、`Fetch::allowed()` は
    /// どこからでも書ける——囲いの中の読み取り 1 つだけをそちらへ差し替える形は、型でも
    /// 警告でも止められない。**そこで作られたものが「取り寄せる」ふりをしないこと**が、
    /// この機構の最後の砦になる（差し替えても、退避された録音は「壊れている」に化けず、
    /// 打鍵のたびにログも出ない）。
    #[cfg(target_os = "macos")]
    #[test]
    fn asking_to_download_inside_the_guard_still_does_not() {
        use std::io::ErrorKind;

        // 囲いの外なら、頼んだとおり取り寄せる側。
        assert!(!Fetch::allowed().blocks_downloads());
        assert_eq!(
            Fetch::allowed().classify(ErrorKind::Deadlock),
            ReadFailure::Failed,
            "a read that may download does not blame a missing body"
        );

        without_downloads(|_| {
            // **囲いの中で作っても止まっている**。証を持たないだけで、扱いは `blocked` と同じ。
            assert!(Fetch::allowed().blocks_downloads());
            assert_eq!(
                Fetch::allowed().classify(ErrorKind::Deadlock),
                ReadFailure::NotDownloaded
            );
            assert!(
                !ReadFailure::Failed.should_report(Fetch::allowed()),
                "a search must not fill the log even if someone asks to download"
            );
        });
    }

    /// **ログに残すのは、待っても直らないものだけ**（#182）。しかも**頼まれた読み取りだけ**。
    ///
    /// 実体が無いだけのものまでログすると、退避された保存先では打鍵のたびに全件ぶんの行が
    /// 出て、本当に調べたい失敗が埋もれる（#178 で一覧側が同じ形になり、件数のまとめ 1 行へ
    /// 倒した）。壊れた JSON も同じで、検索は打鍵のたびに全件を開き直すので 1 つあるだけで
    /// ログが埋まる——そちらはユーザーがその録音を開いたときに 1 度だけ残せばよい。
    /// 逆に `Fetch::Allowed` の `Failed` を黙らせると、権限や破損の手掛かりが消える。
    #[test]
    fn only_what_will_not_fix_itself_goes_to_the_log() {
        // ユーザーが頼んだ読み取り（開いた録音・議事録の生成）。
        assert!(ReadFailure::Failed.should_report(Fetch::allowed()));
        assert!(!ReadFailure::NotDownloaded.should_report(Fetch::allowed()));
        assert!(!ReadFailure::NotCreated.should_report(Fetch::allowed()));

        // 頼まれていない読み取り（検索）。**1 行も出さない**。
        without_downloads(|downloads_off| {
            let blocked = Fetch::blocked(downloads_off);
            for failure in [
                ReadFailure::Failed,
                ReadFailure::NotDownloaded,
                ReadFailure::NotCreated,
            ] {
                assert!(
                    !failure.should_report(blocked),
                    "a search reads every recording on every keystroke"
                );
            }
        });
    }

    /// macOS 以外では止めるものが無い（設定できなくても本体は走る）。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_body_runs_even_with_nothing_to_turn_off() {
        assert_eq!(without_downloads(|_| 42), 42);
    }
}
