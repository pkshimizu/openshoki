//! 一時ファイルへ書いてから rename で原子的に置き換える定型。モデルの取得
//! （`crate::model_download`）、録音の音量正規化（`crate::mixdown::normalize_if_quiet` が
//! `mic.mp3` / `system.mp3` を書き直す経路）、議事録の保存（`crate::summarize::write_summary`）で
//! 共用する。
//!
//! 途中で失敗しても、壊れた／部分的なファイルを成果物として残さない。**パニックで抜けても
//! 一時ファイルを残さない**（#130）: `commit` せずに drop されたら消す。これが無いと、
//! モデル取得なら最大 4.4GB（要約 LLM 7B）がユーザーの辿らないデータディレクトリへ残り続ける
//! （一時ファイル名はプロセス固有なので、次回起動でも上書き再利用されない）。
//!
//! 一時ファイル名を**プロセス固有**（`*.part.<pid>`）にしているのは、アプリの多重起動
//! （`single_instance` はロックが取れない環境で縮退する）が同名の一時ファイルへ同時に書くのを
//! 防ぐため。同名だと、各自が自分の受信ストリームでハッシュを計算するぶんファイルの破損を
//! 検知できず、壊れた内容が「検証済み」として配置されうる。名前を分ければ各自が自分の書いた
//! 内容だけを検証し、rename（原子的・後勝ち）はどちらも検証済みなので安全になる。
//!
//! Drop が走らない終わり方（`abort`・強制終了・電源喪失）では一時ファイルが残る。そちらの
//! 回収は `sweep_orphaned_parts`（判定と限界はその doc）。

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// 置き換え用の一時ファイル。`path()` へ書き、`commit()` で本来の名前へ rename する。
/// `commit()` せずに drop されたら（失敗・パニック）一時ファイルを消す。
///
/// `Debug` は付けない: 保持しているのはフルパスで、`{:?}` でログへ出すとユーザー名が漏れる
/// （`docs/rules/security.md`。ログに出すのはファイル名だけ）。
#[must_use = "the temporary file is removed when the guard is dropped; bind it"]
pub struct PartFile {
    part: PathBuf,
    dest: PathBuf,
    committed: bool,
}

impl PartFile {
    /// `dest` を置き換えるための一時ファイル名を決める（ファイル自体はまだ作らない）。
    /// 名前は `dest` に `.part.<pid>` を足したもの（元の拡張子は残す）。
    ///
    /// `dest` がファイル名で終わらない（`/`・`..` 等）場合は `None`。ディスクへ書く関数なので、
    /// 既定値で埋めて**別の場所**に一時ファイルを作るより、呼び出し側に失敗させる。
    #[must_use = "the returned guard owns the temporary file; bind it and commit() it"]
    pub fn for_dest(dest: &Path) -> Option<Self> {
        let mut name = dest.file_name()?.to_os_string();
        name.push(format!(".part.{}", std::process::id()));
        Some(Self {
            part: dest.with_file_name(name),
            dest: dest.to_path_buf(),
            committed: false,
        })
    }

    /// 書き込み先の一時ファイルのパス。
    pub fn path(&self) -> &Path {
        &self.part
    }

    /// 一時ファイルを `for_dest` で渡した宛先へ rename して番人を解除する。失敗した場合は
    /// 解除せず、一時ファイルは drop 時に消える。
    pub fn commit(mut self) -> std::io::Result<()> {
        std::fs::rename(&self.part, &self.dest)?;
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
        if let Err(err) = std::fs::remove_file(&self.part)
            && err.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("Failed to remove the partially written file: {err}");
        }
    }
}

/// 掃除の対象にする一時ファイルの絞り込み（`sweep_orphaned_parts`）。
///
/// 掃除の安全性は「対象ディレクトリを限定していること」に依るので、**ユーザーが中身を置ける
/// 場所を掃除するときは宛先名まで絞る**（アプリが書いた一時ファイルしか触らないことが名前で
/// 決まる）。アプリ専有の場所（モデルの保存先）は宛先名がカタログ次第で増えるため絞らない。
#[derive(Debug)]
pub enum PartScope<'a> {
    /// `*.part.<数字>` の形ならすべて。アプリ専有のディレクトリ用。
    AnyDest,
    /// この宛先名（`mic.mp3` など）の一時ファイルだけ。ユーザーの領域にあるディレクトリ用。
    Dests(&'a [&'a str]),
}

impl PartScope<'_> {
    /// この名前を掃除の対象にするか。
    fn covers(&self, name: &Path) -> bool {
        let Some(dest) = part_dest(name) else {
            return false;
        };
        match self {
            Self::AnyDest => true,
            Self::Dests(dests) => dests.contains(&dest),
        }
    }
}

/// `dir` の直下に取り残された一時ファイルを消す（消したファイルは 1 件ずつログに出る）。
///
/// `PartFile` の Drop が走らない終わり方（`abort`・強制終了・電源喪失）で残ったものが対象。
/// **走っている取得の一時ファイルは消さない**: 判定は「最終更新から `max_age` 以上経っている」
/// で、書き込み中のファイルは mtime が更新され続けるため引っかからない。多重起動した別プロセス
/// のものも同じ理由で安全（pid の生存確認は要らない）。`max_age` は受信全体のタイムアウトより
/// 長く取ること（そこを超えて進まない取得は失敗して自分で片付ける）。
///
/// `dir` 自身が**シンボリックリンク**なら何もしない（辿った先を掃除しない）。掃除の掛け先を
/// 増やすときに呼び出し側が忘れられないよう、ここで弾く。判定と列挙は別のパス解決なので
/// 差し替えの窓（TOCTOU）は残るが、掛け先はどちらも本人しか書けない場所（アプリのデータ
/// ディレクトリ／ユーザー自身の録音フォルダ）なので許容する。
///
/// **限界**: mtime で見るので、強制終了した**直後**に起動しても回収されない（`max_age` を
/// 過ぎてからの起動で回収される）。即時に回収するには一時ファイルのロック等で「書いていた
/// プロセスが生きているか」を直接見る必要があり、そこまではやらない（掃除が走らない間も、
/// 残骸は空き容量に反映されるので取得の可否判断は正しく働く）。
pub fn sweep_orphaned_parts(dir: &Path, now: SystemTime, max_age: Duration, scope: PartScope) {
    // リンクを辿らずにディレクトリであることを確かめる（無い場合も含めてここで抜ける）。
    if !dir.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        // 不在は上のガードで弾いているので、ここに来るのは判定の後に消えた場合（レース）と
        // 権限エラー。どちらも掃除しないだけで機能に影響しない。保存先のフルパスはログに
        // 出さない（`docs/rules/security.md`）。
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(err) => {
            eprintln!(
                "Skipping the cleanup of leftover temporary files because the folder could not be read: {err}"
            );
            return;
        }
    };

    for entry in entries {
        // 1 件読めなくても残りは掃除する（握りつぶさずログに残す。`docs/rules/error-handling.md`）。
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                eprintln!(
                    "Skipping an entry while cleaning up leftover temporary files because it could not be read: {err}"
                );
                continue;
            }
        };
        // 名前で先にふるう（安い。無関係なエントリに stat を打たない）。
        let name = entry.file_name();
        if !scope.covers(Path::new(&name)) {
            continue;
        }
        // 通常ファイルだけを対象にする。シンボリックリンクを辿って別の場所の mtime で
        // 判断したり、同名のディレクトリを毎起動 remove_file で失敗させたりしない
        // （`entry.metadata()` はリンクを辿らない）。
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(err) => {
                eprintln!(
                    "Skipping an entry that looks like a leftover temporary file because its metadata could not be read: {err}"
                );
                continue;
            }
        };
        if !metadata.is_file() || !is_older_than(&metadata, now, max_age) {
            continue;
        }
        let path = entry.path();
        match std::fs::remove_file(&path) {
            Ok(()) => {
                // 取り残しを黙って消さずログに残す（モデルなら 1 件で数 GB になりうる）。
                // ファイル名だけを出す（フルパスはユーザー名を含むため。
                // `docs/rules/security.md`）。
                println!(
                    "Removed a leftover temporary file: {}",
                    name.to_string_lossy()
                );
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => eprintln!("Failed to remove the leftover temporary file: {err}"),
        }
    }
}

/// `<宛先名>.part.<数字>`（`PartFile::for_dest` が作る形）か。書きかけの一時ファイルを
/// 成果物として扱わないために使う（モデル一覧が `models/` を走査するとき等。#117）。
pub(crate) fn is_part_file(path: &Path) -> bool {
    part_dest(path).is_some()
}

/// `<宛先名>.part.<数字>`（`PartFile::for_dest` が作る形）なら、その宛先名を返す。
/// 形が違えば `None`。
///
/// pid が数字であることまで見るのは、拡張子付きのユーザーファイル（`notes.part.txt` 等）を
/// 誤って掴まないための最低限のふるい。これだけでは分割書庫の慣習名（`archive.zip.part.1`）を
/// 見分けられないので、**安全性の主体は対象を限定していること**にある（`PartScope`）。掃除の
/// 掛け先は 2 つ: モデルの保存先（`model_download::sweep_orphaned_part_files`）と、Library
/// ウィンドウに出たセッションの直下（`recordings::spawn_session_part_sweep`）。範囲と時期の
/// 理由はそれぞれの doc。
fn part_dest(path: &Path) -> Option<&str> {
    let name = path.file_name().and_then(|name| name.to_str())?;
    let (rest, pid) = name.rsplit_once('.')?;
    let dest = rest.strip_suffix(".part")?;
    if dest.is_empty() || pid.is_empty() || !pid.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(dest)
}

/// 最終更新から `max_age` 以上経っているか。時刻が読めない・未来の時刻が入っている場合は
/// 「新しい」と見なす（消さない側に転ぶ）。
fn is_older_than(metadata: &std::fs::Metadata, now: SystemTime, max_age: Duration) -> bool {
    let Ok(modified) = metadata.modified() else {
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

    fn part_for(dest: &Path) -> PartFile {
        PartFile::for_dest(dest).expect("the fixture path has a file name")
    }

    /// 一時ファイル名は元の名前＋`.part.<pid>`（拡張子を落とさない）。
    #[test]
    fn for_dest_keeps_the_original_name() {
        let part = part_for(Path::new("/tmp/models/ggml-small.bin"));
        assert_eq!(
            part.path(),
            Path::new(&format!(
                "/tmp/models/ggml-small.bin.part.{}",
                std::process::id()
            ))
        );
        // drop で消そうとするが存在しないので NotFound（＝何も起きない）。
    }

    /// ファイル名で終わらない宛先は断る（既定値で埋めて別の場所へ書かない）。
    #[test]
    fn for_dest_rejects_paths_without_a_file_name() {
        assert!(PartFile::for_dest(Path::new("/")).is_none());
        assert!(PartFile::for_dest(Path::new("..")).is_none());
    }

    /// `commit` すれば rename され、一時ファイルは残らない。
    #[test]
    fn commit_renames_and_leaves_no_leftover() {
        let dir = temp_dir("commit");
        let dest = dir.join("model.bin");
        let part = part_for(&dest);
        let part_path = part.path().to_path_buf();
        std::fs::write(part.path(), b"payload").expect("writing the part file should succeed");

        part.commit().expect("renaming should succeed");
        assert_eq!(
            std::fs::read(&dest).expect("the destination should exist"),
            b"payload"
        );
        assert!(!part_path.exists(), "the part file should be gone");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `commit` が失敗したら番人は解除されず、一時ファイルは消える（縮退経路）。
    #[test]
    fn failed_commit_still_removes_the_part_file() {
        let dir = temp_dir("commit-fails");
        // 宛先が**ディレクトリ**なので、ファイルからの rename は失敗する。
        let dest = dir.join("model.bin");
        std::fs::create_dir(&dest).expect("creating the fixture directory should succeed");
        let part = part_for(&dest);
        let part_path = part.path().to_path_buf();
        std::fs::write(&part_path, b"partial").expect("writing the part file should succeed");

        part.commit()
            .expect_err("renaming a file onto a directory should fail");
        assert!(
            !part_path.exists(),
            "the part file should be removed even when the commit fails"
        );
        assert!(dest.is_dir(), "the destination should be untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `commit` せずに抜けたら（失敗・パニック相当）一時ファイルは消える。
    #[test]
    fn drop_without_commit_removes_the_part_file() {
        let dir = temp_dir("drop");
        let dest = dir.join("model.bin");
        let part = part_for(&dest);
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
        let part_path = part_for(&dest).path().to_path_buf();

        let unwound = std::panic::catch_unwind(|| {
            let part = part_for(&dest);
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

    /// 掃除は「一時ファイルの形」かつ「通常ファイル」かつ「十分に古い」ものだけを消す。
    #[test]
    fn sweep_removes_only_old_part_files() {
        const LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

        let dir = temp_dir("sweep");
        let old_part = dir.join("model.bin.part.123");
        let fresh_part = dir.join("other.bin.part.456");
        let model = dir.join("model.bin");
        let not_a_part = dir.join("notes.part.txt");
        for path in [&old_part, &fresh_part, &model, &not_a_part] {
            std::fs::write(path, b"x").expect("writing the fixture should succeed");
        }
        // 一時ファイルの名前を持つディレクトリ（remove_file では消せない）。
        let part_dir = dir.join("stale.bin.part.789");
        std::fs::create_dir(&part_dir).expect("creating the fixture directory should succeed");

        // 実ファイルの mtime は「今」なので、判定の現在時刻を未来へずらして経過を作る
        // （mtime を書き換える依存を足さずに決定的に検証する）。
        let elapsed = SystemTime::now() + LIMIT;
        sweep_orphaned_parts(&dir, elapsed, LIMIT, PartScope::AnyDest);
        // どちらの一時ファイルも上限を超えているので消える。
        assert!(!old_part.exists());
        assert!(!fresh_part.exists());
        // 一時ファイル以外は残る。
        assert!(model.exists(), "the model itself must not be touched");
        assert!(
            not_a_part.exists(),
            "a user file that merely contains .part must not be touched"
        );
        assert!(part_dir.is_dir(), "a directory must not be touched");

        // 書き込み中（mtime が新しい）の一時ファイルは消さない。
        std::fs::write(&fresh_part, b"x").expect("writing the fixture should succeed");
        sweep_orphaned_parts(&dir, SystemTime::now(), LIMIT, PartScope::AnyDest);
        assert!(
            fresh_part.exists(),
            "a part file that is still being written must be kept"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 境界: 経過がちょうど上限なら消す（`>=`。`>` に狭めると回収が 1 巡遅れる）。
    #[test]
    fn sweep_removes_a_part_file_exactly_at_the_limit() {
        const LIMIT: Duration = Duration::from_secs(24 * 60 * 60);

        let dir = temp_dir("sweep-boundary");
        let part = dir.join("model.bin.part.123");
        std::fs::write(&part, b"x").expect("writing the fixture should succeed");

        let just_at_limit = std::fs::metadata(&part)
            .and_then(|meta| meta.modified())
            .expect("the fixture should have an mtime")
            + LIMIT;
        sweep_orphaned_parts(&dir, just_at_limit, LIMIT, PartScope::AnyDest);
        assert!(!part.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// シンボリックリンクは消さない（`is_file()` が除く。`entry.metadata()` はリンクを辿らない
    /// のでリンク自身の属性で判断する）。`is_file()` の条件を外すとリンクが消えて落ちる。
    #[test]
    #[cfg(unix)]
    fn sweep_keeps_symlinks() {
        let dir = temp_dir("sweep-symlink");
        let target = dir.join("important.bin");
        std::fs::write(&target, b"keep me").expect("writing the fixture should succeed");
        let link = dir.join("victim.bin.part.111");
        std::os::unix::fs::symlink(&target, &link).expect("creating the symlink should succeed");

        // 古さの条件は満たす時刻を渡す（残る理由が「新しいから」ではないことを固定する）。
        let elapsed = SystemTime::now() + Duration::from_secs(48 * 60 * 60);
        sweep_orphaned_parts(&dir, elapsed, Duration::from_secs(60), PartScope::AnyDest);
        assert!(link.is_symlink(), "a symlink must not be swept");
        assert!(target.exists(), "the target must not be touched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 掃除するディレクトリ自身がシンボリックリンクなら、辿った先を掃除しない（掛け先が
    /// 増えても効くよう、判定は原始側が持つ。`recordings` 側の線引きはそちらのテスト）。
    #[test]
    #[cfg(unix)]
    fn sweep_does_not_follow_a_symlinked_directory() {
        let real = temp_dir("sweep-linked-target");
        let part = real.join("model.bin.part.123");
        std::fs::write(&part, b"x").expect("writing the fixture should succeed");
        let links = temp_dir("sweep-linked");
        let link = links.join("models");
        std::os::unix::fs::symlink(&real, &link).expect("creating the symlink should succeed");

        let elapsed = SystemTime::now() + Duration::from_secs(48 * 60 * 60);
        sweep_orphaned_parts(&link, elapsed, Duration::from_secs(60), PartScope::AnyDest);
        assert!(
            part.exists(),
            "a part file behind a symlinked directory must not be swept"
        );
        let _ = std::fs::remove_dir_all(&links);
        let _ = std::fs::remove_dir_all(&real);
    }

    /// mtime が未来（時計のずれ・別マシンからのコピー）の一時ファイルは消さない
    /// （経過を負として扱えないので「新しい」に転ばせる）。
    #[test]
    fn sweep_keeps_part_files_dated_in_the_future() {
        let dir = temp_dir("sweep-future");
        let part = dir.join("model.bin.part.123");
        std::fs::write(&part, b"x").expect("writing the fixture should succeed");

        let past = SystemTime::now() - Duration::from_secs(60 * 60);
        sweep_orphaned_parts(&dir, past, Duration::from_secs(1), PartScope::AnyDest);
        assert!(
            part.exists(),
            "a file modified after `now` must be treated as fresh"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ディレクトリが無くても（初回起動）静かに何もしない（パニックせずに戻る）。
    #[test]
    fn sweep_handles_a_missing_directory() {
        let missing = std::env::temp_dir().join(format!("shoki-part-none-{}", std::process::id()));
        assert!(!missing.exists());
        sweep_orphaned_parts(
            &missing,
            SystemTime::now(),
            Duration::from_secs(1),
            PartScope::AnyDest,
        );
        assert!(!missing.exists(), "the directory must not be created");
    }

    #[test]
    fn part_dest_wants_a_numeric_pid_and_returns_the_destination() {
        assert_eq!(
            part_dest(Path::new("/tmp/model.bin.part.1")),
            Some("model.bin")
        );
        assert_eq!(
            part_dest(Path::new("/tmp/mic.mp3.part.99999")),
            Some("mic.mp3")
        );
        // 旧形式（拡張子を落としていた頃の名前）も対象にする。
        assert_eq!(
            part_dest(Path::new("/tmp/ggml-small.part.42")),
            Some("ggml-small")
        );
        // pid が数字でない・無いもの、宛先名が無いものはユーザーのファイルとして扱う。
        assert_eq!(part_dest(Path::new("/tmp/notes.part.txt")), None);
        assert_eq!(part_dest(Path::new("/tmp/model.bin.part.")), None);
        assert_eq!(part_dest(Path::new("/tmp/model.bin")), None);
        assert_eq!(part_dest(Path::new("/tmp/part.123")), None);
        assert_eq!(part_dest(Path::new("/tmp/.part.123")), None);
    }

    /// 宛先名で絞ると、アプリが書いた名前以外の一時ファイルには触れない
    /// （ユーザーが中身を置ける場所を掃除するときの主柱）。
    #[test]
    fn sweep_with_named_dests_keeps_other_part_files() {
        let dir = temp_dir("sweep-dests");
        let ours = dir.join("mic.mp3.part.123");
        let theirs = dir.join("archive.zip.part.1");
        for path in [&ours, &theirs] {
            std::fs::write(path, b"x").expect("writing the fixture should succeed");
        }

        let elapsed = SystemTime::now() + Duration::from_secs(60 * 60);
        sweep_orphaned_parts(
            &dir,
            elapsed,
            Duration::from_secs(60),
            PartScope::Dests(&["mic.mp3"]),
        );
        assert!(!ours.exists(), "our own part file must be removed");
        assert!(
            theirs.exists(),
            "a part file we never write must be kept even when it looks like one"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
