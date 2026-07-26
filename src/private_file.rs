//! 機微ファイル（録音データとその派生物）を**所有者だけが読み書きできる**状態で書き出す。
//!
//! 録音・ミックス音声・文字起こし JSON・議事録 Markdown は、いずれも発話内容そのものを含む
//! （`docs/rules/security.md`）。Unix では 0600 で作る。セッションディレクトリ自体は録音側が
//! 0700 で作成する。
//!
//! **`OpenOptions::mode` は新規作成時にしか効かない**のが要点。既に緩いモードのファイルが在ると
//! （セッションを `cp -r` した・バックアップや zip から戻した等）、上書きしてもそのモードが
//! 残ってしまう。文字起こしのやり直しや議事録の再生成は既存ファイルを上書きする経路なので、
//! 開いた**後**にモードを設定し直す。設定はファイルハンドル経由なので、開いた後に差し替えられても
//! 別のファイルへ適用されることはない。

use std::io::Write;
use std::path::Path;

/// `path` を 0600 で開き（既存なら切り詰め、モードも 0600 へ揃える）、`data` を書き出す。
pub fn write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut file = create(path)?;
    file.write_all(data)
}

/// `path` を 0600 で開く（既存なら切り詰め、モードも 0600 へ揃える）。書き出しを分けたい
/// 呼び出し側（複数回 `write_all` する等）はこちらを使う。
pub fn create(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("shoki-private-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("the temp dir should be creatable");
        dir
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn writes_the_data() {
        let dir = temp_dir("write");
        let path = dir.join("secret.txt");
        write(&path, b"hello").expect("writing should succeed");
        assert_eq!(std::fs::read(&path).expect("readable"), b"hello");
        // 既存ファイルは切り詰められる（前の内容が末尾に残らない）。
        write(&path, b"hi").expect("overwriting should succeed");
        assert_eq!(std::fs::read(&path).expect("readable"), b"hi");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **このモジュールの存在理由**。新規作成だけでなく、緩いモードの既存ファイルを上書きする
    /// ときも 0600 へ揃うこと（`OpenOptions::mode` だけでは後者が漏れる）。
    #[cfg(unix)]
    #[test]
    fn tightens_the_mode_of_an_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = temp_dir("mode");
        let path = dir.join("secret.txt");
        std::fs::write(&path, b"world-readable").expect("the fixture should be writable");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("the fixture mode should be settable");
        assert_eq!(mode_of(&path), 0o644, "the fixture should start out loose");

        write(&path, b"secret").expect("writing should succeed");
        assert_eq!(mode_of(&path), 0o600, "overwriting must tighten the mode");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn creates_owner_only_files() {
        let dir = temp_dir("create");
        let path = dir.join("secret.txt");
        write(&path, b"secret").expect("writing should succeed");
        assert_eq!(mode_of(&path), 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
