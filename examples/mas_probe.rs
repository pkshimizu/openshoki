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
    use std::fs::File;
    use std::mem::size_of;
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Path, PathBuf};
    use std::ptr::NonNull;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
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

    /// `--report <path>` を指定したときの追記先。LaunchServices（`open`）経由で起動すると標準出力が
    /// 捨てられるため、TCC がアプリ本体を識別する起動方法でも結果を回収できるようにする。
    ///
    /// 溜めずに 1 行ずつ書いて flush する。サンドボックス違反で殺される・途中でクラッシュする、
    /// といったこのプローブが一番見たい失敗モードで、そこまでの出力が失われないため。
    static REPORT: Mutex<Option<File>> = Mutex::new(None);

    /// `println!` と同じ書式で、標準出力と（開いていれば）レポートファイルの両方へ書く。
    macro_rules! emit {
        ($($arg:tt)*) => {{
            let line = format!($($arg)*);
            println!("{line}");
            if let Ok(mut report) = REPORT.lock()
                && let Some(file) = report.as_mut()
            {
                use std::io::Write;
                let _ = writeln!(file, "{line}");
                let _ = file.flush();
            }
        }};
    }

    /// CoreAudio の成功を表す `OSStatus`（= `noErr`）。
    const OS_STATUS_OK: i32 = 0;
    /// ScreenCaptureKit のサンプルが届くのを待つ上限。届かなければ「開始はできたがサンプル無し」と記録する。
    const AUDIO_WAIT: Duration = Duration::from_secs(3);
    /// マイクを開いてから走査するまでの待ち。CoreAudio がプロセスオブジェクトへ反映するまで間があり、
    /// 即座に走査すると自分自身が「マイク入力中」に乗らない。
    const MIC_SETTLE: Duration = Duration::from_millis(1500);

    /// 解決を試みる 1 プロセス。CoreAudio 由来の母集団だけ `running_input` /
    /// `core_audio_bundle_id` を持つ（`--all-processes` の母集団は CoreAudio を経由しないため
    /// 両方 `None`）。
    struct ProcessEntry {
        pid: i32,
        running_input: Option<bool>,
        core_audio_bundle_id: Option<String>,
    }

    /// 1 プロセスぶんの解決結果。private 方式と公開 API 方式を横並びで比べるための行。
    struct ResolvedProcess {
        pid: i32,
        /// 実行ファイルのパス（`proc_pidpath`）。取得できなければ `None`。
        exec_path: Option<PathBuf>,
        /// マイク入力中か。CoreAudio 由来の行だけ `Some`。
        running_input: Option<bool>,
        /// `NSRunningApplication` が直接返すバンドル ID（ヘルパーでは `None` になりがち）。
        direct: Option<String>,
        /// 公開 API 案 3: CoreAudio 自身が持つ `kAudioProcessPropertyBundleID`。CoreAudio 由来の行だけ `Some`。
        core_audio_bundle_id: Option<String>,
        /// private: responsible pid → バンドル ID。
        responsible: Option<String>,
        /// 公開 API 案 1: 実行パスの**外側**の `.app` → バンドル ID。
        by_path: Option<String>,
        /// 公開 API 案 2: 親 PID を辿って `NSRunningApplication` → バンドル ID。
        by_ppid: Option<String>,
    }

    impl ResolvedProcess {
        /// 「そのプロセスをどのアプリに帰属させるか」の候補集合。
        ///
        /// 本体の `app_audio_monitor::input_running_bundle_ids` は、直接のバンドル ID と親から
        /// 解決したバンドル ID の**両方**を集合へ入れて照合する（どちらか片方に畳まない）。
        /// 方式の比較もそれに揃えないと、直接の ID が取れる行で両辺が自明に一致してしまい、
        /// 本体が実際に使っている親解決の経路を検証できない。
        fn ids(parts: [&Option<String>; 3]) -> BTreeSet<String> {
            parts.iter().filter_map(|part| (*part).clone()).collect()
        }

        /// 親解決（responsible pid）が、直接のバンドル ID に無い ID を足した行か。
        ///
        /// 集合比較にした後の「本題」はここ。`direct.is_none()`（ヘルパー）で切ると、
        /// 直接の ID を持ちつつ親解決が別の ID を足す行（Safari の WebKit GPU プロセス、
        /// Slack/Claude のヘルパーなど、検証の核心）が漏れる。
        fn parent_added_id(&self) -> bool {
            self.private_ids().len() > self.direct.iter().count()
        }

        /// private 方式（現行実装）。
        fn private_ids(&self) -> BTreeSet<String> {
            Self::ids([&self.direct, &self.responsible, &None])
        }

        /// 公開 API 案 1（`proc_pidpath` の外側 `.app`）。
        fn path_ids(&self) -> BTreeSet<String> {
            Self::ids([&self.direct, &self.by_path, &None])
        }

        /// 公開 API 案 2（親 PID 辿り）。
        fn ppid_ids(&self) -> BTreeSet<String> {
            Self::ids([&self.direct, &self.by_ppid, &None])
        }

        /// 公開 API 案 3（CoreAudio が持つバンドル ID）。private 側と同じ形にするため、
        /// こちらも直接のバンドル ID と併せた集合で比べる。
        fn core_audio_ids(&self) -> BTreeSet<String> {
            Self::ids([&self.direct, &self.core_audio_bundle_id, &None])
        }

        /// 置き換え後の実装が作る想定の集合。判定はこれと private の比較で行う。
        ///
        /// 案 2（親 PID 辿り）は**含めない**。案 1 に対する追加の解決力がほぼ無く（XPC サービスは
        /// `launchd` の子なので辿れない）、採らない方針のため。名前を「公開 API 全部」にしないのは
        /// この差を隠さないため。
        fn planned_ids(&self) -> BTreeSet<String> {
            Self::ids([&self.direct, &self.by_path, &self.core_audio_bundle_id])
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
        if let Some(report_file) = value_of(&args, "--report").map(PathBuf::from) {
            open_report(&report_file);
        }

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

        if let Some(value) = value_of(&args, "--watch-mic") {
            match value.parse::<u64>() {
                Ok(seconds) => watch_mic(Duration::from_secs(seconds)),
                // 黙って飛ばすと、検証者は「通話を始めろ」と言われないまま次の節へ進んでしまう。
                Err(err) => emit!("  --watch-mic needs a number of seconds ({value:?}: {err})"),
            }
        }

        if flags.contains("--pick-folder") {
            report_bookmark_save(&bookmark_file, folder.as_deref());
        } else if flags.contains("--resolve-bookmark") {
            report_bookmark_resolve(&bookmark_file);
        }

        if flags.contains("--skip-screen") {
            section("ScreenCaptureKit (system audio)");
            emit!("  skipped (--skip-screen)");
        } else {
            report_screen_capture();
        }

        emit!("");
        emit!("Done.");
    }

    /// `--report` の書き込み先を開く。中身は「どのユーザーが何を動かしているか」の目録になるため、
    /// `docs/rules/security.md` に従って所有者のみ読み書き可（0600）で作る。
    fn open_report(path: &Path) {
        match private_file(path) {
            Ok(file) => {
                if let Ok(mut report) = REPORT.lock() {
                    *report = Some(file);
                }
            }
            Err(err) => eprintln!("Could not open {}: {err}", path.display()),
        }
    }

    /// 所有者のみ読み書き可（0600）で新規作成する。既存があれば切り詰める。
    fn private_file(path: &Path) -> std::io::Result<File> {
        use std::os::unix::fs::OpenOptionsExt;

        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
    }

    fn print_usage() {
        emit!("Usage: mas_probe [options]");
        emit!("");
        emit!("  --all-processes            Compare every running process, not only audio ones");
        emit!("  --verbose                  Print every resolved row");
        emit!("  --hold-mic                 Open the default microphone during the scan");
        emit!("  --watch-mic <seconds>      Keep watching which processes hold the mic");
        emit!("  --skip-screen              Skip the ScreenCaptureKit check");
        emit!(
            "  --pick-folder              Open a folder panel and save a security-scoped bookmark"
        );
        emit!("  --resolve-bookmark         Resolve a saved bookmark and write a probe file there");
        emit!("  --bookmark-file <path>     Where the bookmark blob is stored");
        emit!("  --folder <path>            Use this folder instead of opening the panel");
        emit!(
            "  --report <path>            Also write the report there (for `open`-launched runs)"
        );
    }

    /// `--flag value` 形式の値を取り出す。検証用なので厳密なパーサは持たない。
    fn value_of(args: &[String], flag: &str) -> Option<String> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index + 1).cloned()
    }

    fn section(title: &str) {
        emit!("");
        emit!("== {title} ==");
    }

    // ---------------------------------------------------------------- 環境

    /// サンドボックス内かどうかとプロセスの素性を出す。サンドボックス有無で同じバイナリを走らせて
    /// 出力を比べるため、まずどちらで動いているかを明示する。
    fn report_environment() {
        section("Environment");
        let exec = current_exec_path();
        emit!("  pid: {}", std::process::id());
        emit!(
            "  executable: {}",
            exec.as_deref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "<unknown>".to_owned())
        );
        let sandboxed = sandboxed();
        emit!("  sandboxed: {}", if sandboxed { "yes" } else { "no" });
        emit!(
            "  {}: {}",
            if sandboxed { "container" } else { "home" },
            std::env::var("HOME").unwrap_or_else(|_| "<unset>".to_owned())
        );
        emit!(
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
            emit!("  no default input device");
            return None;
        };
        let name = device
            .description()
            .map(|description| description.name().to_owned())
            .unwrap_or_else(|_| "<unnamed>".to_owned());
        let config = match device.default_input_config() {
            Ok(config) => config,
            Err(err) => {
                emit!("  could not read the default input config: {err}");
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
                emit!("  could not open the input stream: {err}");
                return None;
            }
        };
        if let Err(err) = stream.play() {
            emit!("  could not start the input stream: {err}");
            return None;
        }
        std::thread::sleep(MIC_SETTLE);
        emit!("  holding {name} open ({:?})", config.sample_format());
        Some(stream)
    }

    /// マイクを掴んでいるプロセスを一定時間見張り、顔ぶれが変わるたびに解決結果を出す。
    ///
    /// 「通話を始めた瞬間にどのプロセスが `IsRunningInput` になるか」を調べるための機能。
    /// 1 回きりの走査だと、走らせる側と通話を始める側でタイミングを合わせる必要があり、
    /// 会議アプリの検証が難しい。
    fn watch_mic(duration: Duration) {
        /// 見張りの間隔。本体の `POLL_INTERVAL` と同じにして、実運用と同じ粒度で観測する。
        const POLL: Duration = Duration::from_millis(500);

        section("Microphone watch");
        emit!("  watching for {duration:?}; start your call now…");
        let deadline = Instant::now() + duration;
        let mut previous: BTreeSet<i32> = BTreeSet::new();
        let mut changes = 0usize;
        while Instant::now() < deadline {
            let Some(entries) = audio_processes() else {
                emit!("  CoreAudio process list became unavailable; stopping the watch");
                return;
            };
            let rows: Vec<ResolvedProcess> = entries
                .into_iter()
                .filter(|entry| entry.running_input == Some(true))
                .map(resolve)
                .collect();
            let current: BTreeSet<i32> = rows.iter().map(|row| row.pid).collect();
            if current != previous {
                changes += 1;
                emit!("  [{changes}] processes holding the mic: {}", rows.len());
                for row in &rows {
                    emit!(
                        "    pid {} → private={} path={} ppid={} coreaudio={} exec={}",
                        row.pid,
                        show_set(&row.private_ids()),
                        show_set(&row.path_ids()),
                        show_set(&row.ppid_ids()),
                        show_set(&row.core_audio_ids()),
                        show_path(row)
                    );
                    if !row.private_ids().is_subset(&row.planned_ids()) {
                        emit!(
                            "      ⚠ the public APIs cannot attribute this process to {}",
                            show_set(
                                &row.private_ids()
                                    .difference(&row.planned_ids())
                                    .cloned()
                                    .collect()
                            )
                        );
                    }
                }
                previous = current;
            }
            std::thread::sleep(POLL);
        }
        emit!("  watch finished ({changes} changes seen)");
    }

    // ------------------------------------------------- 解決方式の突き合わせ

    /// private 方式と公開 API 方式の解決結果を並べ、一致・不一致を数える。
    ///
    /// 既定の母集団は CoreAudio のプロセスオブジェクト一覧（＝自動録音が実際に走査する集合）。
    /// `--all-processes` を付けると全プロセスへ広げ、標本を増やす。
    fn report_resolution(all_processes: bool, verbose: bool) {
        section("Bundle-id resolution (private vs public APIs)");
        let (samples, source) = if all_processes {
            (all_processes_sample(), "all running processes")
        } else {
            match audio_processes() {
                Some(samples) => (samples, "CoreAudio process objects"),
                None => {
                    emit!("  CoreAudio process list is unavailable (needs macOS 14.4+).");
                    emit!(
                        "  → auto-record cannot work here; rerun with --all-processes to still compare resolvers."
                    );
                    return;
                }
            }
        };
        emit!("  population: {source} ({} processes)", samples.len());

        let rows: Vec<ResolvedProcess> = samples.into_iter().map(resolve).collect();

        report_mic_rows(&rows);
        report_agreement(&rows, all_processes);
        if verbose {
            report_all_rows(&rows);
        }
        // 会議アプリごとの内訳。ヘルパーを親アプリへ畳めているかを目視できるようにする。
        report_per_app(&rows);
    }

    /// いまマイクを掴んでいる行だけを詳しく出す。会議アプリが正しく解決できているかを直接見る箇所。
    fn report_mic_rows(rows: &[ResolvedProcess]) {
        let mic_rows: Vec<&ResolvedProcess> = rows
            .iter()
            .filter(|row| row.running_input == Some(true))
            .collect();
        emit!("  processes with the mic open: {}", mic_rows.len());
        for row in mic_rows {
            emit!(
                "    pid {} → private={} path={} ppid={} coreaudio={} exec={}",
                row.pid,
                show_set(&row.private_ids()),
                show_set(&row.path_ids()),
                show_set(&row.ppid_ids()),
                show_set(&row.core_audio_ids()),
                show_path(row)
            );
        }
    }

    /// 解決方式 1 つ。名前と「その方式で得られるバンドル ID 集合」の組。
    type Method = (&'static str, fn(&ResolvedProcess) -> BTreeSet<String>);

    /// 1 方式ぶんの突き合わせ結果。
    struct Agreement {
        /// 比較した行数（private 側も方式側も空の行は「比較対象なし」として除く）。
        compared: usize,
        /// 集合が完全に一致した行数。
        equal: usize,
        /// private が持っていた ID を方式が取りこぼした行数（**代替可否の要**）。
        missing: usize,
        /// 方式だけが持っていた ID がある行数（帰属が広がる／狭まる差の裏返し）。
        extra: usize,
    }

    /// private 方式と `ids` 方式を**集合**で突き合わせる。
    ///
    /// 本体の `app_audio_monitor::input_running_bundle_ids` は「直接のバンドル ID」と
    /// 「親から解決したバンドル ID」の**両方**を集合へ入れる。1 値に畳んで比べると、直接の ID が
    /// 取れる行で両辺が自明に一致してしまい、本体が依存している親解決の経路を検証できない。
    /// そのため比較単位を集合に揃える。
    fn compare_sets(
        rows: &[&ResolvedProcess],
        ids: impl Fn(&ResolvedProcess) -> BTreeSet<String>,
    ) -> Agreement {
        let mut agreement = Agreement {
            compared: 0,
            equal: 0,
            missing: 0,
            extra: 0,
        };
        for row in rows {
            let private = row.private_ids();
            let candidate = ids(row);
            // 両方とも空の行は「どちらの方式でも解決できない」だけで、一致に数えると分母が
            // 水増しされる（`.app` の外にいるデーモンなど、母集団の大半がこれになりうる）。
            if private.is_empty() && candidate.is_empty() {
                continue;
            }
            agreement.compared += 1;
            if private == candidate {
                agreement.equal += 1;
                continue;
            }
            if !private.is_subset(&candidate) {
                agreement.missing += 1;
            }
            if !candidate.is_subset(&private) {
                agreement.extra += 1;
            }
        }
        agreement
    }

    /// 方式ごとの一致率を出す。全行とヘルパー行（直接のバンドル ID が無い＝本題）を分けて示す。
    fn report_agreement(rows: &[ResolvedProcess], all_processes: bool) {
        let all: Vec<&ResolvedProcess> = rows.iter().collect();
        // 親解決が ID を足した行が本題。`direct` が無い行（ヘルパー）だけで切ると、直接の ID を
        // 持ちつつ親解決が別の ID を足す行が漏れる。
        let parent_resolved: Vec<&ResolvedProcess> =
            rows.iter().filter(|row| row.parent_added_id()).collect();
        emit!(
            "  processes without a direct bundle id: {}",
            rows.iter().filter(|row| row.direct.is_none()).count()
        );
        emit!(
            "  processes where the parent resolver added an id: {}",
            parent_resolved.len()
        );
        emit!("  agreement with the private API (sets: direct ∪ parent, matching the real code):");
        emit!(
            "    method              all rows equal/compared missing extra | parent-resolved equal/compared missing extra"
        );

        let mut methods: Vec<Method> = vec![
            ("proc_pidpath", ResolvedProcess::path_ids),
            ("ppid chain", ResolvedProcess::ppid_ids),
        ];
        // `--all-processes` は CoreAudio を経由しないので、CoreAudio 由来の列は測れない
        // （全行 None になり「private も None だった行」を数えるだけの無意味な数字になる）。
        // 置き換え想定の集合もこのとき proc_pidpath と恒等になるため、同じ行を 2 本出さない。
        if !all_processes {
            methods.push(("CoreAudio bundle id", ResolvedProcess::core_audio_ids));
            methods.push(("direct+path+coreaudio", ResolvedProcess::planned_ids));
        }

        for (name, ids) in methods {
            let overall = compare_sets(&all, ids);
            let helper = compare_sets(&parent_resolved, ids);
            emit!(
                "    {name:<19} {}/{} {} {} | {}/{} {} {}",
                overall.equal,
                overall.compared,
                overall.missing,
                overall.extra,
                helper.equal,
                helper.compared,
                helper.missing,
                helper.extra
            );
        }
        if all_processes {
            emit!(
                "    (CoreAudio bundle id needs the CoreAudio population; without it the planned set equals proc_pidpath)"
            );
        }

        // 取りこぼし（private にあって公開 API に無い）は代替可否に直結するので、行ごとに見せる。
        let dropped: Vec<&&ResolvedProcess> = all
            .iter()
            .filter(|row| !row.private_ids().is_subset(&row.planned_ids()))
            .collect();
        if dropped.is_empty() {
            emit!("  no row lost an id when switching to the planned public-API set.");
        } else {
            emit!("  rows where the planned public-API set lost an id the private API had:");
            for row in dropped {
                emit!(
                    "    pid {} private={} planned={} exec={}",
                    row.pid,
                    show_set(&row.private_ids()),
                    show_set(&row.planned_ids()),
                    show_path(row)
                );
            }
        }
    }

    fn report_all_rows(rows: &[ResolvedProcess]) {
        emit!("  all rows (pid / direct / private / path / ppid / coreaudio / exec):");
        for row in rows {
            emit!(
                "    {} | {} | {} | {} | {} | {} | {}",
                row.pid,
                show(&row.direct),
                show(&row.responsible),
                show(&row.by_path),
                show(&row.by_ppid),
                show(&row.core_audio_bundle_id),
                show_path(row)
            );
        }
    }

    /// 主要アプリ（Chrome / Zoom / Slack など）ごとに、ヘルパーを何件・どのバンドル ID に畳めたかを出す。
    fn report_per_app(rows: &[ResolvedProcess]) {
        let mut by_app: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();
        for row in rows {
            for app in row.private_ids().union(&row.planned_ids()) {
                let entry = by_app.entry(app.clone()).or_insert((0, 0, 0));
                entry.0 += 1;
                if row.parent_added_id() {
                    entry.1 += 1;
                    // 完全一致ではなく「取りこぼしが無い」で数える。公開 API 側はヘルパー自身の
                    // ID（`….helper`）を余分に持つことがあり、完全一致で見ると常に 0 になる。
                    if row.private_ids().is_subset(&row.planned_ids()) {
                        entry.2 += 1;
                    }
                }
            }
        }
        emit!("  per app (rows / parent-resolved rows / parent-resolved rows that lost no id):");
        for (app, (total, parent_resolved, kept)) in by_app {
            emit!("    {app}: {total} / {parent_resolved} / {kept}");
        }
    }

    fn show_set(ids: &BTreeSet<String>) -> String {
        if ids.is_empty() {
            "-".to_owned()
        } else {
            ids.iter().cloned().collect::<Vec<_>>().join("+")
        }
    }

    fn show_path(row: &ResolvedProcess) -> String {
        row.exec_path
            .as_deref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "<unreadable>".to_owned())
    }

    fn show(value: &Option<String>) -> String {
        value.clone().unwrap_or_else(|| "-".to_owned())
    }

    fn resolve(entry: ProcessEntry) -> ResolvedProcess {
        let pid = entry.pid;
        let exec_path = proc_path(pid);
        ResolvedProcess {
            pid,
            core_audio_bundle_id: entry.core_audio_bundle_id,
            direct: bundle_id_for_pid(pid),
            responsible: responsible_pid(pid).and_then(bundle_id_for_pid),
            by_path: exec_path.as_deref().and_then(bundle_id_for_exec_path),
            by_ppid: responsible_pid_via_parents(pid),
            exec_path,
            running_input: entry.running_input,
        }
    }

    // ---------------------------------------------------- CoreAudio 側の照会

    /// CoreAudio のプロセスオブジェクト一覧を `(pid, マイク入力中か, CoreAudio が持つバンドル ID)` で
    /// 返す。macOS 14.4 未満やサンドボックスで照会できない場合は `None`
    /// （＝自動録音そのものが成立しない）。
    fn audio_processes() -> Option<Vec<ProcessEntry>> {
        let processes = process_object_list()?;
        let total = processes.len();
        let mut samples = Vec::new();
        for process in processes {
            // pid が読めない行は母集団から落ちる。測定値なので、黙って縮めず件数を知らせる。
            let Some(pid) = process_pid(process) else {
                continue;
            };
            samples.push(ProcessEntry {
                pid,
                running_input: process_is_running_input(process),
                core_audio_bundle_id: process_bundle_id(process),
            });
        }
        if samples.len() != total {
            emit!(
                "  note: {} of {total} audio process objects did not return a pid",
                total - samples.len()
            );
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
            emit!("  AudioObjectGetPropertyDataSize failed (OSStatus={status})");
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
            emit!("  AudioObjectGetPropertyData failed (OSStatus={status})");
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

        /// 失敗 status を 1 回だけ知らせるためのフラグ。
        static BUNDLE_ID_STATUS_REPORTED: AtomicBool = AtomicBool::new(false);

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
            // 「未対応（macOS 14 未満）」なのか「サンドボックスで拒否」なのかで結論が変わるので、
            // status を捨てない。毎行だと騒がしいため最初の 1 件だけ知らせる。
            if !BUNDLE_ID_STATUS_REPORTED.swap(true, Ordering::Relaxed) {
                emit!("  kAudioProcessPropertyBundleID query failed (OSStatus={status})");
            }
            return None;
        }
        let ptr = NonNull::new(value)?;
        // SAFETY: CoreAudio は CFStringRef を +1 で返す（AudioHardware.h の
        // kAudioProcessPropertyBundleID: "The caller is responsible for releasing the returned
        // CFObject."）。CFString と NSString は toll-free bridge なので、その +1 をそのまま
        // Retained に渡して解放を任せる。
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
            // RTLD_DEFAULT(-2): 現在のプロセスにロード済みの全イメージからシンボルを探す。
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

    /// 公開 API 案 2: 親 PID を辿り、最初にバンドル ID を持つ祖先の**バンドル ID**を返す。
    fn responsible_pid_via_parents(pid: i32) -> Option<String> {
        /// zygote 構成のようにヘルパーの親がさらにヘルパーである場合に備えて多段で辿る上限。
        const MAX_DEPTH: usize = 8;
        /// 深さ上限で打ち切った件数を 1 回だけ知らせるためのフラグ（上限が足りているかの判断材料）。
        static DEPTH_EXCEEDED_REPORTED: AtomicBool = AtomicBool::new(false);

        let mut current = pid;
        for _ in 0..MAX_DEPTH {
            let parent = parent_pid(current)?;
            // launchd（1）まで来たら、それ以上辿ってもアプリには行き着かない。
            if parent <= 1 {
                return None;
            }
            if let Some(bundle_id) = bundle_id_for_pid(parent) {
                return Some(bundle_id);
            }
            current = parent;
        }
        if !DEPTH_EXCEEDED_REPORTED.swap(true, Ordering::Relaxed) {
            emit!(
                "  note: the parent chain hit the depth limit ({MAX_DEPTH}) for at least one pid"
            );
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
                emit!("  SCShareableContent::get failed: {err}");
                emit!("  → screen-recording permission (TCC) or the sandbox is blocking it.");
                return;
            }
        };
        let displays = content.displays();
        emit!("  shareable displays: {}", displays.len());
        let Some(display) = displays.first() else {
            emit!("  no display found; cannot start a capture");
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
            emit!("  start_capture failed: {err}");
            return;
        }
        emit!("  capture started; waiting up to {AUDIO_WAIT:?} for audio buffers…");
        let deadline = Instant::now() + AUDIO_WAIT;
        while Instant::now() < deadline && samples.load(Ordering::Relaxed) == 0 {
            std::thread::sleep(Duration::from_millis(100));
        }
        let received = samples.load(Ordering::Relaxed);
        if let Err(err) = stream.stop_capture() {
            emit!("  stop_capture failed: {err}");
        }
        emit!("  audio sample buffers received: {received}");
        if received == 0 {
            emit!("  → the capture ran but nothing was playing; play audio and rerun to confirm.");
        }
    }

    // ------------------------------------------- security-scoped bookmark

    /// フォルダ選択（`rfd` = NSOpenPanel）→ security-scoped bookmark を作って保存する。
    /// サンドボックスでは、パネルで選んだ URL からしか bookmark を作れないためこの順になる。
    fn report_bookmark_save(bookmark_file: &Path, preset_folder: Option<&Path>) {
        section("Security-scoped bookmark (save)");
        emit!("  bookmark file: {}", bookmark_file.display());
        // `--folder` を渡すとパネルを出さずにそのパスを使う。サンドボックス内のコンテナに対して
        // bookmark の作成・解決の往復だけを（人手を介さず）確かめるための逃げ道で、
        // 「パネルで選んだコンテナ外のフォルダに書けるか」は `--folder` 無しでしか検証できない。
        let folder = match preset_folder {
            Some(folder) => folder.to_path_buf(),
            None => {
                let Some(folder) = rfd::FileDialog::new().pick_folder() else {
                    emit!("  no folder was selected");
                    return;
                };
                folder
            }
        };
        emit!("  selected: {}", folder.display());

        let Some(folder_str) = folder.to_str() else {
            emit!("  the selected path is not valid UTF-8");
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
                emit!("  bookmarkDataWithOptions failed: {err}");
                return;
            }
        };
        let bytes = data.to_vec();
        // bookmark はユーザーが選んだフォルダへのアクセス権そのものなので 0600 で置く
        // （`docs/rules/security.md`）。
        let write_result = private_file(bookmark_file).and_then(|mut file| {
            use std::io::Write;
            file.write_all(&bytes)
        });
        if let Err(err) = write_result {
            emit!("  Could not write {}: {err}", bookmark_file.display());
            return;
        }
        emit!(
            "  saved {} bytes to {}",
            bytes.len(),
            bookmark_file.display()
        );
        emit!("  → rerun with --resolve-bookmark to check access from a fresh launch.");
    }

    /// 保存済み bookmark を解決し、アクセス権を開いて実際に書けるかまで確かめる。
    /// 「再起動後もアクセスできるか」の検証なので、保存とは別プロセスで走らせる。
    fn report_bookmark_resolve(bookmark_file: &Path) {
        section("Security-scoped bookmark (resolve)");
        emit!("  bookmark file: {}", bookmark_file.display());
        let bytes = match std::fs::read(bookmark_file) {
            Ok(bytes) => bytes,
            Err(err) => {
                emit!("  could not read {}: {err}", bookmark_file.display());
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
                emit!("  URLByResolvingBookmarkData failed: {err}");
                return;
            }
        };
        emit!(
            "  resolved: {}",
            url.path().map(|p| p.to_string()).unwrap_or_default()
        );
        emit!("  stale: {}", stale.as_bool());

        // SAFETY: 解決済みの security-scoped URL に対する呼び出し。stop と対で使う。
        let started = unsafe { url.startAccessingSecurityScopedResource() };
        emit!("  startAccessingSecurityScopedResource: {started}");

        let write_result = url.path().map(|path| {
            let probe = PathBuf::from(path.to_string()).join("openshoki-mas-probe.txt");
            let result = std::fs::write(&probe, b"openshoki MAS probe\n");
            if result.is_ok() {
                let _ = std::fs::remove_file(&probe);
            }
            result
        });
        match write_result {
            Some(Ok(())) => emit!("  write test: ok"),
            Some(Err(err)) => emit!("  write test failed: {err}"),
            None => emit!("  write test skipped (no path on the resolved URL)"),
        }

        if started {
            // SAFETY: start が成功したときだけ対で呼ぶ。
            unsafe { url.stopAccessingSecurityScopedResource() };
        }
    }

    /// 全プロセスの pid を列挙する（`--all-processes` 用）。CoreAudio 由来ではないので、
    /// マイク入力の有無と CoreAudio のバンドル ID は分からず `None`。
    fn all_processes_sample() -> Vec<ProcessEntry> {
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

        // まず必要バイト数を問い合わせる（buffer=null）。固定長で読むと、足りなかったときに
        // proc_listpids はエラーを返さず埋まるだけなので、母集団が黙って縮む。
        // SAFETY: buffer が null・buffersize が 0 のときは必要バイト数を返す仕様。
        let needed = unsafe { proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
        if needed <= 0 {
            emit!("  proc_listpids could not report the required size (returned {needed})");
            return Vec::new();
        }
        // 問い合わせと実取得の間にプロセスが増えても切り捨てないよう、少し余裕を持たせる。
        let capacity = needed as usize / size_of::<i32>() + 64;
        let mut pids = vec![0i32; capacity];
        let size = (pids.len() * size_of::<i32>()) as c_int;
        // SAFETY: buffer は size バイトの有効な書き込み先。戻り値は書き込まれたバイト数。
        let written =
            unsafe { proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast::<c_void>(), size) };
        if written <= 0 {
            emit!("  proc_listpids failed (returned {written})");
            return Vec::new();
        }
        if written == size {
            // 埋まりきった＝入り切らなかった可能性がある。黙って縮めると一致率が歪むので知らせる。
            emit!("  warning: the pid list may be truncated ({capacity} slots were all used)");
        }
        pids.truncate(written as usize / size_of::<i32>());
        pids.into_iter()
            .filter(|&pid| pid > 0)
            .map(|pid| ProcessEntry {
                pid,
                running_input: None,
                core_audio_bundle_id: None,
            })
            .collect()
    }
}
