# 実装ルール: エラーハンドリング

レビューで繰り返し出やすい、エラーの扱いに関する規約。

## Result を握りつぶさない

- `let _ = result;` で `Result` を捨てない。
- `?` で伝播できる箇所は伝播する。
- `?` で伝播できない箇所（`'static` クロージャ、イベントループのコールバック等）では、
  最低限 `eprintln!` などで失敗をログに残す。黙って捨てない。

```rust
// NG
let _ = window.hide();

// OK
if let Err(err) = window.hide() {
    eprintln!("ウィンドウの非表示に失敗した: {err}");
}
```

## expect / unwrap は初期化時の不変条件だけ

- 本番経路の `unwrap()` / `expect()` / `panic!()` は避ける。
- 初期化時など「ここでは必ず成功する」と言える箇所に限り `expect("理由")` を使い、
  **なぜ成功すると言えるのか**を理由に書く。

```rust
// main は常にメインスレッドで動くため成功する、という不変条件を理由に書く
let mtm = MainThreadMarker::new().expect("main は常にメインスレッドで動くため成功する");
```
