fn main() {
    slint_build::compile("ui/app-window.slint").expect("Slint UI のコンパイルに失敗した");

    // screencapturekit は内部で Swift ブリッジを使うため、生成バイナリが Swift ランタイム
    // （`libswift_Concurrency.dylib` など）を必要とする。これらは macOS の dyld 共有キャッシュ
    // 上の `/usr/lib/swift` から解決されるが、`@rpath` 参照のため rpath が通っていないと
    // 起動時に `Library not loaded` で落ちる。rpath を明示して解決させる（本体・examples 共通）。
    #[cfg(target_os = "macos")]
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
