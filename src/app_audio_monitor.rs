//! 登録アプリのマイク入力（使用）を検知するモニタ（macOS 14.4+）。
//!
//! macOS 14 で追加された CoreAudio のプロセスオブジェクト API を使い、各プロセスがマイク入力を
//! 使っているか（`kAudioProcessPropertyIsRunningInput`）を読む。そのプロセスをどのアプリに
//! 帰属させるかは**公開 API だけ**で解決する（`input_running_bundle_ids` 参照）。これにより
//! 「いまマイクを使っているアプリのバンドル ID 集合」を得て、ユーザーが登録した `.app` の
//! バンドル ID と照合する。
//!
//! **既知の限界**: WebKit を音声ホストに使うアプリ（Safari など）は検知できない
//! （理由は `auto_record_limitation` の doc）。
//!
//! 判定は録音ループ（100ms タイマー）に相乗りしたポーリングで行い、`POLL_INTERVAL` に間引く。
//! 登録アプリのいずれかが「非使用→使用」へ変化した立ち上がりを `take_activated()` が返す。
//! API 非対応（macOS 14.4 未満）や照会失敗時は None 相当となり、自動開始しない（アプリは落とさない）。
//!
//! `input_running_bundle_ids()` は「マイク使用中のバンドル ID 集合」を返す公開ヘルパで、
//! 自動停止（#26）でも再利用する。

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::mem::size_of;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2_app_kit::NSRunningApplication;
use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectPropertySelector,
    kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
    kAudioProcessPropertyIsRunningInput, kAudioProcessPropertyPID,
};
use objc2_foundation::{NSBundle, NSString};

use crate::config::AppTrigger;

/// CoreAudio の成功を表す `OSStatus`（= `noErr`）。
const OS_STATUS_OK: i32 = 0;

/// マイク使用状況をポーリングする間隔。100ms タイマーから毎回照会すると無駄なので、この間隔に間引く。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 登録アプリのマイク入力の立ち上がりを検知するモニタ。全状態はメインスレッド上でのみ触る。
pub struct AppAudioMonitor {
    /// 最後にポーリングした時刻。`POLL_INTERVAL` 未満の呼び出しは照会を省く。開始検知
    /// `take_activated` と停止判定 `should_stop` で共有する（両者は録音状態で排他に呼ばれる前提）。
    last_poll: Cell<Instant>,
    /// 直近に観測した「マイク使用中の全アプリ」のバンドル ID 集合（登録有無によらない）。立ち上がり
    /// エッジ判定に使う。
    prev_mic_using: RefCell<HashSet<String>>,
    /// `prev_mic_using` が現在のマイク使用状況で初期化済みか。機能 OFF／登録なしの間は `false` に戻し、
    /// 再び有効になった最初の照会で現在値を取り込むことで、既にマイク使用中のアプリを遡って発火させない。
    primed: Cell<bool>,
    /// 照会不能（macOS 14.4 未満／失敗）を一度ログしたか。500ms ごとのログ氾濫を避けるため、
    /// 有効時に初めて照会できなかったときだけ 1 回知らせる。
    warned_unavailable: Cell<bool>,
    /// 自動停止用: 登録アプリのマイク使用が途絶えた時刻。`None` は「まだ途絶えていない（マイク使用中）」。
    /// 途絶えてから設定のデバウンス期間（`should_stop` の `debounce` 引数）継続したら自動停止する
    /// （瞬間的な途切れで誤停止しない）。
    input_ceased_since: Cell<Option<Instant>>,
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
            prev_mic_using: RefCell::new(HashSet::new()),
            primed: Cell::new(false),
            warned_unavailable: Cell::new(false),
            input_ceased_since: Cell::new(None),
        }
    }

    /// 登録アプリ（`triggers`）のいずれかが「非使用→使用」へ変化していたら `true`。
    ///
    /// 100ms タイマーから毎ティック呼ばれる想定。`enabled` が false または `triggers` が空のときは、
    /// 重いシステム全体の照会を**一切行わず** `false` を返す（オプトイン機能を無効化している間は
    /// 音声プロセスの走査をしない。アイドル負荷も抑える）。このとき `primed` を落とし、再び有効に
    /// なった最初の照会で現在のマイク使用状況を取り込んで遡り発火を防ぐ。有効時は `POLL_INTERVAL` に
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

        let Some(mic_using) = input_running_bundle_ids() else {
            // macOS 14.4 未満や照会失敗。原因切り分けのため一度だけ知らせる（毎回は出さない）。
            if !self.warned_unavailable.replace(true) {
                eprintln!(
                    "App-based auto-record is inactive because audio-process info is unavailable (needs macOS 14.4+)"
                );
            }
            return false;
        };

        if !self.primed.replace(true) {
            // 有効化後の最初の照会。現在マイク使用中のアプリを取り込み、遡って発火しない。
            *self.prev_mic_using.borrow_mut() = mic_using;
            return false;
        }

        let mut prev = self.prev_mic_using.borrow_mut();
        let activated = has_rising_edge(triggers, &prev, &mic_using);
        *prev = mic_using;
        activated
    }

    /// 自動停止すべきか（登録アプリのいずれもマイク入力していない状態が `debounce`
    /// 継続したか）を判定する。自動開始した録音中にのみ呼ぶ想定。`debounce` は呼び出し側が渡す
    /// （`Config::auto_stop_debounce()` が設定秒数を範囲へ丸めた `Duration`）。
    ///
    /// **副作用のあるポーリング**であり、間引きを通過するたびにマイク使用を照会して途絶えタイマー
    /// （`input_ceased_since`）を進める（純粋なクエリではない）。間引きタイマー `last_poll` は
    /// 開始検知 `take_activated` と共有するため、同一ティックで両方は呼ばない前提（未録音なら開始
    /// 検知・録音中なら停止判定、と排他に呼ぶ）。
    ///
    /// `enabled` が false／`triggers` が空／照会不能のときは `false`（自動停止しない）。有効時は
    /// `POLL_INTERVAL` に間引いて照会する。合図は音量ではなく「アプリがマイク入力を掴んでいるか」。
    /// 会議アプリ（ブラウザの Google Meet・Zoom.app 等）は通話中ずっとマイク入力を保持し、ミュートでも
    /// 手放さないことが多いため、ミュート・発言の合間・長い沈黙では止まらず、通話終了（マイク解放）で止まる。
    pub fn should_stop(&self, triggers: &[AppTrigger], enabled: bool, debounce: Duration) -> bool {
        if !enabled || triggers.is_empty() {
            self.input_ceased_since.set(None);
            return false;
        }
        if self.last_poll.get().elapsed() < POLL_INTERVAL {
            return false;
        }
        let now = Instant::now();
        self.last_poll.set(now);

        let Some(mic_using) = input_running_bundle_ids() else {
            return false; // 照会不能時は自動停止しない（状態は保持）。
        };
        let any_mic_using = triggers
            .iter()
            .any(|trigger| trigger_matches(&trigger.bundle_id, &mic_using));
        let (next_ceased, should_stop) =
            evaluate_auto_stop(any_mic_using, self.input_ceased_since.get(), now, debounce);
        self.input_ceased_since.set(next_ceased);
        should_stop
    }

    /// 録音停止後に呼ぶ。次の開始検知の照会で現在値を取り込み直し（`primed` を落とす）、録音中に
    /// マイクを使い始めたアプリを誤って立ち上がりとして拾わないようにする。停止デバウンス状態も初期化する。
    pub fn reset_after_stop(&self) {
        self.primed.set(false);
        self.input_ceased_since.set(None);
    }
}

/// 自動停止判定の純粋部分。登録アプリのマイク使用状況（`any_mic_using`）と、マイク使用が途絶えた時刻
/// （`ceased_since`）・現在時刻（`now`）・デバウンス期間（`debounce`）から、次の「途絶え開始時刻」と
/// 停止すべきかを返す。
///
/// - マイク使用中（`any_mic_using == true`）: 途絶えていないので `None` にリセット、停止しない。
/// - 途絶え中: `ceased_since` が `None` なら `now` から計測開始。経過が `debounce` 以上なら停止。
fn evaluate_auto_stop(
    any_mic_using: bool,
    ceased_since: Option<Instant>,
    now: Instant,
    debounce: Duration,
) -> (Option<Instant>, bool) {
    if any_mic_using {
        return (None, false);
    }
    let since = ceased_since.unwrap_or(now);
    let should_stop = now.duration_since(since) >= debounce;
    (Some(since), should_stop)
}

/// 立ち上がり判定の純粋部分: 登録アプリ（`triggers`）のうち、今はマイク使用中（`current`）で
/// 前回は使っていなかった（`prev` に無い）ものがあれば `true`。
fn has_rising_edge(
    triggers: &[AppTrigger],
    prev: &HashSet<String>,
    current: &HashSet<String>,
) -> bool {
    triggers.iter().any(|trigger| {
        trigger_matches(&trigger.bundle_id, current) && !trigger_matches(&trigger.bundle_id, prev)
    })
}

/// 登録トリガーのバンドル ID `base` が、稼働集合 `running` のいずれかにマッチするか。
///
/// ブラウザ（Chrome 等）は音声をヘルパープロセス（例: `com.google.Chrome.helper.Audio`）で扱うため、
/// 親アプリ（`com.google.Chrome`）の登録でヘルパーを拾えるよう、完全一致に加えて「`base` + `.`」で
/// 始まる前方一致も許す（`com.google.ChromeBeta` のような別アプリは `.` 区切りにより誤マッチしない）。
fn trigger_matches(base: &str, running: &HashSet<String>) -> bool {
    running.iter().any(|id| {
        id == base
            || id
                .strip_prefix(base)
                .is_some_and(|rest| rest.starts_with('.'))
    })
}

/// いまマイク入力を使っているアプリのバンドル ID 集合を返す。macOS 14.4 未満や照会失敗時は `None`
/// （呼び出し側は自動開始・自動停止を行わない）。自動停止（#26）でも再利用する。
///
/// 1 プロセスにつき、**公開 API で得られたバンドル ID をすべて**集合へ入れる（どれか 1 つに
/// 畳まない）。マルチプロセスのアプリは、マイクを掴むのが本体ではなくヘルパープロセスで、
/// 経路によって「ヘルパー自身の ID」と「親アプリの ID」のどちらが得られるかが変わるため:
///
/// 1. `kAudioProcessPropertyBundleID` — オーディオ HAL が持つ値。本体プロセスなら本体の ID、
///    ヘルパーなら `com.google.Chrome.helper` のようにヘルパー自身の ID になることがある。
/// 2. `NSRunningApplication` の直接の ID — 本体プロセスならこれで取れる（例: Zen/Firefox 系）。
///    ヘルパーでは nil になることがある。
/// 3. 実行パスの**最も外側**の `.app` の ID（`exec_path` → `bundle_id_for_exec_path`）—
///    Chrome/Slack/Electron 系のヘルパーを親アプリ（`com.google.Chrome` 等）へ畳む。
///
/// ヘルパー自身の ID しか得られない場合も、`trigger_matches` の前方一致（`base` + `.`）で親アプリ
/// 登録から拾える。
///
/// かつては responsible pid（そのプロセスに責任を持つ親アプリの pid）を返す private シンボルを
/// 実行時解決して使っていたが、Mac App Store が private API を禁じているため公開 API へ
/// 置き換えた（#107）。
/// これに伴う挙動の変化:
///
/// - ターミナル等から起動した CLI のマイク使用が、その**起動元アプリ**に帰属しなくなった
///   （帰属範囲が狭まる方向。コンテナ的なアプリを登録したときの巻き込みが無くなる）。
/// - WebKit を音声ホストに使うアプリ（Safari 等）が**検知できなくなった**。理由と扱いは
///   `auto_record_limitation` を参照。
pub fn input_running_bundle_ids() -> Option<HashSet<String>> {
    let processes = process_object_list()?;
    let mut ids = HashSet::new();
    for process in processes {
        if process_is_running_input(process) != Some(true) {
            continue;
        }
        if let Some(bundle) = process_bundle_id(process) {
            ids.insert(bundle);
        }
        let Some(pid) = process_pid(process) else {
            continue;
        };
        if let Some(bundle) = bundle_id_for_pid(pid) {
            ids.insert(bundle);
        }
        if let Some(bundle) = exec_path(pid).as_deref().and_then(bundle_id_for_exec_path) {
            ids.insert(bundle);
        }
    }
    Some(ids)
}

/// 自動録音で検知できないアプリなら、その理由（ユーザーへ見せる 1 文）を返す。
///
/// マイクを掴むのがアプリ自身ではなく、フレームワーク同梱の共有 XPC サービスになる構成は、
/// 公開 API ではホストアプリへ辿れない（#77 で実測。WebKit は `com.apple.WebKit.GPU.xpc` が
/// マイクを扱い、実行パスに `.app` を含まず親も `launchd`、CoreAudio も XPC サービス自身の
/// ID を返す）。登録自体は許すが、**黙って発火しない**のが一番分かりにくいので設定画面で伝える。
///
/// 判定は既知のバンドル ID の列挙で行う。WKWebView を使う任意のアプリが同じ構成になりうるが、
/// 登録時点でそれを見分ける公開 API が無いため、確実に該当するものだけを挙げる。列挙で拾えない
/// アプリのために、設定画面には一覧全体への注意書きも別途置いてある（`ui/app-window.slint`）。
///
/// 返す文言が短いのは、一覧が固定高さで折り返すと行が潰れるため（`examples/settings_view.rs` で
/// 確認できる）。**なぜ**検知できないかはその注意書きと README が持ち、ここは「どのアプリが
/// 該当するか」と「どうすればよいか」だけを伝える。
pub fn auto_record_limitation(bundle_id: &str) -> Option<&'static str> {
    const WEBKIT_AUDIO_HOSTED_APPS: &[&str] =
        &["com.apple.Safari", "com.apple.SafariTechnologyPreview"];

    WEBKIT_AUDIO_HOSTED_APPS
        .contains(&bundle_id)
        .then_some("Not detected — record manually")
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

/// プロセスがマイク入力を使っているか。取得失敗時は `None`。
fn process_is_running_input(process: AudioObjectID) -> Option<bool> {
    process_u32(process, kAudioProcessPropertyIsRunningInput).map(|value| value != 0)
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

/// CoreAudio が持つ、そのオーディオプロセスのバンドル ID（`kAudioProcessPropertyBundleID`、
/// macOS 14+）。プロセス側から推測せず、オーディオ HAL が知っている値をそのまま読む。
/// 取得失敗（未対応・バンドルを持たないプロセス）は `None`。
fn process_bundle_id(process: AudioObjectID) -> Option<String> {
    use objc2::rc::Retained;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// 失敗 status を 1 回だけ知らせるためのフラグ（500ms ごとのログ氾濫を避ける）。
    static STATUS_REPORTED: AtomicBool = AtomicBool::new(false);

    let address = global_address(kAudioProcessPropertyBundleID);
    let mut value: *mut NSString = std::ptr::null_mut();
    let mut size = size_of::<*mut NSString>() as u32;
    // SAFETY: value はポインタ 1 個ぶんの有効な書き込み先で、size もその大きさを伝えている。
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
    if status != OS_STATUS_OK {
        // この経路だけが黙って死んでも、他の 2 経路が生きている限り「照会不能」の警告は出ない。
        // 未対応（macOS 14 未満）とサンドボックスでの拒否を切り分けられるよう status を残す。
        if !STATUS_REPORTED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "Continuing without the CoreAudio bundle id because the query failed (OSStatus={status})"
            );
        }
        return None;
    }
    // バンドルを持たないプロセスでは noErr のまま何も書かれずに戻るため、null チェックは必須。
    let ptr = NonNull::new(value)?;
    // SAFETY: CoreAudio は CFStringRef を +1 で返す（AudioHardware.h の
    // kAudioProcessPropertyBundleID: "The caller is responsible for releasing the returned
    // CFObject."）。CFString と NSString は toll-free bridge なので、その +1 をそのまま
    // Retained に渡して解放を任せる。
    let string = unsafe { Retained::from_raw(ptr.as_ptr()) }?;
    let bundle_id = string.to_string();
    (!bundle_id.is_empty()).then_some(bundle_id)
}

/// PID から実行ファイルの絶対パスを読む（`libproc.h` の `proc_pidpath`。公開 API）。
fn exec_path(pid: i32) -> Option<PathBuf> {
    use std::ffi::{c_int, c_void};

    /// `libproc.h` の `PROC_PIDPATHINFO_MAXSIZE`（4 * MAXPATHLEN）。
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

    unsafe extern "C" {
        fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
    }

    let mut buffer = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
    // SAFETY: buffer は buffersize バイトの有効な書き込み先。戻り値は書き込まれた長さ（0 以下は失敗）で、
    // 終端 NUL を含まない文字数を返す。
    let len = unsafe {
        proc_pidpath(
            pid,
            buffer.as_mut_ptr().cast::<c_void>(),
            buffer.len() as u32,
        )
    };
    if len <= 0 {
        return None;
    }
    buffer.truncate(len as usize);
    Some(PathBuf::from(std::ffi::OsString::from_vec(buffer)))
}

/// 実行パスから**最も外側**の `.app` を切り出し、そのバンドル ID を読む。
///
/// ヘルパープロセスを親アプリへ畳むための経路。`.app` は入れ子になりうるため、内側ではなく
/// 外側を採る（Chrome のヘルパーは
/// `/Applications/Google Chrome.app/…/Helpers/Google Chrome Helper.app/Contents/MacOS/…` にあり、
/// 内側を採ると親アプリ登録と一致しない）。
fn bundle_id_for_exec_path(exec_path: &Path) -> Option<String> {
    let app_path = outermost_app_bundle(exec_path)?;
    let path_str = app_path.to_str()?;
    let bundle = NSBundle::bundleWithPath(&NSString::from_str(path_str))?;
    Some(bundle.bundleIdentifier()?.to_string())
}

/// パスに含まれる `.app` のうち、最も外側（先頭側）のものまでを返す。含まなければ `None`。
fn outermost_app_bundle(exec_path: &Path) -> Option<PathBuf> {
    let mut prefix = PathBuf::new();
    for component in exec_path.components() {
        prefix.push(component);
        if prefix
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case(OsStr::new("app")))
        {
            return Some(prefix);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{evaluate_auto_stop, has_rising_edge};
    use crate::config::AppTrigger;
    use std::collections::HashSet;
    use std::time::{Duration, Instant};

    /// デバウンス期間は `Config::auto_stop_debounce()` 由来で `should_stop` に渡すため、純粋関数
    /// `evaluate_auto_stop` のテストでは代表値を用いる。
    const DEBOUNCE: Duration = Duration::from_secs(4);

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
        // 前回も使っていたら立ち上がりではない（継続中）。
        let registered = triggers(&["com.apple.Music"]);
        assert!(!has_rising_edge(
            &registered,
            &set(&["com.apple.Music"]),
            &set(&["com.apple.Music"])
        ));
    }

    #[test]
    fn no_edge_for_unregistered_app() {
        // 未登録アプリがマイクを使い始めても発火しない。
        let registered = triggers(&["com.apple.Music"]);
        assert!(!has_rising_edge(
            &registered,
            &set(&[]),
            &set(&["com.google.Chrome"])
        ));
    }

    #[test]
    fn no_edge_when_output_stops() {
        // マイク使用が止まった（今回なし）は立ち上がりではない（自動停止は #26 の担当）。
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

    #[test]
    fn input_running_bundle_ids_does_not_crash() {
        // CoreAudio の照会と CFString の所有権受け渡し（`process_bundle_id`）を通すスモークテスト。
        // 中身は環境依存なので値は見ない。二重解放が起きていれば繰り返しで落ちる。
        for _ in 0..3 {
            let _ = super::input_running_bundle_ids();
        }
    }

    #[test]
    fn exec_path_ffi_returns_own_path() {
        // proc_pidpath の FFI 経路のスモークテスト。自分自身の pid なら必ずパスが取れる。
        let path = super::exec_path(std::process::id() as i32).expect("own exec path");
        assert!(path.is_absolute(), "{path:?} should be absolute");
    }

    #[test]
    fn outermost_app_bundle_picks_the_outer_app() {
        use super::outermost_app_bundle;
        use std::path::{Path, PathBuf};

        // Chrome のヘルパーは .app が入れ子になる。内側（ヘルパー自身）ではなく外側を採る。
        assert_eq!(
            outermost_app_bundle(Path::new(
                "/Applications/Google Chrome.app/Contents/Frameworks/Google Chrome Framework.framework/Versions/1/Helpers/Google Chrome Helper.app/Contents/MacOS/Google Chrome Helper"
            )),
            Some(PathBuf::from("/Applications/Google Chrome.app"))
        );
        // 本体プロセスはその .app 自身。
        assert_eq!(
            outermost_app_bundle(Path::new("/Applications/Slack.app/Contents/MacOS/Slack")),
            Some(PathBuf::from("/Applications/Slack.app"))
        );
        // 拡張子は最終コンポーネントだけを見るので、`.app` を途中に含む別名は誤検出しない。
        assert_eq!(
            outermost_app_bundle(Path::new(
                "/Applications/Foo.app.backup/Bar.app/Contents/MacOS/Bar"
            )),
            Some(PathBuf::from("/Applications/Foo.app.backup/Bar.app"))
        );
        // 大文字の拡張子も同じ扱い（HFS+ は既定で大文字小文字を区別しない）。
        assert_eq!(
            outermost_app_bundle(Path::new("/Applications/Foo.APP/Contents/MacOS/Foo")),
            Some(PathBuf::from("/Applications/Foo.APP"))
        );
        // 末尾スラッシュ付きの `.app` 自身。
        assert_eq!(
            outermost_app_bundle(Path::new("/Applications/Slack.app/")),
            Some(PathBuf::from("/Applications/Slack.app"))
        );
        // .app を含まないパス（CLI やフレームワーク内の XPC サービス）は解決しない。
        assert_eq!(
            outermost_app_bundle(Path::new(
                "/System/Library/Frameworks/WebKit.framework/Versions/A/XPCServices/com.apple.WebKit.GPU.xpc/Contents/MacOS/com.apple.WebKit.GPU"
            )),
            None
        );
        assert_eq!(outermost_app_bundle(Path::new("/bin/zsh")), None);
    }

    #[test]
    fn auto_record_limitation_flags_webkit_hosts() {
        use super::auto_record_limitation;
        // Safari は WebKit の GPU プロセスがマイクを掴むため検知できない。
        assert!(auto_record_limitation("com.apple.Safari").is_some());
        // ヘルパーを自前の .app に持つブラウザ・会議アプリは検知できる。
        assert!(auto_record_limitation("com.google.Chrome").is_none());
        assert!(auto_record_limitation("us.zoom.xos").is_none());
    }

    #[test]
    fn trigger_matches_exact_and_helper_prefix() {
        use super::trigger_matches;
        // 完全一致。
        assert!(trigger_matches(
            "com.google.Chrome",
            &set(&["com.google.Chrome"])
        ));
        // ヘルパープロセス（前方一致 + '.' 区切り）も親アプリ登録で拾う。
        assert!(trigger_matches(
            "com.google.Chrome",
            &set(&["com.google.Chrome.helper.Audio"])
        ));
        // '.' 区切りでない別アプリは誤マッチしない。
        assert!(!trigger_matches(
            "com.google.Chrome",
            &set(&["com.google.ChromeBeta"])
        ));
        // 未使用は false。
        assert!(!trigger_matches(
            "com.google.Chrome",
            &set(&["com.apple.Music"])
        ));
    }

    #[test]
    fn rising_edge_matches_helper_process() {
        // 登録は親アプリだが、ヘルパーのバンドル ID がマイクを使い始めても立ち上がりとして拾う。
        let registered = triggers(&["com.google.Chrome"]);
        assert!(has_rising_edge(
            &registered,
            &set(&[]),
            &set(&["com.google.Chrome.helper.Audio"])
        ));
    }

    #[test]
    fn auto_stop_resets_while_outputting() {
        // 使用中は途絶えていないので ceased=None にリセット、停止しない（途絶えていた履歴も消す）。
        let now = Instant::now();
        let (ceased, stop) = evaluate_auto_stop(true, Some(now), now, DEBOUNCE);
        assert_eq!(ceased, None);
        assert!(!stop);
    }

    #[test]
    fn auto_stop_starts_timer_on_first_cease() {
        // 途絶えの初回は now から計測を始めるだけで、まだ停止しない。
        let now = Instant::now();
        let (ceased, stop) = evaluate_auto_stop(false, None, now, DEBOUNCE);
        assert_eq!(ceased, Some(now));
        assert!(!stop);
    }

    #[test]
    fn auto_stop_waits_for_debounce() {
        // 途絶え継続がデバウンス未満なら停止しない（瞬間的な途切れで誤停止しない）。
        let start = Instant::now();
        let now = start + DEBOUNCE - Duration::from_millis(1);
        let (ceased, stop) = evaluate_auto_stop(false, Some(start), now, DEBOUNCE);
        assert_eq!(ceased, Some(start));
        assert!(!stop);
    }

    #[test]
    fn auto_stop_fires_after_debounce() {
        // 途絶えがデバウンス以上継続したら停止する。
        let start = Instant::now();
        let now = start + DEBOUNCE;
        let (_, stop) = evaluate_auto_stop(false, Some(start), now, DEBOUNCE);
        assert!(stop);
    }
}
