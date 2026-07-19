//! 登録アプリの音声出力（再生）を検知するモニタ（macOS 14.4+）。
//!
//! macOS 14 で追加された CoreAudio のプロセスオブジェクト API を使い、各プロセスが音声出力を
//! 行っているか（`kAudioProcessPropertyIsRunningOutput`）と PID（`kAudioProcessPropertyPID`）を
//! 読み、PID→バンドル ID は `NSRunningApplication` で解決する。これにより「いま音声を再生して
//! いるアプリのバンドル ID 集合」を得て、ユーザーが登録した `.app` のバンドル ID と照合する。
//!
//! 判定は録音ループ（100ms タイマー）に相乗りしたポーリングで行い、`POLL_INTERVAL` に間引く。
//! 登録アプリのいずれかが「非出力→出力」へ変化した立ち上がりを `take_activated()` が返す。
//! API 非対応（macOS 14.4 未満）や照会失敗時は None 相当となり、自動開始しない（アプリは落とさない）。
//!
//! `output_running_bundle_ids()` は「出力稼働中のバンドル ID 集合」を返す公開ヘルパで、
//! 自動停止（#26）でも再利用する。

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::mem::size_of;
use std::path::Path;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2_app_kit::NSRunningApplication;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector,
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
    kAudioProcessPropertyIsRunningOutput, kAudioProcessPropertyPID,
};
use objc2_foundation::{NSBundle, NSString};

use crate::config::AppTrigger;

/// CoreAudio の成功を表す `OSStatus`（= `noErr`）。
const OS_STATUS_OK: i32 = 0;

/// 出力状態をポーリングする間隔。100ms タイマーから毎回照会すると無駄なので、この間隔に間引く。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 登録アプリの音声出力の立ち上がりを検知するモニタ。全状態はメインスレッド上でのみ触る。
pub struct AppAudioMonitor {
    /// 最後にポーリングした時刻。`POLL_INTERVAL` 未満の呼び出しは照会を省く。
    last_poll: Cell<Instant>,
    /// 直近に観測した「出力中の全アプリ」のバンドル ID 集合（登録有無によらない）。立ち上がり
    /// エッジ判定に使う。
    prev_outputting: RefCell<HashSet<String>>,
    /// `prev_outputting` が現在の出力状況で初期化済みか。機能 OFF／登録なしの間は `false` に戻し、
    /// 再び有効になった最初の照会で現在値を取り込むことで、既に再生中のアプリを遡って発火させない。
    primed: Cell<bool>,
    /// 照会不能（macOS 14.4 未満／失敗）を一度ログしたか。500ms ごとのログ氾濫を避けるため、
    /// 有効時に初めて照会できなかったときだけ 1 回知らせる。
    warned_unavailable: Cell<bool>,
}

impl Default for AppAudioMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl AppAudioMonitor {
    pub fn new() -> Self {
        // 生成時にはシステム照会を行わない（プライバシー配慮のオプトイン機能なので、有効化される
        // まで音声プロセスの走査をしない）。初期化は有効化後の最初の照会で行う（`primed`）。
        Self {
            last_poll: Cell::new(Instant::now()),
            prev_outputting: RefCell::new(HashSet::new()),
            primed: Cell::new(false),
            warned_unavailable: Cell::new(false),
        }
    }

    /// 登録アプリ（`triggers`）のいずれかが「非出力→出力」へ変化していたら `true`。
    ///
    /// 100ms タイマーから毎ティック呼ばれる想定。`enabled` が false または `triggers` が空のときは、
    /// 重いシステム全体の照会を**一切行わず** `false` を返す（オプトイン機能を無効化している間は
    /// 音声プロセスの走査をしない。アイドル負荷も抑える）。このとき `primed` を落とし、再び有効に
    /// なった最初の照会で現在の出力状況を取り込んで遡り発火を防ぐ。有効時は `POLL_INTERVAL` に
    /// 間引いて照会する。録音中かどうかの判定は呼び出し側が行う。照会不能時は状態を変えず `false`。
    pub fn take_activated(&self, triggers: &[AppTrigger], enabled: bool) -> bool {
        if !enabled || triggers.is_empty() {
            self.primed.set(false);
            return false;
        }
        if self.last_poll.get().elapsed() < POLL_INTERVAL {
            return false;
        }
        self.last_poll.set(Instant::now());

        let Some(outputting) = output_running_bundle_ids() else {
            // macOS 14.4 未満や照会失敗。原因切り分けのため一度だけ知らせる（毎回は出さない）。
            if !self.warned_unavailable.replace(true) {
                eprintln!(
                    "App-playback auto-record is inactive because audio-process info is unavailable (needs macOS 14.4+)"
                );
            }
            return false;
        };

        if !self.primed.replace(true) {
            // 有効化後の最初の照会。現在出力中のアプリを取り込み、遡って発火しない。
            *self.prev_outputting.borrow_mut() = outputting;
            return false;
        }

        let mut prev = self.prev_outputting.borrow_mut();
        let activated = has_rising_edge(triggers, &prev, &outputting);
        *prev = outputting;
        activated
    }
}

/// 立ち上がり判定の純粋部分: 登録アプリ（`triggers`）のうち、今は出力中（`current`）で
/// 前回は出力していなかった（`prev` に無い）ものがあれば `true`。
fn has_rising_edge(
    triggers: &[AppTrigger],
    prev: &HashSet<String>,
    current: &HashSet<String>,
) -> bool {
    triggers
        .iter()
        .any(|trigger| current.contains(&trigger.bundle_id) && !prev.contains(&trigger.bundle_id))
}

/// いま音声出力を行っているアプリのバンドル ID 集合を返す。macOS 14.4 未満や照会失敗時は `None`
/// （呼び出し側は自動開始・自動停止を行わない）。自動停止（#26）でも再利用する。
pub fn output_running_bundle_ids() -> Option<HashSet<String>> {
    let processes = process_object_list()?;
    let mut ids = HashSet::new();
    for process in processes {
        if process_is_running_output(process) == Some(true)
            && let Some(pid) = process_pid(process)
            && let Some(bundle) = bundle_id_for_pid(pid)
        {
            ids.insert(bundle);
        }
    }
    Some(ids)
}

/// `.app` のパスからバンドル ID と表示名を読む（設定画面でのアプリ登録に使う）。
/// バンドル ID が読めなければ `None`。表示名は `.app` のファイル名（拡張子除く）を使う。
pub fn app_info_for_path(path: &Path) -> Option<AppTrigger> {
    let path_str = path.to_str()?;
    let ns_path = NSString::from_str(path_str);
    let bundle = NSBundle::bundleWithPath(&ns_path)?;
    let bundle_id = bundle.bundleIdentifier()?.to_string();
    let name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("App")
        .to_owned();
    Some(AppTrigger { bundle_id, name })
}

/// 指定セレクタの、システムオブジェクト用グローバルアドレス（スコープ Global・主エレメント）。
fn global_address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// システムの全プロセスオブジェクトの一覧を取得する。API 非対応（プロセスオブジェクト API は
/// macOS 14.0+、本機能に必要な `IsRunningOutput` は 14.4+）や失敗時は `None`。
fn process_object_list() -> Option<Vec<AudioObjectID>> {
    let address = global_address(kAudioHardwarePropertyProcessObjectList);
    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    if status != OS_STATUS_OK {
        return None;
    }
    let count = size as usize / size_of::<AudioObjectID>();
    let mut processes = vec![0 as AudioObjectID; count];
    let Some(out) = NonNull::new(processes.as_mut_ptr()) else {
        // 空 Vec でも as_mut_ptr は非 null のダングリングを返すため、通常この分岐は通らない。
        // 万一 null なら照会せず空で返す（size=0 のときも下の本流が size 0 で正しく空を返す）。
        return Some(processes);
    };
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            out.cast(),
        )
    };
    if status != OS_STATUS_OK {
        return None;
    }
    processes.truncate(size as usize / size_of::<AudioObjectID>());
    Some(processes)
}

/// プロセスオブジェクトの `u32` プロパティを読む。取得失敗時は `None`。
fn process_u32(process: AudioObjectID, selector: AudioObjectPropertySelector) -> Option<u32> {
    let address = global_address(selector);
    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            process,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    (status == OS_STATUS_OK).then_some(value)
}

/// プロセスが音声出力を行っているか。取得失敗時は `None`。
fn process_is_running_output(process: AudioObjectID) -> Option<bool> {
    process_u32(process, kAudioProcessPropertyIsRunningOutput).map(|value| value != 0)
}

/// プロセスオブジェクトの PID。取得失敗時は `None`。`pid_t` は `i32`。
fn process_pid(process: AudioObjectID) -> Option<i32> {
    let address = global_address(kAudioProcessPropertyPID);
    let mut pid: i32 = 0;
    let mut size = size_of::<i32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            process,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut pid).cast(),
        )
    };
    (status == OS_STATUS_OK).then_some(pid)
}

/// PID からアプリのバンドル ID を解決する（`NSRunningApplication` 経由）。バンドルを持たない
/// プロセス（CLI 等）や実行中でない場合は `None`。
fn bundle_id_for_pid(pid: i32) -> Option<String> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    let bundle_id = app.bundleIdentifier()?;
    Some(bundle_id.to_string())
}

#[cfg(test)]
mod tests {
    use super::has_rising_edge;
    use crate::config::AppTrigger;
    use std::collections::HashSet;

    fn triggers(bundle_ids: &[&str]) -> Vec<AppTrigger> {
        bundle_ids
            .iter()
            .map(|id| AppTrigger {
                bundle_id: (*id).to_owned(),
                name: (*id).to_owned(),
            })
            .collect()
    }

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rising_edge_when_registered_app_starts_output() {
        // 登録アプリが「前回なし→今回あり」なら立ち上がり。
        let registered = triggers(&["com.apple.Music"]);
        assert!(has_rising_edge(
            &registered,
            &set(&[]),
            &set(&["com.apple.Music"])
        ));
    }

    #[test]
    fn no_edge_when_already_outputting() {
        // 前回も出力していたら立ち上がりではない（継続中）。
        let registered = triggers(&["com.apple.Music"]);
        assert!(!has_rising_edge(
            &registered,
            &set(&["com.apple.Music"]),
            &set(&["com.apple.Music"])
        ));
    }

    #[test]
    fn no_edge_for_unregistered_app() {
        // 未登録アプリが出力し始めても発火しない。
        let registered = triggers(&["com.apple.Music"]);
        assert!(!has_rising_edge(
            &registered,
            &set(&[]),
            &set(&["com.google.Chrome"])
        ));
    }

    #[test]
    fn no_edge_when_output_stops() {
        // 出力が止まった（今回なし）は立ち上がりではない（自動停止は #26 の担当）。
        let registered = triggers(&["com.apple.Music"]);
        assert!(!has_rising_edge(
            &registered,
            &set(&["com.apple.Music"]),
            &set(&[])
        ));
    }

    #[test]
    fn rising_edge_with_multiple_registered_apps() {
        // 複数登録のうち 1 つでも立ち上がれば発火。
        let registered = triggers(&["com.apple.Music", "com.apple.QuickTimePlayerX"]);
        assert!(has_rising_edge(
            &registered,
            &set(&["com.apple.Music"]),
            &set(&["com.apple.Music", "com.apple.QuickTimePlayerX"])
        ));
    }
}
