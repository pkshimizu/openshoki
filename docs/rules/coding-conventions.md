# 実装ルール: コーディング規約

## Rust edition は 2024

このプロジェクトは **Rust edition 2024** を使う（`Cargo.toml` で指定済み）。edition 2024 は
Rust 1.85 以降で安定しており、`if ... && let ...` の let-chains などが使える。

- 「edition 2024 は存在しない／未サポート」という指摘は誤り。`cargo build` が通ることで確認できる。
- `cargo clippy` が `collapsible_if` 等で let-chain への結合を提案したら、それに従ってよい。

## マジックナンバーは const にする

意味のある数値はリテラル直書きにせず、名前付き定数にして意図を表す。

```rust
// NG
let radius = SIZE as f32 * 0.4;

// OK
const RADIUS_RATIO: f32 = 0.4; // ドットの半径はアイコン一辺に対する割合
let radius = SIZE as f32 * RADIUS_RATIO;
```

## 状態に対応する UI 文言は const にして散らさない

ウィンドウの可視状態とトレイ項目のラベルのように、**状態と表示文言が対応する**ものは、
文字列リテラルを複数箇所に直書きしない。1 箇所変えて他方が追従漏れすると、表示状態と
ラベルが食い違う「ありえない状態」になる。

- ラベルは `pub const TOGGLE_LABEL_SHOW: &str = "...";` のように定数化し、初期値・更新の
  双方から同じ定数を参照する。
- 表示/非表示のような対の操作は、状態変更とラベル更新をまとめた小関数
  （例: `show_window` / `hide_window`）に切り出し、対応関係を一箇所で保証する。

## 検証コマンド

コミット前に次が通ること（PR の確認事項と対応）:

- `cargo build`
- `cargo fmt --check`
- `cargo clippy --all-targets -- -D warnings`
