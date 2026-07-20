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

/// 自動停止デバウンス秒数の設定可能範囲。設定 TOML は手編集されうるため、この範囲へ丸めて使う。
/// 値は `ui/app-window.slint` の SpinBox の minimum/maximum と一致させること。
pub const DEBOUNCE_MIN_SECS: u32 = 1;
pub const DEBOUNCE_MAX_SECS: u32 = 60;

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
    /// フォールバックする。設定 TOML は手編集されうる信頼境界外だが、`deserialize_debounce_secs` により
    /// 読み込み後は常に範囲内。使う側は `auto_stop_debounce()` を通す。
    #[serde(
        default = "default_debounce_secs",
        deserialize_with = "deserialize_debounce_secs"
    )]
    pub auto_stop_debounce_secs: u32,
    /// 録音停止時に保存した各音源を自動で文字起こしするか。whisper は CPU 負荷が大きいため
    /// オプトインの既定 OFF。ON でも `whisper_model_path` が無ければ実行しない。
    #[serde(default)]
    pub auto_transcribe: bool,
    /// whisper モデルファイル（ggml 形式）のパス。モデルは同梱・自動ダウンロードせず、ユーザーが
    /// 配置して設定画面から選択する。未指定なら文字起こしを行わない。
    #[serde(default)]
    pub whisper_model_path: Option<PathBuf>,
    /// 文字起こしの言語（例: `ja`）。`None` は whisper の自動判定に任せる（既定）。
    #[serde(default)]
    pub transcribe_language: Option<String>,
}

/// `auto_stop_debounce_secs` の serde 既定値。項目を持たない旧 config でも既定 4 秒で読める。
fn default_debounce_secs() -> u32 {
    DEFAULT_DEBOUNCE_SECS
}

/// `auto_stop_debounce_secs` を寛容にデシリアライズし、常に有効範囲 `[DEBOUNCE_MIN_SECS,
/// DEBOUNCE_MAX_SECS]` の値を返す。設定 TOML は手編集されうる信頼境界外で、このフィールドが
/// 負値・非数値・範囲外でも、当該項目だけ丸めて他の設定（保存先・登録アプリ）を巻き添えで失わせない
/// （`u32` で直接受けると型不一致でファイル全体が既定へ落ちてしまう）。これによりデシリアライズ後は
/// 「フィールドは常に範囲内」が保証される。なお TOML 整数は i64 のため、i64 も超える極端値は TOML
/// パース段階で弾かれ、他の縮退と同様にファイル全体が既定へフォールバックする（現実的な秒数では起きない）。
fn deserialize_debounce_secs<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = toml::Value::deserialize(deserializer)?;
    let secs = value
        .as_integer()
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(DEFAULT_DEBOUNCE_SECS);
    Ok(clamp_debounce_secs(secs))
}

/// デバウンス秒数を設定可能範囲へ丸める。表示・保存・判定で同じ丸めを使う単一の口。
pub fn clamp_debounce_secs(secs: u32) -> u32 {
    secs.clamp(DEBOUNCE_MIN_SECS, DEBOUNCE_MAX_SECS)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            recording_dir: default_recording_dir(),
            auto_record_on_app_mic: false,
            app_mic_triggers: Vec::new(),
            auto_stop_debounce_secs: DEFAULT_DEBOUNCE_SECS,
            auto_transcribe: false,
            whisper_model_path: None,
            transcribe_language: None,
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
                // auto_stop_debounce_secs はデシリアライズ時に範囲へ丸め済み（deserialize_debounce_secs）。
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

    /// 自動停止のデバウンス期間。設定値を範囲へ丸めて `Duration` にする（呼び出し側にクランプと単位変換の
    /// 重複を持たせない単一の口）。デシリアライズ経由の値は既に範囲内だが、構造体リテラルで組んだ値
    /// （テスト等）も安全なよう冪等に丸める。
    pub fn auto_stop_debounce(&self) -> std::time::Duration {
        std::time::Duration::from_secs(u64::from(clamp_debounce_secs(self.auto_stop_debounce_secs)))
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
            auto_transcribe: true,
            whisper_model_path: Some(PathBuf::from("/tmp/models/ggml-base.bin")),
            transcribe_language: Some("ja".to_owned()),
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
        assert_eq!(restored.auto_transcribe, config.auto_transcribe);
        assert_eq!(restored.whisper_model_path, config.whisper_model_path);
        assert_eq!(restored.transcribe_language, config.transcribe_language);
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
        assert!(!restored.auto_transcribe);
        assert!(restored.whisper_model_path.is_none());
        assert!(restored.transcribe_language.is_none());
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
    fn deserialize_clamps_in_range_type_but_out_of_bounds_value() {
        // u32 として妥当だが範囲外（0・1000）の値は、デシリアライズ時に [MIN, MAX] へ丸まる。
        let low = "recording_dir = \"/tmp/x\"\nauto_stop_debounce_secs = 0\n";
        let high = "recording_dir = \"/tmp/x\"\nauto_stop_debounce_secs = 1000\n";
        assert_eq!(
            toml::from_str::<Config>(low)
                .unwrap()
                .auto_stop_debounce_secs,
            DEBOUNCE_MIN_SECS
        );
        assert_eq!(
            toml::from_str::<Config>(high)
                .unwrap()
                .auto_stop_debounce_secs,
            DEBOUNCE_MAX_SECS
        );
    }

    #[test]
    fn clamp_debounce_secs_bounds() {
        assert_eq!(clamp_debounce_secs(0), DEBOUNCE_MIN_SECS);
        assert_eq!(clamp_debounce_secs(DEBOUNCE_MIN_SECS), DEBOUNCE_MIN_SECS);
        assert_eq!(clamp_debounce_secs(30), 30);
        assert_eq!(clamp_debounce_secs(DEBOUNCE_MAX_SECS), DEBOUNCE_MAX_SECS);
        assert_eq!(clamp_debounce_secs(u32::MAX), DEBOUNCE_MAX_SECS);
    }

    #[test]
    fn deserialize_out_of_range_debounce_keeps_other_fields() {
        // 手編集で負値・u32 範囲外・非数値でもパース失敗させず、当該項目のみ既定へ丸め、
        // 他設定（保存先）を巻き添えで失わない（deserialize_debounce_secs）。
        for bad in ["-5", "999999999999", "\"abc\"", "1.5"] {
            let text = format!(
                "recording_dir = \"/tmp/openshoki-bad\"\nauto_stop_debounce_secs = {bad}\n"
            );
            let restored: Config =
                toml::from_str(&text).expect("loading should not fail on a bad debounce value");
            assert_eq!(restored.recording_dir, PathBuf::from("/tmp/openshoki-bad"));
            assert_eq!(restored.auto_stop_debounce_secs, DEFAULT_DEBOUNCE_SECS);
        }
    }

    #[test]
    fn auto_stop_debounce_clamps_to_duration() {
        // 使用側の単一口。範囲外のメモリ値でも Duration は必ず [MIN, MAX] に収まる。
        let over = Config {
            auto_stop_debounce_secs: 1000,
            ..Config::default()
        };
        assert_eq!(
            over.auto_stop_debounce(),
            std::time::Duration::from_secs(u64::from(DEBOUNCE_MAX_SECS))
        );
        let under = Config {
            auto_stop_debounce_secs: 0,
            ..Config::default()
        };
        assert_eq!(
            under.auto_stop_debounce(),
            std::time::Duration::from_secs(u64::from(DEBOUNCE_MIN_SECS))
        );
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
