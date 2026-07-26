//! Mac App Store 可否の技術検証プローブ（#77）。**検証専用**で、出荷バイナリには入らない。
//!
//! 判定したいのは 2 点（プラン `docs/plans/done/20260722-mac-app-store-submission.md` の
//! フェーズ 1 ゲート）:
//!
//! 1. `src/app_audio_monitor.rs` が使う private API
//!    `responsibility_get_pid_responsible_for_pid`（ヘルパープロセス → 親アプリのバンドル ID 解決）を
//!    **公開 API で置き換えられるか**。候補は `proc_pidpath`（実行パスから外側の `.app` を切り出す）と
//!    `proc_pidinfo`（親 PID を辿る）で、private 方式と解決結果を突き合わせる。
//! 2. App Sandbox の下で、自動録音に要る OS 機能（CoreAudio のプロセス照会・ScreenCaptureKit・
//!    上記の代替 API・security-scoped bookmark）が**動くか**。
//!
//! そのため本体のコードは変更せず、比較に要る最小限をこのファイルへ複製している
//! （private 方式の実装は `app_audio_monitor::responsible_pid` と同じもの）。
//! サンドボックス有無の両方で走らせて出力を比べる想定で、`scripts/mas-probe.sh` が
//! ad-hoc 署名した `.app` に包んで実行する。
//!
//! ```sh
//! cargo run --example mas_probe                 # サンドボックス無しで全チェック
//! ./scripts/mas-probe.sh --sandbox              # App Sandbox 有効の .app として実行
//! ```

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("mas_probe only runs on macOS.");
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
fn main() {
    probe::run();
}

#[cfg(target_os = "macos")]
mod probe {
    use std::collections::{BTreeMap, BTreeSet};
    use std::ffi::{OsStr, c_char, c_int, c_void};
    use std::mem::size_of;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use objc2_app_kit::NSRunningApplication;
    use objc2_core_audio::{
        AudioObjectGetPropertyData, AudioObjectGetPropertyDataSize, AudioObjectID,
        AudioObjectPropertyAddress, AudioObjectPropertySelector,
        kAudioHardwarePropertyProcessObjectList, kAudioObjectPropertyElementMain,
        kAudioObjectPropertyScopeGlobal, kAudioObjectSystemObject, kAudioProcessPropertyBundleID,
        kAudioProcessPropertyIsRunningInput, kAudioProcessPropertyPID,
    };
    use objc2_foundation::{
        NSBundle, NSData, NSString, NSURL, NSURLBookmarkCreationOptions,
        NSURLBookmarkResolutionOptions,
    };

    /// 出力バッファ。`--report <path>` を付けたときは、標準出力と同じ内容をここへ溜めてから書き出す。
    /// LaunchServices（`open`）経由で起動すると標準出力が捨てられるため、TCC がアプリ本体を
    /// 識別する起動方法でも結果を回収できるようにする。
    static REPORT: Mutex<String> = Mutex::new(String::new());

    /// `println!` と同じ書式で、標準出力とレポートバッファの両方へ書く。
    macro_rules! say {
        ($($arg:tt)*) => {{
            let line = format!($($arg)*);
            println!("{line}");
            if let Ok(mut report) = REPORT.lock() {
                report.push_str(&line);
                report.push('\n');
            }
        }};
    }

    /// CoreAudio の成功を表す `OSStatus`（= `noErr`）。
    const OS_STATUS_OK: i32 = 0;
    /// ScreenCaptureKit のサンプルが届くのを待つ上限。届かなければ「開始はできたがサンプル無し」と記録する。
    const AUDIO_WAIT: Duration = Duration::from_secs(3);

    /// 解決を試みる 1 プロセス。CoreAudio 由来の母集団だけ `running_input` / `ca_bundle` を持つ
    /// （`--all-processes` の母集団は CoreAudio を経由しないため両方 `None`）。
    struct ProcessSample {
        pid: i32,
        running_input: Option<bool>,
        ca_bundle: Option<String>,
    }

    /// 1 プロセスぶんの解決結果。private 方式と公開 API 方式を横並びで比べるための行。
    struct Resolution {
        pid: i32,
        /// 実行ファイルのパス（`proc_pidpath`）。取得できなければ `None`。
        exec_path: Option<PathBuf>,
        /// マイク入力中か。CoreAudio 由来の行だけ `Some`。
        running_input: Option<bool>,
        /// `NSRunningApplication` が直接返すバンドル ID（ヘルパーでは `None` になりがち）。
        direct: Option<String>,
        /// 公開 API 案 3: CoreAudio 自身が持つ `kAudioProcessPropertyBundleID`。CoreAudio 由来の行だけ `Some`。
        ca_bundle: Option<String>,
        /// private: responsible pid → バンドル ID。
        responsible: Option<String>,
        /// 公開 API 案 1: 実行パスの**外側**の `.app` → バンドル ID。
        by_path: Option<String>,
        /// 公開 API 案 2: 親 PID を辿って `NSRunningApplication` → バンドル ID。
        by_ppid: Option<String>,
    }

    impl Resolution {
        /// 「そのプロセスをどのアプリに帰属させるか」の最終値。本体プロセスは直接のバンドル ID で
        /// 足りるため、`app_audio_monitor::input_running_bundle_ids` と同じく直接 → 親の順で見る。
        fn effective(direct: &Option<String>, parent: &Option<String>) -> Option<String> {
            direct.clone().or_else(|| parent.clone())
        }

        fn private_effective(&self) -> Option<String> {
            Self::effective(&self.direct, &self.responsible)
        }

        fn path_effective(&self) -> Option<String> {
            Self::effective(&self.direct, &self.by_path)
        }

        fn ppid_effective(&self) -> Option<String> {
            Self::effective(&self.direct, &self.by_ppid)
        }

        /// 案 3 は単独で「そのオーディオプロセスのアプリ」を返すので、直接のバンドル ID と併用しない。
        fn ca_effective(&self) -> Option<String> {
            self.ca_bundle.clone()
        }
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        let flags: BTreeSet<&str> = args.iter().map(String::as_str).collect();
        if flags.contains("--help") || flags.contains("-h") {
            print_usage();
            return;
        }
        // 既定の保存先は HOME 配下にする。App Sandbox 下の HOME はコンテナ（書ける場所）を指すため、
        // 引数を省いてもサンドボックス有無のどちらでも書ける。
        let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_owned()));
        let bookmark_file = value_of(&args, "--bookmark-file")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join("mas-probe-bookmark.bin"));
        let folder = value_of(&args, "--folder").map(PathBuf::from);
        let report_file = value_of(&args, "--report").map(PathBuf::from);

        report_environment();

        // マイクは走査より先に掴む。自分自身が「マイク入力中」として CoreAudio に現れるので、
        // サンドボックスされたアプリのマイク使用がどう見えるか（本人か、代行プロセスか）を
        // 通話なしで確かめられる。ストリームは走査が終わるまで生かす。
        let _mic = if flags.contains("--hold-mic") {
            hold_mic()
        } else {
            None
        };

        report_resolution(
            flags.contains("--all-processes"),
            flags.contains("--verbose"),
        );

        if flags.contains("--pick-folder") {
            report_bookmark_save(&bookmark_file, folder.as_deref());
        } else if flags.contains("--resolve-bookmark") {
            report_bookmark_resolve(&bookmark_file);
        }

        if flags.contains("--skip-screen") {
            section("ScreenCaptureKit (system audio)");
            say!("  skipped (--skip-screen)");
        } else {
            report_screen_capture();
        }

        say!("");
        say!("Done.");

        if let Some(report_file) = report_file {
            let report = REPORT
                .lock()
                .map(|report| report.clone())
                .unwrap_or_default();
            match std::fs::write(&report_file, report) {
                Ok(()) => println!("Report written to {}", report_file.display()),
                Err(err) => eprintln!("Could not write {}: {err}", report_file.display()),
            }
        }
    }

    fn print_usage() {
        say!("Usage: mas_probe [options]");
        say!("");
        say!("  --all-processes            Compare every running process, not only audio ones");
        say!("  --verbose                  Print every resolved row");
        say!("  --hold-mic                 Open the default microphone during the scan");
        say!("  --skip-screen              Skip the ScreenCaptureKit check");
        say!(
            "  --pick-folder              Open a folder panel and save a security-scoped bookmark"
        );
        say!("  --resolve-bookmark         Resolve a saved bookmark and write a probe file there");
        say!("  --bookmark-file <path>     Where the bookmark blob is stored");
        say!("  --folder <path>            Use this folder instead of opening the panel");
        say!("  --report <path>            Also write the report there (for `open`-launched runs)");
    }

    /// `--flag value` 形式の値を取り出す。検証用なので厳密なパーサは持たない。
    fn value_of(args: &[String], flag: &str) -> Option<String> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index + 1).cloned()
    }

    fn section(title: &str) {
        say!("");
        say!("== {title} ==");
    }

    // ---------------------------------------------------------------- 環境

    /// サンドボックス内かどうかとプロセスの素性を出す。サンドボックス有無で同じバイナリを走らせて
    /// 出力を比べるため、まずどちらで動いているかを明示する。
    fn report_environment() {
        section("Environment");
        let exec = current_exec_path();
        say!("  pid: {}", std::process::id());
        say!(
            "  executable: {}",
            exec.as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_owned())
        );
        say!("  sandboxed: {}", if sandboxed() { "yes" } else { "no" });
        say!(
            "  container: {}",
            std::env::var("HOME").unwrap_or_else(|_| "<unset>".to_owned())
        );
        say!(
            "  bundle id: {}",
            main_bundle_id().unwrap_or_else(|| "<none>".to_owned())
        );
    }

    /// サンドボックス判定。App Sandbox 下では `HOME` がコンテナ（`~/Library/Containers/<id>/Data`）へ
    /// 差し替わるため、これで見分ける（`sandbox_check` は private API なので使わない）。
    fn sandboxed() -> bool {
        std::env::var("HOME").is_ok_and(|home| home.contains("/Library/Containers/"))
    }

    fn main_bundle_id() -> Option<String> {
        Some(NSBundle::mainBundle().bundleIdentifier()?.to_string())
    }

    fn current_exec_path() -> Option<PathBuf> {
        proc_path(std::process::id() as i32)
    }

    // ----------------------------------------------------- マイクを掴む

    /// 既定入力デバイスを開いて掴んだままにする（`--hold-mic`）。返り値のストリームを drop すると
    /// 解放されるので、呼び出し側は走査が終わるまで保持する。
    ///
    /// 目的は録音ではなく、**自分自身を「マイク入力中のプロセス」として CoreAudio に登場させる**こと。
    /// サンドボックス下では `device.audio-input` entitlement とマイクの TCC 許可が要る。
    fn hold_mic() -> Option<cpal::Stream> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        section("Microphone (held open for the scan)");
        let host = cpal::default_host();
        let Some(device) = host.default_input_device() else {
            say!("  no default input device");
            return None;
        };
        let name = device
            .description()
            .map(|description| description.name().to_owned())
            .unwrap_or_else(|_| "<unnamed>".to_owned());
        let config = match device.default_input_config() {
            Ok(config) => config,
            Err(err) => {
                say!("  could not read the default input config: {err}");
                return None;
            }
        };
        // 中身は使わないので、サンプル形式に依らない raw ストリームで開く。
        let stream = device.build_input_stream_raw(
            config.config(),
            config.sample_format(),
            |_data, _info| {},
            |err| eprintln!("Input stream error: {err}"),
            None,
        );
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                say!("  could not open the input stream: {err}");
                return None;
            }
        };
        if let Err(err) = stream.play() {
            say!("  could not start the input stream: {err}");
            return None;
        }
        // CoreAudio がプロセスオブジェクトへ反映するまで少し待つ（即座に走査すると乗らない）。
        std::thread::sleep(Duration::from_millis(1500));
        say!("  holding {name} open ({:?})", config.sample_format());
        Some(stream)
    }

    // ------------------------------------------------- 解決方式の突き合わせ

    /// private 方式と公開 API 方式の解決結果を並べ、一致・不一致を数える。
    ///
    /// 既定の母集団は CoreAudio のプロセスオブジェクト一覧（＝自動録音が実際に走査する集合）。
    /// `--all-processes` を付けると全プロセスへ広げ、標本を増やす。
    fn report_resolution(all_processes: bool, verbose: bool) {
        section("Bundle-id resolution (private vs public APIs)");
        let (pids, source) = if all_processes {
            (all_pids(), "all running processes")
        } else {
            match audio_pids() {
                Some(pids) => (pids, "CoreAudio process objects"),
                None => {
                    say!("  CoreAudio process list is unavailable (needs macOS 14.4+).");
                    say!(
                        "  → auto-record cannot work here; rerun with --all-processes to still compare resolvers."
                    );
                    return;
                }
            }
        };
        say!("  population: {source} ({} pids)", pids.len());

        let rows: Vec<Resolution> = pids.into_iter().map(resolve).collect();

        let input_running: Vec<&Resolution> = rows
            .iter()
            .filter(|row| row.running_input == Some(true))
            .collect();
        say!("  processes with the mic open: {}", input_running.len());
        if !input_running.is_empty() {
            for row in &input_running {
                say!(
                    "    pid {} → private={} path={} ppid={} coreaudio={} exec={}",
                    row.pid,
                    show(&row.private_effective()),
                    show(&row.path_effective()),
                    show(&row.ppid_effective()),
                    show(&row.ca_effective()),
                    row.exec_path
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<unreadable>".to_owned())
                );
            }
        }

        // ヘルパープロセス（直接のバンドル ID が取れない）が本題。ここが private 方式と一致するかで
        // go/no-go が決まる。
        let helpers: Vec<&Resolution> = rows.iter().filter(|row| row.direct.is_none()).collect();
        say!(
            "  processes without a direct bundle id (helpers): {}",
            helpers.len()
        );

        let mut path_agree = 0usize;
        let mut ppid_agree = 0usize;
        let mut ca_agree = 0usize;
        let mut mismatches: Vec<&Resolution> = Vec::new();
        for row in &rows {
            let private = row.private_effective();
            if private == row.path_effective() {
                path_agree += 1;
            } else {
                mismatches.push(row);
            }
            if private == row.ppid_effective() {
                ppid_agree += 1;
            }
            if private == row.ca_effective() {
                ca_agree += 1;
            }
        }
        say!(
            "  agreement with the private API: proc_pidpath {}/{}, ppid chain {}/{}, CoreAudio bundle id {}/{}",
            path_agree,
            rows.len(),
            ppid_agree,
            rows.len(),
            ca_agree,
            rows.len()
        );

        if !mismatches.is_empty() {
            say!("  mismatches (private vs proc_pidpath):");
            for row in &mismatches {
                say!(
                    "    pid {} private={} path={} coreaudio={} exec={}",
                    row.pid,
                    show(&row.private_effective()),
                    show(&row.path_effective()),
                    show(&row.ca_effective()),
                    row.exec_path
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<unreadable>".to_owned())
                );
            }
        }

        if verbose {
            say!("  all rows (pid / direct / private / path / ppid / coreaudio / exec):");
            for row in &rows {
                say!(
                    "    {} | {} | {} | {} | {} | {} | {}",
                    row.pid,
                    show(&row.direct),
                    show(&row.responsible),
                    show(&row.by_path),
                    show(&row.by_ppid),
                    show(&row.ca_bundle),
                    row.exec_path
                        .as_deref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "<unreadable>".to_owned())
                );
            }
        }

        // 会議アプリごとの内訳。ヘルパーを親アプリへ畳めているかを目視できるようにする。
        report_per_app(&rows);
    }

    /// 主要アプリ（Chrome / Zoom / Slack など）ごとに、ヘルパーを何件・どのバンドル ID に畳めたかを出す。
    fn report_per_app(rows: &[Resolution]) {
        let mut by_app: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
        for row in rows {
            let Some(app) = row.path_effective().or_else(|| row.private_effective()) else {
                continue;
            };
            let entry = by_app.entry(app).or_insert((0, 0, 0));
            entry.0 += 1;
            if row.direct.is_none() {
                entry.1 += 1;
                if row.private_effective() == row.path_effective() {
                    entry.2 += 1;
                }
            }
        }
        say!("  per app (total / helpers / helpers where both methods agree):");
        for (app, (total, helpers, agree)) in by_app {
            say!("    {app}: {total} / {helpers} / {agree}");
        }
    }

    fn show(value: &Option<String>) -> String {
        value.clone().unwrap_or_else(|| "-".to_owned())
    }

    fn resolve(sample: ProcessSample) -> Resolution {
        let pid = sample.pid;
        let exec_path = proc_path(pid);
        Resolution {
            pid,
            ca_bundle: sample.ca_bundle,
            direct: bundle_id_for_pid(pid),
            responsible: responsible_pid(pid).and_then(bundle_id_for_pid),
            by_path: exec_path.as_deref().and_then(bundle_id_for_exec_path),
            by_ppid: responsible_pid_via_parents(pid).and_then(bundle_id_for_pid),
            exec_path,
            running_input: sample.running_input,
        }
    }

    // ---------------------------------------------------- CoreAudio 側の照会

    /// CoreAudio のプロセスオブジェクト一覧を `(pid, マイク入力中か, CoreAudio が持つバンドル ID)` で
    /// 返す。macOS 14.4 未満やサンドボックスで照会できない場合は `None`
    /// （＝自動録音そのものが成立しない）。
    fn audio_pids() -> Option<Vec<ProcessSample>> {
        let processes = process_object_list()?;
        let mut samples = Vec::new();
        for process in processes {
            let Some(pid) = process_pid(process) else {
                continue;
            };
            samples.push(ProcessSample {
                pid,
                running_input: process_is_running_input(process),
                ca_bundle: process_bundle_id(process),
            });
        }
        Some(samples)
    }

    fn global_address(selector: AudioObjectPropertySelector) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            mSelector: selector,
            mScope: kAudioObjectPropertyScopeGlobal,
            mElement: kAudioObjectPropertyElementMain,
        }
    }

    fn process_object_list() -> Option<Vec<AudioObjectID>> {
        let address = global_address(kAudioHardwarePropertyProcessObjectList);
        let mut size: u32 = 0;
        // SAFETY: address / size は有効なローカル変数を指し、修飾子は無し（0, null）。
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
            say!("  AudioObjectGetPropertyDataSize failed (OSStatus={status})");
            return None;
        }
        let count = size as usize / size_of::<AudioObjectID>();
        let mut processes = vec![0 as AudioObjectID; count];
        let out = NonNull::new(processes.as_mut_ptr())?;
        // SAFETY: out は size バイトぶん確保済みのバッファ先頭を指す。
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
            say!("  AudioObjectGetPropertyData failed (OSStatus={status})");
            return None;
        }
        processes.truncate(size as usize / size_of::<AudioObjectID>());
        Some(processes)
    }

    fn process_u32(process: AudioObjectID, selector: AudioObjectPropertySelector) -> Option<u32> {
        let address = global_address(selector);
        let mut value: u32 = 0;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: value は u32 で、size もその大きさを伝えている。
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

    fn process_is_running_input(process: AudioObjectID) -> Option<bool> {
        process_u32(process, kAudioProcessPropertyIsRunningInput).map(|value| value != 0)
    }

    fn process_pid(process: AudioObjectID) -> Option<i32> {
        process_u32(process, kAudioProcessPropertyPID).map(|value| value as i32)
    }

    /// 公開 API 案 3: CoreAudio 自身が持つ `kAudioProcessPropertyBundleID`（macOS 14+）。
    ///
    /// プロセス側から親アプリを推測するのではなく、オーディオ HAL が「このオーディオプロセスは
    /// どのアプリのものか」として持っている値を読む。サンドボックスされたアプリの音声を
    /// `com.apple.audio.SandboxHelper` が代行する構成でも、HAL 側は本来のクライアントを知っている
    /// 可能性があるため、案 1・2 が届かないケースの本命候補。
    fn process_bundle_id(process: AudioObjectID) -> Option<String> {
        use objc2::rc::Retained;

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
            return None;
        }
        let ptr = NonNull::new(value)?;
        // SAFETY: CoreAudio は CFStringRef を +1（呼び出し側が解放する契約）で返す。CFString と
        // NSString は toll-free bridge なので、その +1 をそのまま Retained に渡して解放を任せる。
        let string = unsafe { Retained::from_raw(ptr.as_ptr()) }?;
        let bundle_id = string.to_string();
        (!bundle_id.is_empty()).then_some(bundle_id)
    }

    // ------------------------------------------------------- 解決方式の実装

    fn bundle_id_for_pid(pid: i32) -> Option<String> {
        let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
        Some(app.bundleIdentifier()?.to_string())
    }

    /// private 方式（現行実装と同一）。`app_audio_monitor::responsible_pid` の複製。
    fn responsible_pid(pid: i32) -> Option<i32> {
        use std::sync::OnceLock;

        type ResponsibleFn = unsafe extern "C" fn(c_int) -> c_int;
        static RESOLVER: OnceLock<Option<ResponsibleFn>> = OnceLock::new();

        let resolver = RESOLVER.get_or_init(|| {
            unsafe extern "C" {
                fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
            }
            let rtld_default = (-2isize) as *mut c_void;
            // SAFETY: シンボル名は有効な C 文字列。見つからなければ null が返るだけ。
            let sym = unsafe {
                dlsym(
                    rtld_default,
                    c"responsibility_get_pid_responsible_for_pid".as_ptr(),
                )
            };
            if sym.is_null() {
                None
            } else {
                // SAFETY: 非 null を確認済み。実シグネチャは
                // `pid_t responsibility_get_pid_responsible_for_pid(pid_t)`（TCC・Chromium 等での既知利用）。
                Some(unsafe { std::mem::transmute::<*mut c_void, ResponsibleFn>(sym) })
            }
        });

        let func = (*resolver)?;
        // SAFETY: 解決済みの C 関数を pid_t 引数で呼ぶだけ。メモリは触らない。
        let responsible = unsafe { func(pid) };
        (responsible > 0 && responsible != pid).then_some(responsible)
    }

    /// 公開 API 案 1 の要: 実行パスから**最も外側**の `.app` を切り出し、そのバンドル ID を読む。
    ///
    /// Chrome のヘルパーは
    /// `/Applications/Google Chrome.app/…/Helpers/Google Chrome Helper.app/Contents/MacOS/…` のように
    /// `.app` が入れ子になるため、内側（ヘルパー自身の `.app`）ではなく**外側**を採る。
    fn bundle_id_for_exec_path(exec_path: &Path) -> Option<String> {
        let app_path = outermost_app_bundle(exec_path)?;
        let path_str = app_path.to_str()?;
        let bundle = NSBundle::bundleWithPath(&NSString::from_str(path_str))?;
        Some(bundle.bundleIdentifier()?.to_string())
    }

    /// パスに含まれる `.app` のうち、最も外側（先頭側）のものまでを返す。無ければ `None`。
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

    /// 公開 API 案 2: 親 PID を辿り、最初にバンドル ID を持つ祖先を返す。zygote 構成のように
    /// ヘルパーの親がさらにヘルパーである場合に備えて多段で辿る（launchd = pid 1 で打ち切る）。
    fn responsible_pid_via_parents(pid: i32) -> Option<i32> {
        const MAX_DEPTH: usize = 8;
        let mut current = pid;
        for _ in 0..MAX_DEPTH {
            let parent = parent_pid(current)?;
            if parent <= 1 {
                return None;
            }
            if bundle_id_for_pid(parent).is_some() {
                return Some(parent);
            }
            current = parent;
        }
        None
    }

    /// `proc_pidinfo(PROC_PIDTBSDINFO)` で親 PID を読む。
    ///
    /// issue が挙げていた `sysctl(KERN_PROC_PID)` と同じ情報だが、`struct kinfo_proc` の巨大な
    /// レイアウトを Rust 側で写す必要がなく、公開ヘッダ（`libproc.h`）の範囲で済むこちらを採る。
    fn parent_pid(pid: i32) -> Option<i32> {
        /// `libproc.h` の `PROC_PIDTBSDINFO`。
        const PROC_PIDTBSDINFO: c_int = 3;

        /// `struct proc_bsdinfo`（`sys/proc_info.h`）。必要なのは `pbi_ppid` だけだが、
        /// `proc_pidinfo` は構造体全体ぶんのバッファを要求するため全フィールドを写す。
        #[repr(C)]
        struct ProcBsdInfo {
            pbi_flags: u32,
            pbi_status: u32,
            pbi_xstatus: u32,
            pbi_pid: u32,
            pbi_ppid: u32,
            pbi_uid: u32,
            pbi_gid: u32,
            pbi_ruid: u32,
            pbi_rgid: u32,
            pbi_svuid: u32,
            pbi_svgid: u32,
            rfu_1: u32,
            pbi_comm: [c_char; 16],
            pbi_name: [c_char; 32],
            pbi_nfiles: u32,
            pbi_pgid: u32,
            pbi_pjobc: u32,
            e_tdev: u32,
            e_tpgid: u32,
            pbi_nice: i32,
            pbi_start_tvsec: u64,
            pbi_start_tvusec: u64,
        }

        unsafe extern "C" {
            fn proc_pidinfo(
                pid: c_int,
                flavor: c_int,
                arg: u64,
                buffer: *mut c_void,
                buffersize: c_int,
            ) -> c_int;
        }

        // SAFETY: `ProcBsdInfo` は整数と `c_char` 配列だけの POD なので、全ゼロは有効な値。
        let mut info: ProcBsdInfo = unsafe { std::mem::zeroed() };
        let size = size_of::<ProcBsdInfo>() as c_int;
        // SAFETY: buffer は size バイトの有効な書き込み先。proc_pidinfo は書き込んだバイト数を返す。
        let written = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        (written == size).then_some(info.pbi_ppid as i32)
    }

    /// `proc_pidpath` で実行ファイルの絶対パスを読む（公開ヘッダ `libproc.h`）。
    fn proc_path(pid: i32) -> Option<PathBuf> {
        /// `libproc.h` の `PROC_PIDPATHINFO_MAXSIZE`（4 * MAXPATHLEN）。
        const PROC_PIDPATHINFO_MAXSIZE: usize = 4 * 1024;

        unsafe extern "C" {
            fn proc_pidpath(pid: c_int, buffer: *mut c_void, buffersize: u32) -> c_int;
        }

        let mut buffer = vec![0u8; PROC_PIDPATHINFO_MAXSIZE];
        // SAFETY: buffer は buffersize バイトの有効な書き込み先。戻り値は書き込まれた長さ（0 は失敗）。
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
        Some(PathBuf::from(OsStr::from_bytes(&buffer).to_owned()))
    }

    // ----------------------------------------------- ScreenCaptureKit の検証

    /// システム音声キャプチャを短時間だけ動かし、サンプルが届くかを見る。サンドボックス下で
    /// 「共有可能コンテンツの取得」「キャプチャ開始」「サンプル受信」のどこで落ちるかを切り分ける。
    fn report_screen_capture() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use screencapturekit::prelude::*;
        use screencapturekit::stream::configuration::audio::{AudioChannelCount, AudioSampleRate};

        struct CountingHandler {
            samples: Arc<AtomicUsize>,
        }

        impl SCStreamOutputTrait for CountingHandler {
            fn did_output_sample_buffer(
                &self,
                _sample: CMSampleBuffer,
                of_type: SCStreamOutputType,
            ) {
                if of_type == SCStreamOutputType::Audio {
                    self.samples.fetch_add(1, Ordering::Relaxed);
                }
            }
        }

        section("ScreenCaptureKit (system audio)");
        let content = match SCShareableContent::get() {
            Ok(content) => content,
            Err(err) => {
                say!("  SCShareableContent::get failed: {err}");
                say!("  → screen-recording permission (TCC) or the sandbox is blocking it.");
                return;
            }
        };
        let displays = content.displays();
        say!("  shareable displays: {}", displays.len());
        let Some(display) = displays.first() else {
            say!("  no display found; cannot start a capture");
            return;
        };

        let filter = SCContentFilter::create()
            .with_display(display)
            .with_excluding_windows(&[])
            .build();
        let config = SCStreamConfiguration::new()
            .with_width(2)
            .with_height(2)
            .with_captures_audio(true)
            .with_sample_rate(AudioSampleRate::Rate48000)
            .with_channel_count(AudioChannelCount::Stereo)
            .with_excludes_current_process_audio(true);

        let samples = Arc::new(AtomicUsize::new(0));
        let mut stream = SCStream::new(&filter, &config);
        stream.add_output_handler(
            CountingHandler {
                samples: Arc::clone(&samples),
            },
            SCStreamOutputType::Audio,
        );
        if let Err(err) = stream.start_capture() {
            say!("  start_capture failed: {err}");
            return;
        }
        say!("  capture started; waiting up to {AUDIO_WAIT:?} for audio buffers…");
        let deadline = Instant::now() + AUDIO_WAIT;
        while Instant::now() < deadline && samples.load(Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        let received = samples.load(Ordering::Relaxed);
        if let Err(err) = stream.stop_capture() {
            say!("  stop_capture failed: {err}");
        }
        say!("  audio sample buffers received: {received}");
        if received == 0 {
            say!("  → the capture ran but nothing was playing; play audio and rerun to confirm.");
        }
    }

    // ------------------------------------------- security-scoped bookmark

    /// フォルダ選択（`rfd` = NSOpenPanel）→ security-scoped bookmark を作って保存する。
    /// サンドボックスでは、パネルで選んだ URL からしか bookmark を作れないためこの順になる。
    fn report_bookmark_save(bookmark_file: &Path, preset_folder: Option<&Path>) {
        section("Security-scoped bookmark (save)");
        say!("  bookmark file: {}", bookmark_file.display());
        // `--folder` を渡すとパネルを出さずにそのパスを使う。サンドボックス内のコンテナに対して
        // bookmark の作成・解決の往復だけを（人手を介さず）確かめるための逃げ道で、
        // 「パネルで選んだコンテナ外のフォルダに書けるか」は `--folder` 無しでしか検証できない。
        let folder = match preset_folder {
            Some(folder) => folder.to_path_buf(),
            None => {
                let Some(folder) = rfd::FileDialog::new().pick_folder() else {
                    say!("  no folder was selected");
                    return;
                };
                folder
            }
        };
        say!("  selected: {}", folder.display());

        let Some(folder_str) = folder.to_str() else {
            say!("  the selected path is not valid UTF-8");
            return;
        };
        let url = NSURL::fileURLWithPath(&NSString::from_str(folder_str));
        let data = match url
            .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                NSURLBookmarkCreationOptions::WithSecurityScope,
                None,
                None,
            ) {
            Ok(data) => data,
            Err(err) => {
                say!("  bookmarkDataWithOptions failed: {err}");
                return;
            }
        };
        let bytes = data.to_vec();
        if let Err(err) = std::fs::write(bookmark_file, &bytes) {
            say!("  could not write {}: {err}", bookmark_file.display());
            return;
        }
        say!(
            "  saved {} bytes to {}",
            bytes.len(),
            bookmark_file.display()
        );
        say!("  → rerun with --resolve-bookmark to check access from a fresh launch.");
    }

    /// 保存済み bookmark を解決し、アクセス権を開いて実際に書けるかまで確かめる。
    /// 「再起動後もアクセスできるか」の検証なので、保存とは別プロセスで走らせる。
    fn report_bookmark_resolve(bookmark_file: &Path) {
        section("Security-scoped bookmark (resolve)");
        say!("  bookmark file: {}", bookmark_file.display());
        let bytes = match std::fs::read(bookmark_file) {
            Ok(bytes) => bytes,
            Err(err) => {
                say!("  could not read {}: {err}", bookmark_file.display());
                return;
            }
        };
        let data = NSData::with_bytes(&bytes);
        let mut stale = objc2::runtime::Bool::NO;
        // SAFETY: data は有効な NSData、stale は有効な Bool を指す。相対 URL は使わない。
        let url = unsafe {
            NSURL::URLByResolvingBookmarkData_options_relativeToURL_bookmarkDataIsStale_error(
                &data,
                NSURLBookmarkResolutionOptions::WithSecurityScope,
                None,
                &raw mut stale,
            )
        };
        let url = match url {
            Ok(url) => url,
            Err(err) => {
                say!("  URLByResolvingBookmarkData failed: {err}");
                return;
            }
        };
        say!(
            "  resolved: {}",
            url.path().map(|p| p.to_string()).unwrap_or_default()
        );
        say!("  stale: {}", stale.as_bool());

        // SAFETY: 解決済みの security-scoped URL に対する呼び出し。stop と対で使う。
        let started = unsafe { url.startAccessingSecurityScopedResource() };
        say!("  startAccessingSecurityScopedResource: {started}");

        let write_result = url.path().map(|path| {
            let probe = PathBuf::from(path.to_string()).join("openshoki-mas-probe.txt");
            let result = std::fs::write(&probe, b"openshoki MAS probe\n");
            if result.is_ok() {
                let _ = std::fs::remove_file(&probe);
            }
            result
        });
        match write_result {
            Some(Ok(())) => say!("  write test: ok"),
            Some(Err(err)) => say!("  write test failed: {err}"),
            None => say!("  write test skipped (no path on the resolved URL)"),
        }

        if started {
            // SAFETY: start が成功したときだけ対で呼ぶ。
            unsafe { url.stopAccessingSecurityScopedResource() };
        }
    }

    /// 全プロセスの pid を列挙する（`--all-processes` 用）。CoreAudio 由来ではないので、
    /// マイク入力の有無と CoreAudio のバンドル ID は分からず `None`。
    fn all_pids() -> Vec<ProcessSample> {
        /// `libproc.h` の `PROC_ALL_PIDS`。
        const PROC_ALL_PIDS: u32 = 1;

        unsafe extern "C" {
            fn proc_listpids(
                r#type: u32,
                typeinfo: u32,
                buffer: *mut c_void,
                buffersize: c_int,
            ) -> c_int;
        }

        // 余裕を持った固定長で 1 回だけ読む（検証用なので厳密な再試行は持たない）。
        let mut pids = vec![0i32; 8192];
        let size = (pids.len() * size_of::<i32>()) as c_int;
        // SAFETY: buffer は size バイトの有効な書き込み先。戻り値は書き込まれたバイト数。
        let written =
            unsafe { proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast::<c_void>(), size) };
        if written <= 0 {
            return Vec::new();
        }
        pids.truncate(written as usize / size_of::<i32>());
        pids.into_iter()
            .filter(|&pid| pid > 0)
            .map(|pid| ProcessSample {
                pid,
                running_input: None,
                ca_bundle: None,
            })
            .collect()
    }
}
