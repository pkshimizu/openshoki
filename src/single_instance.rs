//! アプリの多重起動を防ぐ排他ロック。
//!
//! 常駐（メニューバー）アプリの openshoki が複数プロセス同時に動くと、トレイアイコンが重複し、
//! 自動録音が複数プロセスで同時発火して同じ音を二重録音する・保存先を奪い合う、といった不整合を
//! 招く。起動時にロックファイルへ排他ロックを取り、既に別インスタンスが保持していればこのプロセスは
//! 常駐を始めずに終了する。
//!
//! ロックは OS がプロセス終了・クラッシュ時に自動解放する（`fs2` の `flock`/`LockFileEx` ベース）ため、
//! 前回の残骸で起動不能になる stale lock は起きない。
//!
//! ガードのために起動不能にはしない: ロックファイルを用意できない異例環境（`ProjectDirs` 取得不可・
//! IO エラーなど）では排他を諦めて起動を続行する（`docs/rules/error-handling.md` の精神）。全 OS 共通で
//! 効かせるため `cfg` で囲まない。

use std::fs::File;
use std::path::Path;

use fs2::FileExt;

/// ロックファイル名。`config::cache_dir()`（`ProjectDirs` のキャッシュディレクトリ）配下に固定名で置く。
const LOCK_FILE_NAME: &str = "instance.lock";

/// 排他ロックの取得結果。
pub enum Acquire {
    /// ロックを取得できた（このプロセスが唯一のインスタンス）。保持している `File` を
    /// **プロセスの生存期間ずっと保持する**こと。drop（プロセス終了含む）で OS がロックを解放する。
    Acquired(File),
    /// 既に別インスタンスがロックを保持している。呼び出し側は常駐を始めず終了する。
    AlreadyRunning,
    /// ロックの仕組みを用意できなかった（異例環境）。多重起動ガード無しで起動を続行する。
    Unavailable,
}

/// 起動時に排他ロックを試みる。結果の扱いは [`Acquire`] の各バリアント参照。
pub fn acquire() -> Acquire {
    let Some(dir) = crate::config::cache_dir() else {
        eprintln!("Skipping the single-instance guard because the cache directory is unavailable.");
        return Acquire::Unavailable;
    };
    // ロックファイルは排他の目印だけで中身を持たない（機微データではない）ため、キャッシュ
    // ディレクトリは OS 既定パーミッションで作成してよい。
    if let Err(err) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "Skipping the single-instance guard because the cache directory could not be created: {err}"
        );
        return Acquire::Unavailable;
    }
    acquire_at(&dir.join(LOCK_FILE_NAME))
}

/// 指定パスのロックファイルへ排他ロックを試みる。`acquire` から切り出してテスト可能にした本体。
fn acquire_at(path: &Path) -> Acquire {
    let file = match File::create(path) {
        Ok(file) => file,
        Err(err) => {
            eprintln!(
                "Skipping the single-instance guard because the lock file could not be opened: {err}"
            );
            return Acquire::Unavailable;
        }
    };
    match file.try_lock_exclusive() {
        Ok(()) => Acquire::Acquired(file),
        // 競合（別インスタンスが保持中）は WouldBlock 相当のエラーで返る。
        Err(err) if err.kind() == fs2::lock_contended_error().kind() => Acquire::AlreadyRunning,
        // 競合以外のロック失敗は異例環境として、ガードを諦めて起動を続行する。
        Err(err) => {
            eprintln!(
                "Skipping the single-instance guard because the lock could not be acquired: {err}"
            );
            Acquire::Unavailable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Acquire, acquire_at};

    /// 同一パスへ 2 回ロックを試みると、1 回目は取得でき、2 回目は「既に動作中」になる。
    /// 1 回目のロックを保持したまま 2 回目を試すことで、多重起動の検知経路を再現する。
    #[test]
    fn second_lock_on_same_path_reports_already_running() {
        // 並列テスト間で衝突しないよう、プロセス固有名の一時ファイルを使う。
        let path = std::env::temp_dir().join(format!(
            "openshoki-instance-test-{}.lock",
            std::process::id()
        ));

        let first = acquire_at(&path);
        assert!(
            matches!(first, Acquire::Acquired(_)),
            "the first lock should be acquired"
        );

        // 1 つ目のロックを保持したまま 2 つ目を試す。
        let second = acquire_at(&path);
        assert!(
            matches!(second, Acquire::AlreadyRunning),
            "the second attempt should report already running"
        );

        // 1 つ目を解放すれば再取得できる（stale lock にならないことの確認）。
        drop(first);
        drop(second);
        let third = acquire_at(&path);
        assert!(
            matches!(third, Acquire::Acquired(_)),
            "the lock should be re-acquirable after release"
        );
        drop(third);

        let _ = std::fs::remove_file(&path);
    }
}
