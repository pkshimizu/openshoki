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

/// 自動録音のトリガーにする登録アプリ。`.app` から取得したバンドル ID で音声出力プロセスを
/// 照合し、表示名は設定画面での一覧表示に使う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppTrigger {
    /// アプリのバンドル ID（例: `com.apple.Music`）。音声出力プロセスの照合キー。
    pub bundle_id: String,
    /// 設定画面で表示するアプリ名。
    pub name: String,
}

/// 永続化するユーザー設定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 録音ファイルの保存先ディレクトリ。
    /// 設定 TOML は手編集されうるため信頼境界の外。録音機能で書き込む際は、存在・書き込み可否を
    /// 検証してから使う（本 issue のスコープは設定値の保持まで）。
    pub recording_dir: PathBuf,
    /// 他アプリがマイクを使い始めたら録音を自動開始するか（macOS のみ有効に働く）。
    /// プライバシーに関わるためオプトインの既定 OFF とする。
    /// この項目を持たない旧 `config.toml` を読んでも失敗しないよう `#[serde(default)]` を付ける
    /// （付けないとデシリアライズが失敗し、`recording_dir` ごとデフォルトへ落ちる）。
    #[serde(default)]
    pub auto_record_on_mic_active: bool,
    /// 登録アプリの音声再生を検知したら録音を自動開始するか（macOS 14.4+ のみ有効）。
    /// オプトインの既定 OFF。旧 `config.toml` 互換のため `#[serde(default)]`。
    #[serde(default)]
    pub auto_record_on_app_playback: bool,
    /// 音声再生での自動開始トリガーにする登録アプリ一覧。
    #[serde(default)]
    pub app_playback_triggers: Vec<AppTrigger>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            recording_dir: default_recording_dir(),
            auto_record_on_mic_active: false,
            auto_record_on_app_playback: false,
            app_playback_triggers: Vec::new(),
        }
    }
}

impl Config {
    /// 設定を読み込む。失敗時はログを残してデフォルトを返す（アプリは落とさない）。
    /// 設定ファイルが無い初回起動はデフォルト扱い（ログ不要）。
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            eprintln!(
                "Using default settings because the settings directory could not be determined"
            );
            return Self::default();
        };

        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(config) => config,
                Err(err) => {
                    eprintln!(
                        "Using defaults because parsing the settings file failed ({}): {err}",
                        path.display()
                    );
                    Self::default()
                }
            },
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(err) => {
                eprintln!(
                    "Using defaults because reading the settings file failed ({}): {err}",
                    path.display()
                );
                Self::default()
            }
        }
    }

    /// 設定を OS 標準の設定ディレクトリに TOML で保存する。
    /// 設定ディレクトリが無ければ作成する。
    pub fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = config_path().ok_or("Cannot determine the settings directory")?;
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
    // ホームディレクトリすら取得できない異例環境。黙って縮退させず、相対パスへ
    // フォールバックする旨をログに残す。
    eprintln!(
        "Falling back to a relative recording folder because the user directory could not be determined"
    );
    PathBuf::from(DEFAULT_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_roundtrip_preserves_fields() {
        let config = Config {
            recording_dir: PathBuf::from("/tmp/openshoki-test"),
            auto_record_on_mic_active: true,
            auto_record_on_app_playback: true,
            app_playback_triggers: vec![AppTrigger {
                bundle_id: "com.apple.Music".to_owned(),
                name: "Music".to_owned(),
            }],
        };
        let text = toml::to_string_pretty(&config).expect("serialization should succeed");
        let restored: Config = toml::from_str(&text).expect("deserialization should succeed");
        assert_eq!(restored.recording_dir, config.recording_dir);
        assert_eq!(
            restored.auto_record_on_mic_active,
            config.auto_record_on_mic_active
        );
        assert_eq!(
            restored.auto_record_on_app_playback,
            config.auto_record_on_app_playback
        );
        assert_eq!(restored.app_playback_triggers, config.app_playback_triggers);
    }

    #[test]
    fn deserialize_old_config_without_new_field_defaults_false() {
        // 新項目を持たない旧 config.toml を読んでも失敗せず、recording_dir は保持され、
        // 新項目は既定（OFF・空リスト）になる（#[serde(default)]）。
        let text = "recording_dir = \"/tmp/openshoki-old\"\n";
        let restored: Config =
            toml::from_str(text).expect("loading the old settings should succeed");
        assert_eq!(restored.recording_dir, PathBuf::from("/tmp/openshoki-old"));
        assert!(!restored.auto_record_on_mic_active);
        assert!(!restored.auto_record_on_app_playback);
        assert!(restored.app_playback_triggers.is_empty());
    }

    #[test]
    fn default_recording_dir_uses_app_folder() {
        // デフォルト保存先は openshoki 用フォルダで終わる。
        let dir = default_recording_dir();
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("openshoki"));
    }
}
