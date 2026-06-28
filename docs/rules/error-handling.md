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

## 永続化に成功してから in-memory / UI を更新する

保存（ファイル書き込み・設定の永続化など）を伴う更新では、**永続化が成功してから**
メモリ上の状態や画面表示を更新する。先に更新して保存失敗をログするだけだと、「画面・メモリは
新しい値なのにディスクは古い」という不整合になり、次回起動で戻ってユーザーを混乱させる。

```rust
// NG: 先に反映してから保存（失敗すると表示とディスクが食い違う）
state.value = new_value;
if let Err(err) = state.save() { eprintln!("保存失敗: {err}"); }
ui.set_value(new_value);

// OK: 候補を保存し、成功したときだけ反映する
let mut candidate = state.clone();
candidate.value = new_value;
if let Err(err) = candidate.save() {
    eprintln!("保存に失敗したため変更しない: {err}");
    return;
}
ui.set_value(candidate.value);
state = candidate;
```

## expect / unwrap は初期化時の不変条件だけ

- 本番経路の `unwrap()` / `expect()` / `panic!()` は避ける。
- 初期化時など「ここでは必ず成功する」と言える箇所に限り `expect("理由")` を使い、
  **なぜ成功すると言えるのか**を理由に書く。

```rust
// main は常にメインスレッドで動くため成功する、という不変条件を理由に書く
let mtm = MainThreadMarker::new().expect("main は常にメインスレッドで動くため成功する");
```
