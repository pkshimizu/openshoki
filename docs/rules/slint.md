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

## 入力ウィジェットとウィンドウのプロパティは `<=>` で双方向に束ねる

`CheckBox { checked: root.value; }` のような**片方向バインディング**にすると、std-widgets が
ユーザー操作時に自分のプロパティへ**命令的に代入**する（`root.checked = !root.checked` /
`ComboBoxBase::select()` の `root.current-index = index`）ため、その時点でバインディングが外れる。
以後 Rust から `set_value()` してもウィジェットの表示は追従せず、**下の「保存失敗時に旧値へ戻す」が
実際には効かない**（表示は失敗した操作のまま、メモリ・ディスクは旧値。再起動まで自己修復しない）。

対策: ウィジェット側のプロパティは `<=>` でウィンドウの `in-out property` に束ねる
（`checked <=> root.value;` / `current-index <=> root.index;` / `value <=> root.secs;`）。
束ねたらハンドラ内の `root.value = self.checked;` は不要（二重更新になるので消す）。

- 対象は `CheckBox` / `SpinBox` / `ComboBox` など**ユーザーが値を変える**ウィジェット全部。
  1 つだけ直すと、その項目だけ巻き戻しが効く／効かないの非対称になる。
- 検証は `tests/ui_settings_rollback.rs`（テストバックエンド）。「ユーザー操作 → Rust が書き戻す →
  表示が戻る」を見る。片方向へ戻すと落ちることを確認済み。
- `ComboBox` の表示テキスト（`current-value`）は `changed current-index` ハンドラ経由で更新される。
  これは**イベントループの次の回**で走るので、テストでは `mock_elapsed_time`（`Duration::ZERO` で
  よい。経過量に関係なく changed ハンドラを流すために呼ぶ）を挟んでから `accessible_value()` を
  見る（set 直後は旧値のまま）。

## in-out プロパティの操作は、保存失敗時に表示を旧値へ戻す

`CheckBox` / `SpinBox` など `in-out property` に結ぶ入力ウィジェットは、慣例として
**Slint 側で先にプロパティを新値へ更新してから** Rust のコールバックを呼ぶ
（`toggled => { root.value = self.checked; root.on_change(...) }`）。このため、コールバックで
永続化に失敗して早期 return するだけだと、**表示は新値・メモリ/ディスクは旧値**という食い違いが
残る（[[error-handling.md]] の「永続化に成功してから更新する」の Slint 版の落とし穴）。

対策: 保存失敗の分岐で、保存済みの値を `set_<prop>()` で書き戻して表示を巻き戻す。
プログラムからの `set_` はユーザー操作の `edited`/`toggled` を発火しないのでループしない。
**この書き戻しが表示へ届くのは、上のとおりウィジェットを `<=>` で束ねている場合だけ**。

```rust
if let Err(err) = candidate.save() {
    eprintln!("Not changing ... because saving the settings failed: {err}");
    ui.set_value(state.borrow().value); // 表示を保存済みの値へ戻す
    return;
}
```

- 同種の in-out 入力が複数あるなら**全部**同じ巻き戻しを持たせる。1 つだけ対応して他を
  放置すると、その項目だけ表示不整合を抱えたまま非対称になる。

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

## `.slint` の宣言はグルーピングを保つ

`AppWindow` の宣言は「プロパティ群 → コールバック群」でまとめている。項目を追加するときは
既存の並びに割り込ませ、`property → callback → property → callback` の混在にしない
（機能ペアで末尾へ追記すると崩れやすい）。

## 繰り返すタイポ・配色値は `global` にまとめ、直書きで散らさない

フォントサイズ・ウェイト・淡色（`transparentize`/`opacity`）など**意味を持って複数箇所に現れる値**は、
`.slint` 内の `global`（例 `global Style { out property <length> body-size: 13px; ... }`）に集約し、
各所からそれを参照する。直書きを散らすと、トーン調整時に追従漏れ・不整合（見出しと本文のサイズ逆転など）が
起きやすい。

- 対象: タイトル/見出し/本文/補助のフォントサイズ、semibold 等のウェイト、muted 用のアルファ。
- 対象外（直書きでよい）: 特定箇所の一回きりの余白・インデント（`padding-top: 6px`・左 23px 等）。
  過剰な定数化は逆に読みにくい。

## 従属コントロール群の無効化は単一のゲートに一本化する

あるトグルに従属するコントロール群（例: 自動録音 OFF で無効化する delay・一覧・追加/削除ボタン）は、
Slint に「コンテナからの `enabled` カスケード」が無いため各ウィジェットに `enabled` を付けざるを得ない。
このとき条件式（`root.auto-record-app` 等）を各所へコピーせず、**囲む要素に 1 つの真偽プロパティ**
（例 `deps := Rectangle { property <bool> enabled-gate: root.auto-record-app; }`）を持たせ、
`opacity` と各 `enabled` はそれを参照する。淡色化（opacity）と操作不可（enabled）の条件が食い違うのを防ぐ。

### 複数ウィンドウで使うトークンは共有 `.slint` に置く

`global Style` などのトークンを複数のウィンドウ（`.slint` ファイル）で使うなら、共有ファイル
（例 `ui/style.slint`）に `export global` で置き、各ファイルから `import` する。1 つのウィンドウ
ファイルに定義して他方が直書きに戻ると、片方だけ不整合になる（トークン集約の意味が消える）。

## 件数が増えうる一覧は ListView で可視範囲だけ生成する

`ScrollView` + `for` はモデルの**全行を即時生成**する（Slint の `for` は仮想化されない）。
文字起こしセグメントのように数百〜数千件になりうる一覧でこれをやると、モデル更新時
（セッション選択時など）に全行の要素生成と word-wrap レイアウトが UI スレッドで一度に走り、
ヒッチとメモリ増につながる。件数が増えうる一覧は `std-widgets` の `ListView` を使い、
可視範囲の行だけ遅延生成させる。

- `ListView` の行は自身の高さを明示する（例: `height: row-content.preferred-height;`）。
  word-wrap する本文を含む行では、内側レイアウトに名前を付けてその `preferred-height` に
  結ぶと実高さに追従する。
- 固定数行の小さな一覧（設定画面の数項目など）は `ScrollView` + `for` のままでよい。

## `alignment` を指定したレイアウトの中では `*-stretch` が効かない

`VerticalLayout` / `HorizontalLayout` に `alignment: start` 等を指定すると、子要素はすべて
**推奨サイズ**で配置され、子の `vertical-stretch` / `horizontal-stretch` は無視される。
中身が可変の要素（ListView を含む箱など。推奨高さがほぼ 0）は見出しだけに潰れ、
「実装したのに何も表示されない」symptom になる。

- 「上詰めで、余りは特定の子へ」という配置は、`alignment` を既定（stretch）のままにして、
  固定したい子へ `vertical-stretch: 0`、伸ばしたい子へ `vertical-stretch: 1` を付けて表現する。
- レイアウト崩れの検証は `examples/` の確認用バイナリ（`examples/transcript_view.rs` /
  `examples/settings_view.rs`）で目視する（ビルドが通ってもレイアウトの潰れは検出できない。
  テストバックエンドでも検出できない: `take_snapshot` が未実装で、そもそも描画しない）。
- 画面収録（TCC）の許可が無い環境では `screencapture` が使えない。確認用バイナリに
  「イベントループ開始後に `Window::take_snapshot()` して PNG を書く」経路を持たせると、
  許可なしで目視できる（`examples/settings_view.rs -- snapshot out.png`）。
  ループ開始**前**に撮ると中身が空になるので、`Timer` で 1 フレーム後に撮ること。

## Rust ⇄ Slint で受け渡す状態は int でなく `export enum` を使う

Slint のモデル・プロパティで状態（例: 文字起こしの 前/中/完了/失敗）を渡すとき、`int` に
数値を割り当てる方式（0=前, 1=中, …）にしない。対応表がコメント頼みで Rust / Slint の
双方に散らばり、`== 1` のようなマジックナンバー比較はタイポしても検出されず、値の追加時に
網羅性チェックも効かない。

- `.slint` 側で `export enum`（例: `TranscriptStatus`）を定義し、struct のフィールドや
  プロパティの型に使う。Rust 側には同名の enum が生成され、`match` の網羅チェック・
  `==` 比較がそのまま使える（`i32` への変換コード ・対応表コメントが不要になる）。
- 既定値は最初のバリアントになるため、「未設定」に相当する状態を先頭に置く。

## enum を渡したら、そこから導出できる bool を別プロパティで渡さない

Rust ⇄ Slint で状態 enum（例: `TranscriptStatus`）を渡すようになったら、その enum から
導出できる真偽値（例: 「実行中か」）を **別の `in property <bool>` として並存させない**。
同じ事実の二重表現になり、「enum は failed なのに bool は true」のようなありえない組み合わせを
作れてしまう。整合性が「必ず同じ関数経由で set する」という運用頼みになる。

- 導出値は Slint 内の private プロパティにする
  （`property <bool> transcribing: root.status == TranscriptStatus.transcribing;`）。
  `in property` でなくなれば Rust 側に setter が生成されず、片方だけ更新する事故が
  型レベルで塞がれる。

## 状態→UI 文言の対応表は Rust の網羅 match に置き、Slint の三項連鎖にしない

状態 enum に対応する表示文言（状態テキスト・縮退表示ラベル等）を Slint 側のネストした
三項演算子で分岐させない。Rust 側の `match`（ワイルドカードなし）による純粋関数に集約し、
`in property <string>` で渡して Slint は表示するだけにする。

- Rust の網羅 match ならバリアント追加時にコンパイルエラーで両対応表の更新漏れを検出できる。
  Slint の三項連鎖は最後の分岐へ暗黙にフォールスルーし、追加漏れが静かに誤表示になる。
- 純粋関数には全状態を検証する単体テストを置く（`transcript_status_text_covers_all_states` が
  前例）。文言はリテラルで検証し、定数を再利用したトートロジーにしない。
- 複数の対応表で同じ文言を使う状態があるなら、そのラベルだけ `const` で共有して食い違いを防ぐ
  （例: `TRANSCRIBING_LABEL`）。

## ドラッグ操作を作るときの TouchArea の約束

`TouchArea` でスライダー・シークバーのようなドラッグ操作を作るときは、次を必ず押さえる
（`SeekBar` が実例）。取りこぼすと「右クリックで値が飛ぶ」「ウィンドウ外で離すと掴んだまま
固まる」といった不具合になり、レビューで繰り返し指摘される。

- `pointer-event` は**どのボタンでも来る**。操作の開始（`down`）・確定（`up`）は
  `event.button == PointerEventButton.left` で絞る（`clicked` / `pressed` が左限定なのと違う）。
- `cancel`（ポインタがウィンドウ外へ出た、`enabled` が false になった等）を必ず処理し、
  ドラッグ状態を畳む。確定はせず表示だけ元へ戻すのが基本。
- `moved` は**押下でグラブしている間だけ**発火する（ホバーでは来ない）。それでも
  「自分のドラッグ中か」のフラグで囲む（左以外のボタンのドラッグで表示だけが動くのを防ぐ）。
- ドラッグ中の座標はバー外へも出る。比率へ換算する値は `min(max(...))` で 0.0〜1.0 に丸める
  （幅 0 で NaN になる経路もあるため、Rust 側でも受け取った値を丸め直す）。
- 細いバーは当たり判定を上下に広げる（見た目 5px でも `TouchArea` は 20px 程度）。

## Rust が読むだけの UI 状態は `out property`、部品とは `<=>` で束ねる

ドラッグ中フラグのように **UI が所有し Rust は読むだけ**の状態は、ウィンドウ側で
`out property` にする（Rust に setter が生成されず、片方だけ更新する事故が型で塞がれる）。
その状態を部品へ切り出すときは次の制約に当たるので、束ね方を間違えないこと。

- `if` ブロックの内側で宣言した id は外から参照できない（`out property <bool> x: bar.x;` は
  `Cannot access id 'bar'` でコンパイルエラー）。詳細ペインのように条件付きで生成される
  サブツリーの中の部品は、この方法でエイリアスできない。
- 代わりに**使用箇所で双方向束縛**する（`SeekBar { scrubbing <=> root.scrubbing; }`）。
  部品側のプロパティは `in-out` にする必要がある（`out` は双方向束縛の対象にできない）。
  ウィンドウ側が `out` なら Rust から見た読み取り専用は保てる。
- 双方向束縛は 1 つのプロパティに束ねるため、**部品側の初期値は使われない**。
  `in-out property <bool> scrubbing: false;` のような初期化子は誤解を招くので置かない。

## レイアウト内に複雑な対話部品を直書きしない

`TouchArea` と状態プロパティを伴う部品（シークバー等）をウィンドウのレイアウト式の中へ
そのまま書くと、インデントが深くなり、状態が「ウィンドウ / 中間の Rectangle / TouchArea」の
複数スコープに散って読むのに往復が必要になる。`component` へ切り出し、内部状態は部品内の
private プロパティに閉じる（このファイルは `SourceBadge` / `DangerButton` など、より単純な
部品まで切り出す流儀にしている）。

## 「失敗したら表示を更新しない」は、ポーリング tick の上書きまで考える

再生位置などの表示は 100ms の再生 tick が毎回上書きする（`poll_and_update` 相当）。そのため
コールバック側で「失敗したから表示を更新しない」と決めても、**次の tick が別の値で塗り直す**。
抑止したい表示は tick 側の条件も揃えること（ドラッグ中の `scrubbing` ガードが実例）。

- 抑止が 1 コールバック内で完結すると思い込まない。tick が読む値（`position()` 等）が
  外部クレートの内部状態に依存するなら、失敗時にその値が何になるかまで確かめる。
- 逆に、tick が必ず塗り直す表示なら「一時的なズレは次の tick で収束する」と割り切れる。
  どちらなのかを doc コメントに書き残す。

## 表示値の導出は 1 つの関数に集め、**起動時の初期化もその関数を通す**

同じプロパティを入れる箇所は、たいてい 4 つある: **起動時の初期化**・**そのプロパティを変える
コールバック**・**100ms tick の追従**・**別ウィンドウの操作からの追従**。導出のロジック（状態から
文言を組み立てる、条件で文言を出し分ける）を関数に切り出しても、**初期化だけ古い呼び方のまま
残る**事故が起きやすい。初期化は `main` の冒頭にあって、後から足した分岐の差分に現れないため。

- 症状は「起動して画面を開いた瞬間だけ違う表示が出て、次の tick で正しい値に差し替わる」。
  tick が自己修復するぶん**テストでも目視でも気づきにくい**（#123 のレビューで 4 観点が同時に
  指摘したのがこれ）。
- 対策: 導出を関数にしたら、初期化もその関数を呼ぶ。プロパティが複数あるなら、
  **まとめて入れる関数**（`apply_model_selection_to_settings` が実例）を 1 つ作り、初期化と
  追従の両方をそこへ通す。種別や状態行が増えたときに、初期化だけ取り残されなくなる。
- 「片方だけ書き換えて食い違う」を防ぐために文言を定数へ切り出したなら、**その定数を使う側が
  全経路そろっているか**も確かめる。定数化は文言の重複を消すだけで、経路の取り残しは消さない。

## 色は `Style` のトークンから引く。標準ウィジェットは追従しないと知っておく

面・文字・枠・状態色は `ui/style.slint` の `Style` に意味で名付けて集約し、`.slint` の中で
色を直書きしない（片方のテーマだけ調整が漏れる）。ライト/ダークの分岐も `Style.dark` 1 つに
寄せ、各所で `Palette.color-scheme` を見ない。

- **Slint の `Palette` は読み取り専用**で、自前のトークンへ差し替えられない（`Palette.background = …`
  はコンパイルエラー）。したがって std-widgets の `Button` / `CheckBox` / `ComboBox` / `SpinBox` は
  OS の見た目のままになる。**#146 でこの 4 つは自作部品へ置き換えた**（`ui/controls.slint` の
  `ActionButton` / `Toggle` / `Stepper` / `Select`）ので、新しい画面でも std-widgets ではなく
  そちらを使う。std-widgets から使い続けてよいのは `ScrollView` のように配色を持たないものだけ。
- **自作の操作部品には、標準が持っていたものを自分で用意する**: キーボードフォーカス
  （`FocusScope` と可視化）、`accessible-role` / `accessible-label` / `accessible-checked` /
  `accessible-value`、`enabled` が false のときの不発、押下中にポインタが外へ出たときの取り消し。
  **部品の内側の `Text` には `accessible-role: none` を付ける**（付けないと同じ文字列が支援技術へ
  2 回出る。`tests/ui_trigger_apps.rs` が実際に検出した）。
- **`FocusScope` は `TouchArea` より前に宣言する**。ヒットテストは後ろの子から手前として辿るので、
  後ろに置くと `FocusScope` がフォーカス移動のために最初の押下を食べ、**1 回目のクリックが
  空振りする**（2 回目から効くので気づきにくい）。#146 では `SeekBar` と `Stepper` の両方で踏んだ。
- **`clip: true` はフォーカスリングを消す**。輪郭を枠の外側へ出す作りなので、クリップする矩形の
  中に置くと 1px も描かれない。枠・面・クリップは内側の箱に持たせ、リングはその外に立てる
  （`Stepper` が実例）。
- **`enabled` が false でも `accessible-action-*` は素通りする**。Slint は支援技術からの操作を
  `enabled` で遮断しないので、ハンドラの中で自分でガードする。ポインタとキーボードだけ塞いでも、
  淡色表示のまま操作できてしまう。
- **状態を支援技術へ伝えるには、状態プロパティだけでは足りない**。`accessible-checked` は
  `accessible-checkable: true` とセットで、タブの選択は `checked` ではなく
  `accessible-item-selectable` / `accessible-item-selected` で伝わる。**テストバックエンドは
  状態プロパティを直読みするので、テストが通っていても実 AT には届いていないことがある**。
  操作できるものに `progress-indicator` を当てるのも同種の誤りで、フォーカスも操作もできない
  要素として見える（操作できるなら `slider`）。
- **`has-hover` は掴んでいる間、要素の外へ出ても落ちない**（落ちるのはウィンドウから出たとき）。
  押下・ホバーの見た目を `has-hover` だけで決めると、外へドラッグしたまま押下表示が残る。
  座標（`mouse-x` / `mouse-y` が 0〜幅・高さの内か）で判定する。
- **`PopupWindow` の中の要素は外から掴めない**（`Cannot access element … from enclosing component`）。
  開いた先にキー入力を届けるには `forward-focus:` で中の `FocusScope` を指す（`Select` が実例）。
- 状態に対応する色は `enum`（`StatusTone`）で渡し、意味→色の対応表は `Style` の関数 1 箇所に置く。
  Rust 側は文言と一緒に意味を渡す（文言の対応表を Rust の網羅 match に置くのと対）。

## モデル（`ModelRc`）は変わったときだけ差し替える

Slint のプロパティは値で比較されるので、同じ文字列・同じ enum を毎 tick 入れ直しても再描画は
起きない。**`ModelRc` だけが例外**で、比較がポインタなので中身が同じでも必ず dirty になり、
`for` のリピータが要素を全部捨てて作り直す。

- 症状: 押している最中にボタンが消えて `clicked` が発火しない、ホバーやフォーカスが 10Hz で飛ぶ。
  ビルドもテストも通るので、実機で触るまで気づけない（#154 で読む領域のボタンが該当した）。
- 対策: 入れる前に中身を比べ、違うときだけ set する（`main::set_pane_actions` が実例）。
  あるいは同じ `Rc<VecModel>` を持ち続けて行だけ差し替える。

## 操作を置く場所を増やしたら、活性のゲートも一緒に持っていく

同じ操作を別の場所（空表示・コンテキストメニュー等）からも起こせるようにしたとき、**元の
ボタンに掛かっていた `enabled` の条件を写し忘れる**。押す口が増えただけのつもりでも、活性
条件は付いてこない。

- #154 では、詳細ヘッダの Transcribe / Summarize が `detail-jobs-pending` で塞いであるのに、
  読む領域の空表示から同じジョブを重ねて投入できた（ワーカーが書き換え中の JSON を別のジョブが
  読む）。
- 条件を 2 箇所に書かない。ゲートを 1 つの関数・1 つのプロパティに寄せ、**両方がそれを見る**
  （`docs/rules/testing.md` の「ガードは 1 箇所に置く」）。

## 表示のキャッシュは、tick を止める区間で捨てる

「前回の値と比べて、変わったときだけ UI へ流す」差分更新は、**その判定を回している tick が
止まる区間**があると壊れる。止まっている間に別経路が UI を直接書くと、こちらの記憶と UI が
食い違い、再開後に「導出値＝記憶」で一致してスキップし、**古い表示が固定される**。

- ウィンドウが非表示の間 tick を止めるなら、その分岐で記憶を捨てる（#127 の
  `StatusLineCache::forget`）。次に開いたときは必ず流し直す。
- 「UI へ書く口は 1 つだけ」を保てるならそちらが本筋だが、別ウィンドウの操作から書く経路が
  あるとたいてい保てない。**捨てるほうが安い**。
- 差分更新は比較対象を**表示に使う値ぜんぶ**にする。1 つ（文言など）を代表にすると、
  代表が同じまま他（色・進捗）だけ変わる区間で古い値が残る。

## 本文を `ScrollView` に入れたら、UI テストは先にウィンドウを大きくする

`ScrollView` の外にある要素は、テストバックエンドの要素探索（`find_by_element_type_name` 等）で
**見つからない**。既定の大きさのまま探すと、下端のコントロールを触るテストが「要素が無い」で
落ちる（#127 で設定画面をスクロールさせたときに実際に踏んだ）。

- 要素を探す前に、本文がすべて収まる大きさへ `window().set_size(...)` する
  （`tests/ui_support/fit_settings_content` が実例）。スクロール位置に依存させない。
- 見た目の確認（`examples/`）では逆に、画面より本文が長いと 1 枚に収まらない。スクロール位置を
  外から動かせるプロパティを用意して下端も撮る（`AppWindow` の `body-scroll` が実例。
  本番の Rust 側は触らない）。

## `examples/` のスナップショットは「表示した時点の絵」しか撮れない

`Window::take_snapshot()` で PNG を書く経路（`examples/verification/snapshot.rs`）は、
**show したあとにプロパティを書き換えても絵が変わらない**。タイマーで値を動かしてから撮っても、
撮れるのは変える前の 1 枚（#154 で `set_current_segment` / `set_time_text` の両方で確認。
バイト単位で同一の PNG になる）。

- したがって、examples で確認できるのは**開いた瞬間の状態**だけ。状態を変える引数は、
  タイマーではなく `show()` の前に置くこと。
- 「値が変わったときに動く」振る舞い（`changed` ハンドラ、スクロール追従など）は、この経路では
  検証できない。テストバックエンドも `Timer` を回さないので、そちらでも取れない。
  **手元で実際に動かして見るしかない**——検証できていないことを、PR に書いて残す。

## UI 操作の検証は `tests/` のテストバックエンドで、見た目は example ＋ screencapture で

Slint の UI には検証手段が 2 つあり、対象で使い分ける。片方だけでは「ビルドは通るのに
クリックが何も起きない」「配線は正しいのにレイアウトが潰れている」を取りこぼす。

- **操作（クリック・ドラッグ・状態遷移）**: `tests/` に統合テストを置き、
  `i-slint-backend-testing`（dev-dependency）で実際のポインタイベントを流す
  （`tests/ui_seekbar.rs` が実例）。`ElementHandle::find_by_element_type_name` で要素を
  探し、`mock_single_click` / `mock_drag` で操作して、コールバックの発火回数・引数と
  `out property` の値をアサートする。
- **見た目（レイアウト・配色・縮退表示）**: `examples/` の確認用バイナリ＋ screencapture
  （この文書の上のほうにある検証手順）。

テストバックエンド側の制約:

- `init_no_event_loop()` は**スレッドごとに 1 回だけ**呼べる（2 回目は panic）。`--test-threads=1`
  で同じスレッドに複数のテストが載る場合に備え、thread-local のフラグで 1 回に絞る。
- `Timer` や `invoke_from_event_loop` は動かない。100ms tick に依存する振る舞い（表示の
  自己修復など）はこのテストでは再現しないので、コールバック単体の契約に絞ってアサートする。
- `ElementHandle` は生成コードのデバッグ情報を要求する。`build.rs` が既定で有効にしているので
  素の `cargo test` で通る。出荷する release ビルドには入れないため、release を継承する
  プロファイル（`--release` / `bench`）ではテストが `slint_debug_info` cfg で切り替わり、
  「飛ばした」ことを報告するテストだけが走る。そこで実行したいときは
  `SLINT_EMIT_DEBUG_INFO=1` を付ける。
- 座標は `absolute_position()` / `size()` から算出する。ウィンドウサイズや周辺のレイアウトが
  変わっても追従し、フォント差のある環境でも壊れない。

## UI が添字で操作対象を指すなら、絞り込みは「素材」の段でやる

Slint の `for` は行の添字をそのままコールバックへ渡し、Rust 側はその添字で元の配列を引いて
操作対象（消すモデル・選ぶモデル）を決める。**つまり「UI に並ぶ行」と「Rust が持つ素材」は
同じ順序で 1 対 1 でなければならない**。

行を作る段で絞ると（`sources.iter().filter(...).map(row)`）、素材はそのままなので添字がずれる。
ずれ幅は間引いた件数ぶんで、`get(i)` は範囲内を返してしまうため**パニックも空振りもせず、
黙って別のものを消す**。

絞るなら素材を作る関数（`model_row_sources`）で絞り、UI へ渡す行と素材を常に同じ長さに保つ。
組で持つ型（`ModelListHandles`）の doc に 1 対 1 を明記し、テストでも
「行 `i` の名前が素材 `i` と一致する」ことを直接検査する。
