# 実装ルール: ユーザー向けメッセージ

ユーザーが目にする文言（GUI ラベル・ツールチップ・メニューバー表示、および `eprintln!` /
`println!` / `Err(...)` / `expect` / `panic!` のログ・エラー）を書くときの規約。ソース内の
コメント（`//` / `//!` / `///`）と `docs/` はこの規約の対象外（日本語でよい）。

## 英語で書く

ユーザー向けの文言はすべて英語で書く（i18n / ロケール切替は導入しない）。日本語リテラルを
残さない。目安の検証:

```sh
grep -rnP '"[^"]*[ぁ-んァ-ヶ一-龠][^"]*"' src ui --include='*.rs' --include='*.slint' \
  | grep -v '^src/summarize' | grep -vP ':\s*//'
```

の結果が**装飾グリフだけ**であること（下の例外を参照）。コメントの日本語は可なので、
`//` 始まりの行は落としている（行末コメントに引用符付きの日本語があると素の grep では
ヒットしてしまう）。

### 例外: 装飾の漢字グリフ（`FeatureHeading` / `Door` の `glyph`）

機能を表す 1 文字の漢字（文 / 議）は**意匠であって文言ではない**。プロダクト名（書記）と
アプリアイコン（筆の一画）と同じ系統の記号で、意味を担うのは隣に並ぶ英語のラベル。

- 必ず `accessible-role: none` を付け、**読み上げ対象から外す**（読めない記号を読ませない）。
- グリフだけで意味が決まる置き方をしない（英語ラベルを消して記号だけにしない）。
- フォントは同梱していないので OS 任せ。出せない環境では豆腐になるが、隣のラベルが本体なので
  意味は失われない。

上の grep はこの 4 箇所（`ui/transcription-window.slint` / `ui/minutes-window.slint` /
`ui/app-window.slint` の扉 2 つ）にだけヒットする。増えたら、それが装飾か文言かを判断すること。

### 記号は ASCII / Latin-1 に収める

上の漢字グリフ以外の記号（`⌕` / `✕` / `▶` など）は**使わない**。フォントを同梱していないので
多くの環境で豆腐になり、意味を伝えないうえに見た目も壊れる。漢字グリフが例外なのは、隣に英語の
ラベルが必ずあって**意味を担っていない**から（#161 で `⌕` と `✕` が実際に豆腐になった）。

- 使ってよい: `+`（`ui/app-window.slint` の追加ボタン）、`×`（Latin-1。消す操作）、`·`（区切り）
- 代わりに**言葉で言う**。検索欄なら虫めがねを置かずプレースホルダで `Search …` と書く

### 例外: LLM へ渡すプロンプト（`src/summarize*`）

議事録要約のプロンプト（`src/summarize.rs` の `MINUTES_SYSTEM_JA` / `NOTES_SYSTEM_JA` と、
日本語の user プロンプト）は**ユーザー向けの文言ではなくモデルへの入力データ**なので、この
規約の対象外。**出力言語は「何語で指示するか」で決まる**ため、英語へ直すと生成される議事録の
言語そのものが変わる（whisper へ渡す言語コードと同じ性質のパラメータ）。上の grep はこのため
`src/summarize` を除外している。テストの assert に出てくる日本語（期待する見出しなど）も同様。

同じ理由で、プロンプトの文面は #78 の品質検証で確定したものをそのまま使う。

### 例外: テストの入力データ

テストが**入力として与える**日本語（検索語、議事録の中身など）は文言ではなくデータ。日本語の
会議を録るアプリなので、日本語で当たることを検査する側に日本語が要る（#161 の
`session_matches_looks_at_the_transcript_and_the_notes`）。上の grep はここにも当たるので、
ヒットしたら**それが画面に出るか**で判断すること。変えるなら
`docs/plans/done/20260722-meeting-minutes-summary.md` の判定基準で再評価すること。

## 製品名 shoki は変えない

ウィンドウタイトル・アイドル時ツールチップなどの製品名 `shoki` はそのまま保持する
（`openshoki` から改名した経緯は #111。旧名を復活させない）。

## 用語と文体を揃える

- 同じ概念には同じ語を使う。録音の保存先は、**画面に出す文言では `save location`** に揃える
  （Settings のラベルが `Save location` なので、読んだ人が同じものだと分かる）。`path` と
  混在させない。**ログの `recording folder`（`src/config.rs` / `src/main.rs` の 3 箇所）は
  未統一**——画面には出ないが同じ概念に 2 語なので、揃えるのは別 issue。
- **装飾の記号をラベルに混ぜない**（`＋ Add app…` ではなく `Add app…`）。ラベルはそのまま
  `accessible-label` になるので、飾りの記号まで読み上げられる。記号は部品側の別プロパティ
  （`ActionButton` の `glyph`）で足す。全角記号は日本語混入の grep にも掛からない。
- **支援技術が読むラベルも GUI ラベル**。可視ラベルと同じ文字列を渡すのが原則で、同名が複数
  あるときだけ区別できる名前にしてよい（`Transcription model` / `Meeting notes model`）。
  可視ラベルが文章形式で長い場合は短い名詞句にしてよいが、**sentence case は守る**
  （`auto-stop delay in seconds` ではなく `Auto-stop delay`）。
- GUI ラベルは **sentence case**（`Save location` / `Add app…` / `Delete this model?` /
  `Move this recording to the Trash?`）。#127 のデザインに合わせて Title Case から変えた。
  **すべてのウィンドウが移行済み**（設定画面 #127、機能ウィンドウ #141、Recordings #128）。
  **トレイメニューの項目（`src/tray.rs` の `Start Recording` / `Settings` / `Recordings…`）は
  対象外**——OS のメニューなので Title Case のままにする。
- **綴りは米式に揃える**（`summarize` / `recognize` / `behavior`）。設定キー（`auto_summarize`）と
  Recordings の `Summarize` が米式なので、そちらへ寄せる。同じ動詞が画面ごとに割れると、同じ
  操作に見えなくなる。
- 件数に依存する言い方をしない（`the other` ではなく `the rest`）。カタログに 1 件足した瞬間に
  嘘になる文言を書かない。
- 縮退・フォールバックのログは「`Continuing … because …`」「`Skipping … because …`」のように
  **1 文構造**で揃える（独立節をカンマで繋ぐカンマスプライスを避ける）。
- 複合修飾語はハイフンで繋いで揺れをなくす（`system-audio` / `mic-recording`）。

## フォーマット指定子を壊さない

文言を書き換えるときも `{err}` / `{status}` / `{sample_format:?}` などのインライン引数と、
位置引数 `{}`（例: `println!("Saved the recording ({} files)", saved.len())`）を保つ。
取りこぼしや余分な `{}` を入れない。
