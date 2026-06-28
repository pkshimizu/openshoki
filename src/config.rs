//! ユーザー設定の永続化。
//!
//! 録音ファイルの保存先などのユーザー設定を、OS 標準の設定ディレクトリに TOML で保存・復元する。
//! 読み込みに失敗した場合（ファイル無し・破損・ディレクトリ取得不可）はデフォルトへ
//! フォールバックし、アプリ（常駐）は落とさない（`docs/rules/error-handling.md`）。

use std::path::PathBuf;

use directories::{ProjectDirs, UserDirs};
use serde::{Deserialize, Serialize};

/// `ProjectDirs` の識別子。設定ファイルの保存パスを決めるため、一度決めたら変えない
/// （変えると過去の設定ファイルを見失う）。
const QUALIFIER: &str = "net";
const ORGANIZATION: &str = "noncore";
const APPLICATION: &str = "openshoki";

/// 設定ファイル名。
const CONFIG_FILE: &str = "config.toml";

/// デフォルト保存先のフォルダ名（Documents もしくはホーム配下に作る想定）。
const DEFAULT_DIR_NAME: &str = "openshoki";

/// 永続化するユーザー設定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 録音ファイルの保存先ディレクトリ。
    pub recording_dir: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            recording_dir: default_recording_dir(),
        }
    }
}

impl Config {
    /// 設定を読み込む。失敗時はログを残してデフォルトを返す（アプリは落とさない）。
    /// 設定ファイルが無い初回起動はデフォルト扱い（ログ不要）。
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            eprintln!("設定ディレクトリを取得できないため、デフォルト設定を使う");
            return Self::default();
        };

        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(config) => config,
                Err(err) => {
                    eprintln!(
                        "設定ファイルの解析に失敗したためデフォルトを使う ({}): {err}",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!(
                    "設定ファイルの読み込みに失敗したためデフォルトを使う ({}): {err}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// 設定を OS 標準の設定ディレクトリに TOML で保存する。
    /// 設定ディレクトリが無ければ作成する。
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = config_path().ok_or("設定ディレクトリを取得できない")?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = toml::to_string_pretty(self)?;
        std::fs::write(&path, text)?;
        Ok(())
    }
}

/// 設定ファイルのフルパス（OS 標準の設定ディレクトリ配下）。取得できなければ `None`。
fn config_path() -> Option<PathBuf> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .map(|dirs| dirs.config_dir().join(CONFIG_FILE))
}

/// デフォルトの録音ファイル保存先。Documents 配下を基本とし、取得できなければホーム配下、
/// それも無ければカレント相対のフォルダ名にフォールバックする。
fn default_recording_dir() -> PathBuf {
    if let Some(user_dirs) = UserDirs::new() {
        return user_dirs
            .document_dir()
            .unwrap_or_else(|| user_dirs.home_dir())
            .join(DEFAULT_DIR_NAME);
    }
    PathBuf::from(DEFAULT_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip_preserves_recording_dir() {
        let config = Config {
            recording_dir: PathBuf::from("/tmp/openshoki-test"),
        };
        let text = toml::to_string_pretty(&config).expect("シリアライズは成功するはず");
        let restored: Config = toml::from_str(&text).expect("デシリアライズは成功するはず");
        assert_eq!(restored.recording_dir, config.recording_dir);
    }

    #[test]
    fn default_recording_dir_uses_app_folder() {
        // デフォルト保存先は openshoki 用フォルダで終わる。
        let dir = default_recording_dir();
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("openshoki"));
    }
}
