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

## トレイ常駐アプリは `run_event_loop_until_quit()` を使う

`slint::run_event_loop()` は「最後のウィンドウが閉じられ、かつ最後の **Slint の**
`SystemTrayIcon` が隠れた」時点で return する。`tray-icon` クレートなど **Slint 製でない**
トレイは Slint から見えないため、ウィンドウを隠した（`hide()` / `on_close_requested` →
`HideWindow`）瞬間に「表示物ゼロ」と判定され、ループが終了してプロセスが落ちる。

対策: 常駐させたいなら `slint::run_event_loop_until_quit()` を使う。これは
`quit_on_last_window_closed(false)` 相当で、表示物が無くても回り続け、終了は
`slint::quit_event_loop()`（＝「終了」メニュー）だけがトリガーになる。

- 検証は「閉じる→非表示でプロセスが生きているか」を見る。クリック座標は環境依存で不安定
  なので、`window.hide()` をタイマーから呼んで pid の生死を見るのが確実
  （`run_event_loop` 版は hide 直後に DEAD、`until_quit` 版は ALIVE になる）。
