# 実装ルール: FFI（OS ネイティブ API 呼び出し）

CoreAudio や AppKit など、`unsafe` な C/Objective-C API を直接叩くときの規約。
`src/mic_monitor.rs`（CoreAudio のプロパティリスナー）や `src/app_audio_monitor.rs`
（オーディオプロセスの照会）などが該当する。レビューで繰り返し出やすい落とし穴をここに集約する。

## 戻り値（OSStatus 等）を必ず検証し、失敗理由を文脈に含める

- FFI 呼び出しの戻り値（`OSStatus` など）は毎回チェックし、未定義状態へ進ませない。
- 失敗を上位へ返す／ログに残すときは、**どの操作が・どの status で失敗したか**を含める。
  同じモジュール内の一部だけ status を落とすと、原因調査時に切り分けができず一貫性も崩れる。

```rust
// NG: 失敗を Option に畳んで status を捨てる（なぜ失敗したか分からない）
fn default_input_device() -> Option<AudioObjectID> { /* status == 0 なら Some */ }

// OK: 失敗理由に status を含めて返す
fn default_input_device() -> Result<AudioObjectID, Box<dyn Error>> {
    // ...
    if status != OS_STATUS_OK {
        return Err(format!("既定入力デバイスの取得に失敗した (OSStatus={status})").into());
    }
    // ...
}
```

- best-effort な読み取り（失敗しても既定値で続けてよい箇所）に限り `Option` で畳んでよい。
  その場合も、名前で「取得を伴う」ことが分かるようにする（`is_`/`has_` は素の `bool` を
  返す慣習なので、`Option<bool>` を返すなら `read_*` などにする）。

## `extern "C-unwind"` コールバックはパニックさせない

OS がコールバックを呼ぶ関数（プロパティリスナー等）はバインディングが `extern "C-unwind"` で
定義していることが多い。ここでパニックすると C フレームへ巻き戻り、**未定義動作**になる。

- コールバック本体は生ポインタの参照化・状態の読み書きなど、パニック経路を持たない処理に限る。
  `unwrap()` / `expect()` / 添字アクセス / パニックしうるマクロを持ち込まない。
- 署名（`extern "C-unwind"` / 戻り値型）はバインディング指定であり `extern "C"` へ変えられない。
  パニックしうる処理を足すなら `std::panic::catch_unwind` で包んで握る。
- 「この関数はパニックしてはならない」旨をコメントに残し、後から手を入れる人に前提を伝える。

## コールバックへ渡すクライアントデータのポインタ寿命を保証する

リスナー登録時に渡す `client_data`（`*mut c_void`）の指す実体は、**リスナーを解除するまで**
生かし続ける。`Arc` などで所有し続け、`Drop` で「リスナー解除 → 実体解放」の順を守る。

- 解除 API（`AudioObjectRemovePropertyListener` 等）が、別スレッドで実行中のコールバックの
  完了まで同期する保証は必ずしも無い。厳密には解除直後の解放に use-after-free の窓が残る。
  常駐アプリの全ライフタイム保持で Drop がプロセス終了時のみなら実害は無いが、その前提と
  残存リスクをコメントに明記する（プロセス寿命を超えて Drop を繰り返す使い方はしない）。

## private API は使わない（Mac App Store が禁じている）

公開ヘッダに無い private シンボルは**使わない**。Mac App Store は private API の使用を禁じており、
審査のリジェクト対象になる。shoki は配布を MAS に寄せる方針（#112 でリポジトリを private に
するため。詳細は `docs/CONTEXT.md` の配布に関する決定記録）で、**ビルドを分岐させない**ので、
手元ビルドでも private API を持たない。

- かつては自動録音の親アプリ解決に responsible pid を返す private シンボルを `dlsym` で
  実行時解決して使っていたが、公開 API へ置き換えた（#107。検証は #77）。
- 公開 API に無い機能が欲しくなったら、まず**その機能を諦めたときの影響**を測る。#77 では
  「Safari の自動録音を捨てる」という判断とセットで置き換えを決めた
  （`docs/plans/done/20260722-mac-app-store-submission.md` の「判断の記録」）。

## マイクを使っているアプリの特定は、複数の公開 API の結果を合わせる

マルチプロセスのアプリ（Chrome・Slack 等の Electron / Chromium 系）は、マイクを掴むのが本体では
なくヘルパープロセスで、経路によって「ヘルパー自身の ID」と「親アプリの ID」のどちらが得られるかが
変わる。どれか 1 つに畳まず、**得られた ID をすべて集合に入れて照合する**
（`app_audio_monitor::input_running_bundle_ids`）。

- `kAudioProcessPropertyBundleID`（CoreAudio、macOS 14+）— オーディオ HAL が持つ値。
- `NSRunningApplication` の直接のバンドル ID — 本体プロセスならこれで取れる。
- `proc_pidpath`（`libproc.h`）で得た実行パスの、**最も外側**の `.app` のバンドル ID。
  `.app` は入れ子になりうる（`Google Chrome.app/…/Google Chrome Helper.app/…`）ので、
  内側を採ると親アプリ登録と一致しない。

**限界も doc に明記する**: フレームワーク同梱の共有 XPC サービスが別アプリの代理で音声を扱う構成
（WebKit の `com.apple.WebKit.GPU.xpc` など）は、実行パスに `.app` を含まず親も `launchd` で、
CoreAudio も XPC サービス自身の ID を返すため、公開 API ではホストアプリへ辿れない。
該当するアプリ（Safari 等）は自動録音の対象外になり、設定画面でその旨を伝える
（`app_audio_monitor::auto_record_limitation`）。

## CFString を返す C API は所有権の契約をヘッダで確かめる

CoreAudio のように `CFStringRef` を返すプロパティは、**呼び出し側が解放する（+1）**契約のことが
多い。ヘッダのコメントで確かめてから、`Retained::from_raw`（toll-free bridge）で解放を任せる。
追加で `retain` すると解放漏れ、`+0` のものを `Retained` に渡すと二重解放になる。

- 根拠にしたヘッダの記述を SAFETY コメントへ引用する（例: `AudioHardware.h` の
  `kAudioProcessPropertyBundleID` は "The caller is responsible for releasing the returned CFObject."）。
- 挙動が環境依存で単体テストしにくい FFI は、最低限「パニックせず戻る」スモークテストを置く。
  所有権を扱う経路は**繰り返し呼ぶ**テストにすると、二重解放がクラッシュとして出る。

## 信頼境界外の文字列を C API へ渡す前に NUL バイトを弾く

設定 TOML など手編集されうる値を FFI ラッパ（whisper-rs 等）へ渡すとき、ラッパが内部で
`CString::new(value).expect(...)` を使っていると **NUL バイト入りの文字列で panic** する。
ワーカースレッド上なら以降その機能が黙って死ぬ。渡す前に `value.contains('\0')` を弾いて
安全側（スキップ/既定値）へフォールバックし、信頼境界外である旨をコメントに残す。

- 値の妥当性検証（例: 言語コードが既知か）はネイティブ側が行いエラーで返すことが多いので、
  Rust 側では panic 経路（NUL）だけ防げば十分なことが多い。ラッパの実装を確認して判断する。

## 生ポインタを受け取って参照化する関数は unsafe fn にする

raw-window-handle の `RawWindowHandle` のように、**安全なコードだけで構築できる型**に生ポインタが
入っている場合、それを受けて内部で `as_ref()` する関数を safe fn にしてはならない。呼び出し側が
`unsafe` なしでダングリングポインタ入りの値を渡せてしまい、「safe fn はどんな入力でも UB を
起こさない」という健全性規則に違反する（unsound な safe fn）。

- 満たせない前提（ポインタの生存など）があるなら `unsafe fn` + doc の `# Safety` で呼び出し側に
  契約を課し、呼び出し側は `// SAFETY: ...` でその契約の履行根拠（ポインタの出所・生存期間）を
  逐一書く。
- `# Safety` には**呼び出し側の義務だけ**を書く。関数内部で確認して縮退する条件（メインスレッド
  判定等）を混ぜると、「その保証は誰の責務か」が読み手に伝わらず、呼び出し側 SAFETY コメントとの
  食い違いになる。内部で確認する旨は「呼び出し側の前提ではない」と明記してよい。
- ポインタを参照しない縮退経路（Err・別バリアント）は「前提を自明に満たす」とテスト側の SAFETY
  コメントに書けば、`unsafe { ... }` で包んだ決定的なスモークテストがそのまま書ける。

## whisper-rs 0.16.0 の `set_abort_callback_safe` は使わない

`FullParams::set_abort_callback_safe` は、閉包を `Box<Box<dyn FnMut() -> bool>>` として確保
しながら、トランポリンを閉包の具体型で単相化する（`trampoline::<F>`）。C 側から返ってくる
ポインタが指すのは外側の `Box` なので、`*mut F` として読むと別の型を読むことになる。閉包が
何かを捕まえていれば未定義動作。

進捗側（`set_progress_callback_safe`）は `trampoline::<Box<dyn FnMut(i32)>>` と正しく書かれて
いて、abort 側だけが取り違えている。**「safe と付いているから安全」を確かめずに信じない。**

中断が要るときは unsafe な素の口（`set_abort_callback` / `set_abort_callback_user_data`）へ
自前のトランポリンを渡す（`transcribe::abort_if_stopped`）。渡すのは `AtomicBool` のアドレス
だけにして、`full()` が返るまで `Arc` で生かす。クレートを上げるときに直っているか確かめる。
