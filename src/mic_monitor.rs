//! 他アプリが既定入力デバイス（マイク）を使い始めたことを検知する監視モニタ（macOS）。
//!
//! CoreAudio の既定入力デバイスに `kAudioDevicePropertyDeviceIsRunningSomewhere`
//! （いずれかのプロセスがそのデバイスを稼働させているか）のプロパティリスナーを登録し、
//! **非稼働→稼働の立ち上がり**（＝会議アプリなどがマイクを使い始めた瞬間）を検知する。
//! プロパティ監視自体は録音（マイク権限）を必要としない（デバイス状態の参照のため）。
//!
//! リスナーは CoreAudio 管理のスレッドから呼ばれるため、検知は共有フラグ（`AtomicBool`）に
//! 記録するだけにし、実際の録音開始は呼び出し側が `take_activated()` を既存の 100ms タイマーで
//! ポーリングしてメインスレッド上で行う（トレイイベントと同じ橋渡し方式）。
//!
//! 既定入力デバイスの変更追随（差し替え時のリスナー付け替え）は初期実装では未対応で、
//! 起動時に得た既定入力デバイスのみを監視する。CoreAudio 連携に失敗した場合はエラーを返し、
//! 呼び出し側はモニタ無しで常駐を続ける（`docs/rules/error-handling.md`）。

use std::error::Error;
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2_core_audio::{
    AudioObjectAddPropertyListener, AudioObjectGetPropertyData, AudioObjectID,
    AudioObjectPropertyAddress, AudioObjectRemovePropertyListener,
    kAudioDevicePropertyDeviceIsRunningSomewhere, kAudioHardwarePropertyDefaultInputDevice,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject,
};

/// CoreAudio の成功を表す `OSStatus`（= `noErr`）。
const OS_STATUS_OK: i32 = 0;

/// リスナーとメインスレッドで共有する監視状態。`AtomicBool` のみで、ロック不要。
struct MonitorState {
    /// 非稼働→稼働の立ち上がりを検知したら真にする。`take_activated()` で 1 回だけ取り出す。
    activated: AtomicBool,
    /// 直前に観測した稼働状態。リスナーは変化通知のたびに現在値を読み直し、これと比較して
    /// 立ち上がりエッジだけを拾う。起動時に既に稼働中でも、その状態を初期値に入れておくことで
    /// 遡っての自動開始をしない（`start()` で現在値を初期化する）。
    was_running: AtomicBool,
}

/// 既定入力デバイスの稼働状態を監視するモニタ。生存している間だけリスナーが登録される。
///
/// 常駐アプリの全ライフタイムにわたって保持する想定で、`Drop` でリスナーを解除する。
/// `state` は `Arc` でリスナーのクライアントデータとして渡したポインタの実体を生かし続ける。
pub struct MicMonitor {
    /// 監視対象の既定入力デバイス（起動時に確定）。解除時に同じデバイスへ指定する。
    device: AudioObjectID,
    /// リスナーと共有する状態。リスナー解除まで生かすため `Arc` で保持する。
    state: Arc<MonitorState>,
}

impl MicMonitor {
    /// 既定入力デバイスを取得し、`IsRunningSomewhere` のプロパティリスナーを登録して監視を始める。
    ///
    /// 既定入力デバイスが得られない、またはリスナー登録に失敗した場合はエラーを返す
    /// （呼び出し側はモニタ無しで続行する）。
    pub fn start() -> Result<Self, Box<dyn Error>> {
        let device = default_input_device().ok_or("既定の入力デバイスを取得できない")?;

        // 起動時点の稼働状態を初期値にする。既に使用中なら was_running=true とし、以後の
        // 「停止→開始」でだけ立ち上がりを検知する（起動時の使用中を遡って拾わない）。
        let was_running = device_is_running(device).unwrap_or(false);
        let state = Arc::new(MonitorState {
            activated: AtomicBool::new(false),
            was_running: AtomicBool::new(was_running),
        });

        let address = running_somewhere_address();
        // クライアントデータには state の実体ポインタを渡す。state は本構造体が保持し続けるため、
        // Drop でリスナーを解除するまでポインタは有効。
        let client_data = Arc::as_ptr(&state) as *mut c_void;
        let status = unsafe {
            AudioObjectAddPropertyListener(
                device,
                NonNull::from(&address),
                Some(property_listener),
                client_data,
            )
        };
        if status != OS_STATUS_OK {
            return Err(format!("プロパティリスナーの登録に失敗した (OSStatus={status})").into());
        }

        Ok(Self { device, state })
    }

    /// 直近に検知した立ち上がりを 1 回だけ取り出す。フラグを立ててからこれが呼ばれるまでに
    /// 複数回の立ち上がりがあっても、まとめて 1 回として返す（多重開始を招かないため）。
    pub fn take_activated(&self) -> bool {
        self.state.activated.swap(false, Ordering::SeqCst)
    }
}

impl Drop for MicMonitor {
    fn drop(&mut self) {
        let address = running_somewhere_address();
        let client_data = Arc::as_ptr(&self.state) as *mut c_void;
        let status = unsafe {
            AudioObjectRemovePropertyListener(
                self.device,
                NonNull::from(&address),
                Some(property_listener),
                client_data,
            )
        };
        // 後始末の失敗も握りつぶさずログに残す（`docs/rules/error-handling.md`）。
        if status != OS_STATUS_OK {
            eprintln!("プロパティリスナーの解除に失敗した (OSStatus={status})");
        }
    }
}

/// `IsRunningSomewhere` の変化通知を受け取るリスナー。CoreAudio 管理のスレッドから呼ばれる。
///
/// 変化通知は「何かが変わった」ことしか伝えないため、現在の稼働状態を読み直して、
/// 非稼働→稼働の立ち上がりだけを共有フラグに記録する。重い処理はせず即座に返す。
///
/// # Safety
///
/// `in_client_data` は `start()` で登録した `Arc<MonitorState>` の実体を指す有効なポインタで
/// なければならない（リスナー解除まで生存することを `MicMonitor` が保証する）。
unsafe extern "C-unwind" fn property_listener(
    in_object_id: AudioObjectID,
    _in_number_addresses: u32,
    _in_addresses: NonNull<AudioObjectPropertyAddress>,
    in_client_data: *mut c_void,
) -> i32 {
    let state = unsafe { &*(in_client_data as *const MonitorState) };
    // 稼働状態を読めなかった通知は無視する（was_running を書き換えず、誤検知も起こさない）。
    let Some(running) = device_is_running(in_object_id) else {
        return OS_STATUS_OK;
    };
    let was_running = state.was_running.swap(running, Ordering::SeqCst);
    if running && !was_running {
        state.activated.store(true, Ordering::SeqCst);
    }
    OS_STATUS_OK
}

/// `IsRunningSomewhere` を指すプロパティアドレス（グローバルスコープ・主エレメント）。
fn running_somewhere_address() -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceIsRunningSomewhere,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// 既定入力デバイスの ID を取得する。取得に失敗、または未設定（0）なら `None`。
fn default_input_device() -> Option<AudioObjectID> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioHardwarePropertyDefaultInputDevice,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut device: AudioObjectID = 0;
    let mut size = size_of::<AudioObjectID>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            kAudioObjectSystemObject as AudioObjectID,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut device).cast(),
        )
    };
    if status == OS_STATUS_OK && device != 0 {
        Some(device)
    } else {
        None
    }
}

/// 指定デバイスの `IsRunningSomewhere`（いずれかのプロセスが稼働させているか）を読む。
/// 取得に失敗したら `None`（呼び出し側が通知の無視・既定値扱いを決める）。
fn device_is_running(device: AudioObjectID) -> Option<bool> {
    let address = running_somewhere_address();
    let mut value: u32 = 0;
    let mut size = size_of::<u32>() as u32;
    let status = unsafe {
        AudioObjectGetPropertyData(
            device,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
            NonNull::from(&mut value).cast(),
        )
    };
    if status == OS_STATUS_OK {
        Some(value != 0)
    } else {
        None
    }
}
