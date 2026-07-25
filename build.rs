fn main() {
    // UI 操作テスト（`tests/ui_seekbar.rs`）が使う `ElementHandle` は、生成コードに要素名などの
    // デバッグ情報が埋まっていることを要求する。素の `cargo test` で通したいので dev/test
    // プロファイルでは有効にし、出荷する release ビルドには入れない（cargo が build script へ
    // 渡す `PROFILE` は release ビルドのみ "release"）。
    let emit_debug_info = std::env::var("PROFILE").as_deref() == Ok("debug");
    let config = slint_build::CompilerConfiguration::new().with_debug_info(emit_debug_info);
    slint_build::compile_with_config("ui/app-window.slint", config)
        .expect("Slint UI のコンパイルに失敗した");

    // screencapturekit は内部で Swift ブリッジを使うため、生成バイナリが Swift ランタイム
    // （`libswift_Concurrency.dylib` など）を必要とする。これらは macOS の dyld 共有キャッシュ
    // 上の `/usr/lib/swift` から解決されるが、`@rpath` 参照のため rpath が通っていないと
    // 起動時に `Library not loaded` で落ちる。rpath を明示して解決させる（本体・examples 共通）。
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
