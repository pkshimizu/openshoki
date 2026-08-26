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
//! `stat`（`metadata` / `is_file`）は取り寄せを起こさないので、有無やサイズの判定はそのままでよい。
//! 止めたいのは中身を読む経路だけ。実体が要る取り寄せ（再生・文字起こし）は別スレッドで走るので、
//! そちらは従来どおり落ちてくる。

/// 退避されたファイルを取り寄せない設定で `body` を走らせる（#178）。
///
/// **番人を呼び出し側に持たせない**。`let _ = MaterializationOff::for_this_thread();` と書くと
/// その場で落ちて何も止まらず、しかもコンパイルは通る（実際、レビュー前のミューテーションで
/// テストが素通りした）。閉包で受ければ、**効いている範囲が構造で決まる**。
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
pub fn without_downloads<T>(body: impl FnOnce() -> T) -> T {
    let _guard = MaterializationOff::for_this_thread();
    body()
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

/// いまのスレッドの設定を読む（テスト用）。
///
/// **取り寄せが止まっているかを確かめる側もここを通す**——このモジュールの外（走査が本当に
/// 止めた状態で測っているか。`recordings`）からも要る。
#[cfg(all(test, target_os = "macos"))]
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

    /// 閉包の中だけ取り寄せが止まり、**抜けると元へ戻る**。
    ///
    /// 戻らないと、同じスレッドで後から走る処理（テストランナーは 1 スレッドに複数のテストを
    /// 載せる）まで巻き込む。
    #[cfg(target_os = "macos")]
    #[test]
    fn downloads_are_off_inside_and_back_to_normal_outside() {
        let before = current_policy();
        assert!(before >= 0, "the current policy should be readable");
        let inside = without_downloads(current_policy);
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
        let caught = std::panic::catch_unwind(|| without_downloads(|| panic!("boom")));
        assert!(caught.is_err(), "the panic should come through");
        assert_eq!(
            current_policy(),
            before,
            "the previous policy comes back anyway"
        );
    }

    /// macOS 以外では止めるものが無い（設定できなくても本体は走る）。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn the_body_runs_even_with_nothing_to_turn_off() {
        assert_eq!(without_downloads(|| 42), 42);
    }
}
