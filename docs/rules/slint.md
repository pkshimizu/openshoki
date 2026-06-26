# 実装ルール: Slint

## イベントループ稼働中に初めて show() するウィンドウは、初回にジオメトリを明示する

トレイメニューなどから、イベントループが回り始めた**後**に Slint ウィンドウを初めて
`show()` すると、ウィンドウのジオメトリが確定されず**高さ 0**（タイトルバーだけ）で
表示されることがある（macOS の Accessory アプリで確認）。`.slint` の `preferred-*` /
`min-*` だけでは防げない。

対策: 初回表示時に `set_position` と `set_size` を明示してから `show()` する。
特に `set_position` がジオメトリ確定の引き金になる（`set_size` だけでは直らない）。

```rust
if !geometry_committed {
    window.set_position(slint::LogicalPosition::new(WINDOW_X, WINDOW_Y));
    window.set_size(slint::LogicalSize::new(WINDOW_WIDTH, WINDOW_HEIGHT));
    geometry_committed = true;
}
let _ = window.show();
```

- 毎回呼ぶとウィンドウが動く／ユーザーのリサイズを戻してしまうため、初回だけにする。
- 検証は `examples/` に確認用バイナリを置き、`screencapture` で目視確認するのが速い
  （ループ開始**前**に show すると再現しないので、実アプリと同じ「ループ稼働中の show」を再現すること）。
