//! 一時ファイルへ書いてから rename で原子的に置き換える定型（モデルの取得・ミックス音声の
//! 上書きで共用）。
//!
//! 途中で失敗しても、壊れた／部分的なファイルを成果物として残さない。**パニックで抜けても
//! 一時ファイルを残さない**（#130）: `commit` せずに drop されたら消す。これが無いと、
//! 数 GB（要約 LLM は最大 4.4GB、ミックスは 1 時間で 1.4GB 級）がユーザーの気づかない場所へ
//! 残り続ける（一時ファイル名はプロセス固有なので、次回起動でも上書き再利用されない）。
//!
//! 一時ファイル名を**プロセス固有**（`*.part.<pid>`）にしているのは、アプリの多重起動
//! （`single_instance` はロックが取れない環境で縮退する）が同名の一時ファイルへ同時に書くのを
//! 防ぐため。同名だと、各自が自分の受信ストリームでハッシュを計算するぶんファイルの破損を
//! 検知できず、壊れた内容が「検証済み」として配置されうる。名前を分ければ各自が自分の書いた
//! 内容だけを検証し、rename（原子的・後勝ち）はどちらも検証済みなので安全になる。
//!
//! Drop が走らない終わり方（`abort`・強制終了・電源喪失）では一時ファイルが残る。そちらは
//! `sweep_orphaned_parts` が次回以降に回収する。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 置き換え用の一時ファイル。`path()` へ書き、`commit()` で本来の名前へ rename する。
/// `commit()` せずに drop されたら（失敗・パニック）一時ファイルを消す。
#[must_use = "the temporary file is removed when the guard is dropped; bind it"]
pub struct PartFile {
    path: PathBuf,
    committed: bool,
}

impl PartFile {
    /// `dest` を置き換えるための一時ファイル名を決める（ファイル自体はまだ作らない）。
    /// 名前は `dest` に `.part.<pid>` を足したもの（元の拡張子は残す）。
    pub fn for_dest(dest: &Path) -> Self {
        let mut name = dest.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".part.{}", std::process::id()));
        Self {
            path: dest.with_file_name(name),
            committed: false,
        }
    }

    /// 書き込み先の一時ファイルのパス。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 一時ファイルを `dest` へ rename して番人を解除する。失敗した場合は解除せず、
    /// 一時ファイルは drop 時に消える。
    pub fn commit(mut self, dest: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, dest)?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for PartFile {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // 後始末の失敗も黙って捨てない（`docs/rules/error-handling.md`）。一度も作られて
        // いなければ NotFound になるので、それは正常として扱う。
        if let Err(err) = std::fs::remove_file(&self.path)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("Failed to remove the partially written file: {err}");
        }
    }
}

/// `dir` の直下に取り残された一時ファイルを消す（消した数を返す）。
///
/// `PartFile` の Drop が走らない終わり方（`abort`・強制終了・電源喪失）で残ったものが対象。
/// **走っている取得の一時ファイルは消さない**: 判定は「最終更新から `max_age` 以上経っている」
/// で、書き込み中のファイルは mtime が更新され続けるため引っかからない。多重起動した別プロセス
/// のものも同じ理由で安全（pid の生存確認は要らない）。`max_age` は受信全体のタイムアウトより
/// 十分に長く取ること（そこを超えて無反応な取得は失敗して自分で片付ける）。
pub fn sweep_orphaned_parts(dir: &Path, now: SystemTime, max_age: Duration) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // ディレクトリが無い（初回起動）・読めないのは掃除しないだけで機能に影響しない。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return 0,
        Err(err) => {
            eprintln!(
                "Skipping the cleanup of leftover temporary files because {dir:?} could not be read: {err}"
            );
            return 0;
        }
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if !is_part_file(&path) {
            continue;
        }
        if !is_older_than(&path, now, max_age) {
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => {
                // 数 GB を回収する操作なので、黙って消さずログに残す（パスはデータ
                // ディレクトリ配下のモデル名で、機微情報ではない）。
                println!("Removed a leftover temporary file: {}", path.display());
                removed += 1;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!("Failed to remove the leftover temporary file: {err}"),
        }
    }
    removed
}

/// `*.part.<数字>` という名前か（`PartFile::for_dest` が作る形）。
///
/// 数字まで見るのは、ユーザーが置いた別物（`notes.part.txt` 等）を消さないため。
fn is_part_file(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    let Some((rest, pid)) = name.rsplit_once('.') else {
        return false;
    };
    rest.ends_with(".part") && !pid.is_empty() && pid.chars().all(|c| c.is_ascii_digit())
}

/// 最終更新から `max_age` 以上経っているか。時刻が読めない・未来の時刻が入っている場合は
/// 「新しい」と見なす（消さない側に転ぶ）。
fn is_older_than(path: &Path, now: SystemTime, max_age: Duration) -> bool {
    let Ok(modified) = std::fs::metadata(path).and_then(|meta| meta.modified()) else {
        return false;
    };
    now.duration_since(modified)
        .is_ok_and(|elapsed| elapsed >= max_age)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("shoki-part-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("creating the temp dir should succeed");
        dir
    }

    /// 一時ファイル名は元の名前＋`.part.<pid>`（拡張子を落とさない）。
    #[test]
    fn for_dest_keeps_the_original_name() {
        let part = PartFile::for_dest(Path::new("/tmp/models/ggml-small.bin"));
        assert_eq!(
            part.path(),
            Path::new(&format!(
                "/tmp/models/ggml-small.bin.part.{}",
                std::process::id()
            ))
        );
        // drop で消そうとするが存在しないので NotFound（＝何も起きない）。
    }

    /// `commit` すれば rename され、一時ファイルは残らない。
    #[test]
    fn commit_renames_and_leaves_no_leftover() {
        let dir = temp_dir("commit");
        let dest = dir.join("model.bin");
        let part = PartFile::for_dest(&dest);
        let part_path = part.path().to_path_buf();
        std::fs::write(part.path(), b"payload").expect("writing the part file should succeed");

        part.commit(&dest).expect("renaming should succeed");
        assert_eq!(
            std::fs::read(&dest).expect("the destination should exist"),
            b"payload"
        );
        assert!(!part_path.exists(), "the part file should be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `commit` せずに抜けたら（失敗・パニック相当）一時ファイルは消える。
    #[test]
    fn drop_without_commit_removes_the_part_file() {
        let dir = temp_dir("drop");
        let dest = dir.join("model.bin");
        let part = PartFile::for_dest(&dest);
        let part_path = part.path().to_path_buf();
        std::fs::write(part.path(), b"partial").expect("writing the part file should succeed");

        drop(part);
        assert!(!part_path.exists(), "the part file should be removed");
        assert!(!dest.exists(), "the destination should not be created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// パニックで unwind しても消える（番人の存在理由。#130）。
    #[test]
    fn panic_removes_the_part_file() {
        let dir = temp_dir("panic");
        let dest = dir.join("model.bin");
        let part_path = PartFile::for_dest(&dest).path().to_path_buf();

        let unwound = std::panic::catch_unwind(|| {
            let part = PartFile::for_dest(&dest);
            std::fs::write(part.path(), b"partial").expect("writing the part file should succeed");
            panic!("boom");
        });
        assert!(unwound.is_err(), "the closure should have panicked");
        assert!(
            !part_path.exists(),
            "unwinding should have removed the part file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 掃除は「一時ファイルの形」かつ「十分に古い」ものだけを消す。
    #[test]
    fn sweep_removes_only_old_part_files() {
        let dir = temp_dir("sweep");
        let old_part = dir.join("model.bin.part.123");
        let fresh_part = dir.join("other.bin.part.456");
        let model = dir.join("model.bin");
        let not_a_part = dir.join("notes.part.txt");
        for path in [&old_part, &fresh_part, &model, &not_a_part] {
            std::fs::write(path, b"x").expect("writing the fixture should succeed");
        }

        // 実ファイルの mtime は「今」なので、判定の現在時刻を未来へずらして経過を作る
        // （mtime を書き換える依存を足さずに決定的に検証する）。
        let now = SystemTime::now() + Duration::from_secs(48 * 60 * 60);
        assert_eq!(
            sweep_orphaned_parts(&dir, now, Duration::from_secs(24 * 60 * 60)),
            2,
            "both part files are older than the limit at that point in time"
        );
        assert!(!old_part.exists());
        assert!(!fresh_part.exists());
        // 一時ファイル以外は残る。
        assert!(model.exists(), "the model itself must not be touched");
        assert!(
            not_a_part.exists(),
            "a user file that merely contains .part must not be touched"
        );

        // 書き込み中（mtime が新しい）の一時ファイルは消さない。
        std::fs::write(&fresh_part, b"x").expect("writing the fixture should succeed");
        assert_eq!(
            sweep_orphaned_parts(&dir, SystemTime::now(), Duration::from_secs(24 * 60 * 60)),
            0,
            "a part file that is still being written must be kept"
        );
        assert!(fresh_part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ディレクトリが無くても（初回起動）静かに何もしない。
    #[test]
    fn sweep_handles_a_missing_directory() {
        let missing = std::env::temp_dir().join(format!("shoki-part-none-{}", std::process::id()));
        assert!(!missing.exists());
        assert_eq!(
            sweep_orphaned_parts(&missing, SystemTime::now(), Duration::from_secs(1)),
            0
        );
    }

    #[test]
    fn is_part_file_wants_a_numeric_pid() {
        assert!(is_part_file(Path::new("/tmp/model.bin.part.1")));
        assert!(is_part_file(Path::new("/tmp/mix.mp3.part.99999")));
        // 旧形式（拡張子を落としていた頃の名前）も対象にする。
        assert!(is_part_file(Path::new("/tmp/ggml-small.part.42")));
        // pid が数字でない・無いものはユーザーのファイルとして扱う。
        assert!(!is_part_file(Path::new("/tmp/notes.part.txt")));
        assert!(!is_part_file(Path::new("/tmp/model.bin.part.")));
        assert!(!is_part_file(Path::new("/tmp/model.bin")));
        assert!(!is_part_file(Path::new("/tmp/part.123")));
    }
}
