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

/// 自動停止デバウンスの既定秒数。登録アプリのマイク使用が途絶えてから自動停止するまでの待ち時間。
const DEFAULT_DEBOUNCE_SECS: u32 = 4;

/// 自動録音のトリガーにする登録アプリ。`.app` から取得したバンドル ID で、マイク入力を使っている
/// プロセスを照合し、表示名は設定画面での一覧表示に使う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppTrigger {
    /// アプリのバンドル ID（例: `com.apple.Music`）。マイク使用プロセスの照合キー。
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
    /// 登録アプリがマイクを使い始めたら録音を自動開始し、使い終わったら自動停止するか
    /// （macOS 14.4+ のみ有効）。会議アプリ（ブラウザの Google Meet・Zoom.app 等）は通話中だけ
    /// マイク入力を掴むため、これを合図に通話の開始/終了へ連動できる。オプトインの既定 OFF。
    /// 旧項目名 `auto_record_on_app_playback`（出力ベース時代）からエイリアスで互換を保つ。
    #[serde(default, alias = "auto_record_on_app_playback")]
    pub auto_record_on_app_mic: bool,
    /// マイク使用での自動録音トリガーにする登録アプリ一覧。旧名 `app_playback_triggers` と互換。
    #[serde(default, alias = "app_playback_triggers")]
    pub app_mic_triggers: Vec<AppTrigger>,
    /// 登録アプリのマイク使用が途絶えてから自動停止するまでのデバウンス秒数（既定 4 秒）。通話終了後に
    /// 確実に閉じる長さは環境・好みで変わるため設定可能にする。旧 config 互換のため未指定時は既定へ
    /// フォールバックする。設定 TOML は手編集されうるため、実際に使う側で妥当な範囲へクランプする。
    #[serde(default = "default_debounce_secs")]
    pub auto_stop_debounce_secs: u32,
}

/// `auto_stop_debounce_secs` の serde 既定値。項目を持たない旧 config でも既定 4 秒で読める。
fn default_debounce_secs() -> u32 {
    DEFAULT_DEBOUNCE_SECS
}

impl Default for Config {
    fn default() -> Self {
        Self {
            recording_dir: default_recording_dir(),
            auto_record_on_app_mic: false,
            app_mic_triggers: Vec::new(),
            auto_stop_debounce_secs: DEFAULT_DEBOUNCE_SECS,
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
            auto_record_on_app_mic: true,
            app_mic_triggers: vec![AppTrigger {
                bundle_id: "com.apple.Music".to_owned(),
                name: "Music".to_owned(),
            }],
            auto_stop_debounce_secs: 7,
        };
        let text = toml::to_string_pretty(&config).expect("serialization should succeed");
        let restored: Config = toml::from_str(&text).expect("deserialization should succeed");
        assert_eq!(restored.recording_dir, config.recording_dir);
        assert_eq!(
            restored.auto_record_on_app_mic,
            config.auto_record_on_app_mic
        );
        assert_eq!(restored.app_mic_triggers, config.app_mic_triggers);
        assert_eq!(
            restored.auto_stop_debounce_secs,
            config.auto_stop_debounce_secs
        );
    }

    #[test]
    fn deserialize_old_config_without_new_field_defaults_false() {
        // 新項目を持たない旧 config.toml を読んでも失敗せず、recording_dir は保持され、
        // 新項目は既定（OFF・空リスト）になる（#[serde(default)]）。
        let text = "recording_dir = \"/tmp/openshoki-old\"\n";
        let restored: Config =
            toml::from_str(text).expect("loading the old settings should succeed");
        assert_eq!(restored.recording_dir, PathBuf::from("/tmp/openshoki-old"));
        assert!(!restored.auto_record_on_app_mic);
        assert!(restored.app_mic_triggers.is_empty());
        assert_eq!(restored.auto_stop_debounce_secs, DEFAULT_DEBOUNCE_SECS);
    }

    #[test]
    fn deserialize_reads_configured_debounce_secs() {
        // 設定された自動停止デバウンス秒数がそのまま読める。
        let text = concat!(
            "recording_dir = \"/tmp/openshoki-debounce\"\n",
            "auto_stop_debounce_secs = 10\n",
        );
        let restored: Config = toml::from_str(text).expect("loading the settings should succeed");
        assert_eq!(restored.auto_stop_debounce_secs, 10);
    }

    #[test]
    fn deserialize_ignores_removed_mic_field() {
        // 削除した項目 auto_record_on_mic_active が残る旧 config.toml を読んでも、未知項目として
        // 無視され失敗しない（serde 既定で未知フィールドは無視）。
        let text = concat!(
            "recording_dir = \"/tmp/openshoki-removed\"\n",
            "auto_record_on_mic_active = true\n",
        );
        let restored: Config =
            toml::from_str(text).expect("loading a config with the removed field should succeed");
        assert_eq!(
            restored.recording_dir,
            PathBuf::from("/tmp/openshoki-removed")
        );
        assert!(!restored.auto_record_on_app_mic);
    }

    #[test]
    fn deserialize_reads_legacy_playback_field_names() {
        // 出力ベース時代の旧項目名（auto_record_on_app_playback / app_playback_triggers）も
        // serde alias で読めること（互換）。
        let text = concat!(
            "recording_dir = \"/tmp/openshoki-legacy\"\n",
            "auto_record_on_app_playback = true\n",
            "[[app_playback_triggers]]\n",
            "bundle_id = \"com.apple.Music\"\n",
            "name = \"Music\"\n",
        );
        let restored: Config =
            toml::from_str(text).expect("loading the legacy settings should succeed");
        assert!(restored.auto_record_on_app_mic);
        assert_eq!(restored.app_mic_triggers.len(), 1);
        assert_eq!(restored.app_mic_triggers[0].bundle_id, "com.apple.Music");
    }

    #[test]
    fn default_recording_dir_uses_app_folder() {
        // デフォルト保存先は openshoki 用フォルダで終わる。
        let dir = default_recording_dir();
        assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some("openshoki"));
    }
}
