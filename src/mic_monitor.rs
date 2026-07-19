//! 他アプリがマイク（入力デバイス）を使い始めたことを検知する監視モニタ（macOS）。
//!
//! CoreAudio の各入力デバイスの `kAudioDevicePropertyDeviceIsRunningSomewhere`
//! （いずれかのプロセスがそのデバイスを稼働させているか）を定期的にポーリングし、
//! **どれかの入力デバイスが非稼働→稼働へ変化した立ち上がり**（＝会議アプリやブラウザが
//! マイクを使い始めた瞬間）を検知する。プロパティの参照自体は録音（マイク権限）を必要としない。
//!
//! ## 全入力デバイスを見る理由
//!
//! ブラウザや会議アプリは、システム既定の入力デバイスとは別のマイク（外付け・iPhone の
//! Continuity マイク等）を使うことがある。既定デバイス 1 台だけを見ると、別デバイスでの
//! マイク使用を取りこぼす。そこで既定に限らず全入力デバイスを対象にし、デバイスの追加/削除
//! （差し替え・接続）にも毎回の列挙で追随する。
//!
//! ## リスナーではなくポーリングにした理由
//!
//! デバイスが動的に増減する中でプロパティリスナーを付け外しし続けるのは複雑で誤りやすい。
//! 本アプリはもともとメニューイベント用に 100ms タイマーを常時回している（`main.rs`）。
//! そこに相乗りして数台のデバイスの稼働状態を `POLL_INTERVAL` ごとに読むだけなら、実装は
//! 単純で安全であり、会議開始から録音開始までの遅延も 1 秒未満に収まる。CoreAudio 連携に
//! 失敗しても呼び出し側はモニタ無しで常駐を続ける（`docs/rules/error-handling.md`）。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::error::Error;
use std::mem::size_of;
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use objc2_core_audio::{
    AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
    AudioObjectPropertyAddress, kAudioDevicePropertyDeviceIsRunningSomewhere,
    kAudioDevicePropertyStreams, kAudioHardwarePropertyDevices, kAudioObjectPropertyElementMain,
    kAudioObjectPropertyScopeGlobal, kAudioObjectPropertyScopeInput, kAudioObjectSystemObject,
};

/// CoreAudio の成功を表す `OSStatus`（= `noErr`）。
const OS_STATUS_OK: i32 = 0;

/// 稼働状態をポーリングする間隔。100ms タイマーから毎回読むと無駄なので、この間隔に間引く。
/// 会議開始の検知としてはこの程度の遅延で十分。
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// 入力デバイスの稼働状態を監視するモニタ。全状態はメインスレッド上でのみ触るため同期は不要。
pub struct MicMonitor {
    /// 直近に観測した各入力デバイスの稼働状態。立ち上がりエッジ判定と、消えたデバイスの掃除に使う。
    running: RefCell<HashMap<AudioObjectID, bool>>,
    /// 最後にポーリングした時刻。`POLL_INTERVAL` 未満の呼び出しは読み取りを省く。
    last_poll: Cell<Instant>,
}

impl MicMonitor {
    /// 監視を開始する。起動時点の各入力デバイスの稼働状態を初期値にして、以後の立ち上がりだけを
    /// 検知できるようにする（起動時に既に使用中だったものを遡って拾わない）。
    ///
    /// 入力デバイスの列挙に失敗した場合はエラーを返す（呼び出し側はモニタ無しで続行する）。
    pub fn start() -> Result<Self, Box<dyn Error>> {
        let mut running = HashMap::new();
        for device in input_devices()? {
            running.insert(device, read_device_running(device).unwrap_or(false));
        }
        Ok(Self {
            running: RefCell::new(running),
            last_poll: Cell::new(Instant::now()),
        })
    }

    /// 前回ポーリング以降に、いずれかの入力デバイスが非稼働→稼働へ変化していたら `true` を返す。
    ///
    /// 100ms タイマーから毎ティック呼ばれる想定。`POLL_INTERVAL` 未満の呼び出しでは読み取りを
    /// 行わず `false` を返す（間引き）。録音中かどうかの判定は呼び出し側が行うため、ここは
    /// 「立ち上がりがあったか」だけを返す。
    pub fn take_activated(&self) -> bool {
        if self.last_poll.get().elapsed() < POLL_INTERVAL {
            return false;
        }
        self.last_poll.set(Instant::now());

        let devices = match input_devices() {
            Ok(devices) => devices,
            Err(err) => {
                eprintln!(
                    "Skipping this monitoring pass because input-device enumeration failed: {err}"
                );
                return false;
            }
        };

        let mut running = self.running.borrow_mut();
        let mut activated = false;
        for &device in &devices {
            let now = read_device_running(device).unwrap_or(false);
            // insert は旧値を返す。未知のデバイス（今回初めて見た）は旧値なし=非稼働扱い。
            let was_running = running.insert(device, now).unwrap_or(false);
            if now && !was_running {
                activated = true;
            }
        }
        // 消えたデバイスのエントリを捨てる（同じ ID で再登場したときに立ち上がりとして扱えるよう）。
        running.retain(|device, _| devices.contains(device));
        activated
    }
}

/// 指定セレクタの、システムオブジェクト用グローバルアドレス（スコープ Global・主エレメント）。
fn global_address(
    selector: objc2_core_audio::AudioObjectPropertySelector,
) -> AudioObjectPropertyAddress {
    AudioObjectPropertyAddress {
        mSelector: selector,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    }
}

/// システムに存在する全 AudioObject（デバイス含む）の一覧を取得する。
fn all_devices() -> Result<Vec<AudioObjectID>, Box<dyn Error>> {
    let address = global_address(kAudioHardwarePropertyDevices);
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
        return Err(format!("Failed to get the device-list size (OSStatus={status})").into());
    }
    let count = size as usize / size_of::<AudioObjectID>();
    let mut devices = vec![0 as AudioObjectID; count];
    let Some(out) = NonNull::new(devices.as_mut_ptr()) else {
        return Ok(devices); // デバイスが 0 台（out バッファが空）なら空で返す。
    };
    // size は確保済みバッファのバイト数として渡し、実際に書き込まれたバイト数が返る。
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
        return Err(format!("Failed to get the device list (OSStatus={status})").into());
    }
    // 実際に書き込まれた要素数へ切り詰める（列挙中に台数が変わってもはみ出さない）。
    devices.truncate(size as usize / size_of::<AudioObjectID>());
    Ok(devices)
}

/// デバイスが入力ストリームを持つ（＝マイク等の入力デバイス）かを判定する。
/// 入力スコープのストリーム一覧のサイズが 0 より大きければ入力デバイス。
fn device_has_input(device: AudioObjectID) -> bool {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyStreams,
        mScope: kAudioObjectPropertyScopeInput,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut size: u32 = 0;
    let status = unsafe {
        AudioObjectGetPropertyDataSize(
            device,
            NonNull::from(&address),
            0,
            std::ptr::null(),
            NonNull::from(&mut size),
        )
    };
    status == OS_STATUS_OK && size > 0
}

/// 入力デバイス（マイク等）だけを列挙する。
fn input_devices() -> Result<Vec<AudioObjectID>, Box<dyn Error>> {
    Ok(all_devices()?
        .into_iter()
        .filter(|&device| device_has_input(device))
        .collect())
}

/// 指定デバイスの `IsRunningSomewhere`（いずれかのプロセスが稼働させているか）を読む。
/// 取得に失敗したら `None`（呼び出し側が既定値扱いを決める）。
fn read_device_running(device: AudioObjectID) -> Option<bool> {
    let address = AudioObjectPropertyAddress {
        mSelector: kAudioDevicePropertyDeviceIsRunningSomewhere,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
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
