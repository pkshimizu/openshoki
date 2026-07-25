fn main() {
    // UI 操作テスト（`tests/ui_seekbar.rs`）が使う `ElementHandle` は、生成コードに要素名などの
    // デバッグ情報が埋まっていることを要求する。素の `cargo test` で通したいので既定で有効にし、
    // 出荷する release ビルドだけ落とす（`PROFILE` は release を継承するプロファイル
    // ——`--release` / `bench` / `inherits = "release"`——で "release" になる）。それらでも
    // テストを走らせたいときは `SLINT_EMIT_DEBUG_INFO=1` で明示的に有効化できる
    // （slint 本来の切り替え口を殺さない）。
    //
    // 代償: dev ビルド（bin・examples・tests）の生成コードが数倍になる（実測 約 1 万行 → 約 2.7
    // 万行）。重くなりすぎたら「テストのときだけ環境変数で有効化」へ寄せる余地がある。
    // `with_debug_info` は `#[doc(hidden)]`（semver 保証の外）なので、将来シグネチャが変わったら
    // 上記の環境変数経路へ退避する。
    let emit_debug_info = std::env::var("PROFILE").as_deref() != Ok("release")
        || std::env::var_os("SLINT_EMIT_DEBUG_INFO").is_some();
    let config = slint_build::CompilerConfiguration::new().with_debug_info(emit_debug_info);
    slint_build::compile_with_config("ui/app-window.slint", config)
        .expect("Slint UI のコンパイルに失敗した");
    // デバッグ情報の有無をテスト側へ伝える（無い状態で ElementHandle を使うと要素が見つからず、
    // 原因の分からない失敗になるため、テスト自体を切り替える）。
    println!("cargo::rustc-check-cfg=cfg(slint_debug_info)");
    if emit_debug_info {
        println!("cargo::rustc-cfg=slint_debug_info");
    }

    // screencapturekit は内部で Swift ブリッジを使うため、生成バイナリが Swift ランタイム
    // （`libswift_Concurrency.dylib` など）を必要とする。これらは macOS の dyld 共有キャッシュ
    // 上の `/usr/lib/swift` から解決されるが、`@rpath` 参照のため rpath が通っていないと
    // 起動時に `Library not loaded` で落ちる。rpath を明示して解決させる（本体・examples 共通）。
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
