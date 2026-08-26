//! 退避されたファイル（クラウド管理で実体がディスクに無いもの）を、**取り寄せずに扱う**ための
//! スレッド設定（#178）。
//!
//! macOS は iCloud Drive などのファイルプロバイダが管理するファイルを `dataless` として残し、
//! `open` / `read` した時点で**同期で実体をダウンロード**する。ヘッダを数バイト読むだけの処理でも
//! ファイル全体が落ちてくるので、一覧の走査のように「触るだけで中身は要らない」経路では致命的に
//! なる——#178 では 11 セッション・82 MB を取り寄せて 97 秒かかり、その間ウィンドウが出なかった。
//!
//! `stat`（`metadata` / `is_file`）は取り寄せを起こさないので、有無やサイズの判定はそのままでよい。
//! 止めたいのは中身を読む経路だけ。

/// 取り寄せを止めている間だけ生きる番人。**落とすと元の設定へ戻る**。
///
/// スレッド単位なので、実体が要る操作（再生・文字起こし）が別スレッドで走っている間は影響しない。
/// 同じスレッドで後から走る処理を巻き込まないよう、`Drop` で必ず戻す。
///
/// 作れなかったときは `None`（設定できないだけで、走査自体は従来どおり動く）。macOS 以外は
/// 常に `None` ——取り寄せという概念が無いので、止めるものが無い。
pub struct MaterializationOff {
    #[cfg(target_os = "macos")]
    previous: std::ffi::c_int,
}

#[cfg(target_os = "macos")]
mod sys {
    use std::ffi::c_int;

    // `sys/resource.h` の値（公開 API。`docs/rules/ffi.md` の「private API は使わない」）。
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
    pub fn for_this_thread() -> Option<Self> {
        // Safety: 引数は上の定数だけで、ポインタを渡さない。どちらも libSystem の公開 API。
        let previous = unsafe {
            sys::getiopolicy_np(
                sys::IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                sys::IOPOL_SCOPE_THREAD,
            )
        };
        if previous < 0 {
            eprintln!(
                "Continuing to scan without the no-download policy because the current one could \
                 not be read: {}",
                std::io::Error::last_os_error().kind()
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
                "Continuing to scan without the no-download policy because it could not be set: {}",
                std::io::Error::last_os_error().kind()
            );
            return None;
        }
        Some(Self { previous })
    }
}

#[cfg(not(target_os = "macos"))]
impl MaterializationOff {
    /// 取り寄せという概念が無いので、止めるものが無い。
    pub fn for_this_thread() -> Option<Self> {
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
            // 戻せないと、このスレッドで後から走る処理まで取り寄せなくなる。握りつぶさない
            // （`docs/rules/error-handling.md`）。
            eprintln!(
                "The no-download policy could not be restored on this thread: {}",
                std::io::Error::last_os_error().kind()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MaterializationOff;

    /// 番人が生きている間だけ取り寄せが止まり、**落とすと元へ戻る**。
    ///
    /// 戻らないと、同じスレッドで後から走る処理（テストランナーは 1 スレッドに複数のテストを
    /// 載せる）まで巻き込む。
    #[cfg(target_os = "macos")]
    #[test]
    fn the_policy_goes_back_to_what_it_was() {
        use super::sys;

        let read = || unsafe {
            sys::getiopolicy_np(
                sys::IOPOL_TYPE_VFS_MATERIALIZE_DATALESS_FILES,
                sys::IOPOL_SCOPE_THREAD,
            )
        };

        let before = read();
        assert!(before >= 0, "the current policy should be readable");
        {
            let guard = MaterializationOff::for_this_thread();
            assert!(guard.is_some(), "the policy should be settable");
            assert_eq!(
                read(),
                sys::IOPOL_MATERIALIZE_DATALESS_FILES_OFF,
                "downloads are off while the guard is alive"
            );
        }
        assert_eq!(read(), before, "the previous policy comes back");
    }

    /// macOS 以外では止めるものが無い（`None` でも走査は従来どおり動く）。
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn there_is_nothing_to_turn_off() {
        assert!(MaterializationOff::for_this_thread().is_none());
    }
}
