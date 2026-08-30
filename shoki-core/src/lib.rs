// **クレート属性は item より前に置く**（`//!` の後ろでも通るが、item の後ろだと
// 「inner attribute is not permitted in this context」で落ちる）。doc と混ぜず先頭に固めて
// おけば、#188 で `pub mod` をどこに足しても位置を考えなくて済む。
//
// FFI も `unsafe` も shell の仕事なので、`allow` で穴を開けられない `forbid` にする。
#![forbid(unsafe_code)]

//! shoki の純粋な層（`core`）。
//!
//! **この層は状態（`AppState`）・判断（`update` / `Effect`）・状態から文言への表だけを持つ。**
//! ファイルを読む・スレッドを立てる・画面を触るコードは `shoki` 側（`shell`）に置く。
//!
//! ```text
//! shell（副作用がある層）
//!   UI（Slint 配線）／ job runtime ／ adapters
//!         ↓ Command / Event        ↑ Effect / View
//! core（このクレート）
//!   update(&mut AppState, Msg) -> Vec<Effect>
//!   view_*(&AppState) -> ...
//! ```
//!
//! 状態を持つ場所を 1 つにし、判断を副作用から引き剥がすのが狙い。背景と計測は
//! `docs/plans/done/20260829-core-shell-layers.md`、今後も効く結論は `docs/CONTEXT.md` の
//! 「主要な設計判断」が正。
//!
//! # 何が守られていて、何が守られていないか
//!
//! **クレート境界が禁じるのは `shoki` 側の型に触ること**（Slint の生成型・whisper・adapter・
//! `private_file` などのヘルパー）。依存が `shoki-core` → `shoki` の向きに無いので、参照した
//! 時点でコンパイルが通らない。
//!
//! **`std` の I/O は止まらない。** 依存ゼロのクレートでも `std::fs::read_to_string` は書けるし、
//! `#![forbid(unsafe_code)]` も I/O は止めない。ここを「コンパイラが守ってくれる」と思い込むと、
//! `private_file`（0600 で作る保証。`docs/rules/security.md`）を通らない書き出しが core 側に
//! 生まれる。
//!
//! 代わりに `clippy.toml` の `disallowed-types` / `disallowed-methods` で、よくある入り口
//! （`std::fs` / `std::thread::spawn` / 時刻の取得）を CI で弾いている。**網羅ではないので、
//! レビューの代わりにはならない。**
//!
//! # いまの状態
//!
//! 読む領域の文言と状態（`reading_pane`。#188 の PR-1）と、録音セッションの事実と表示
//! （`session`。PR-3a）が入っている。`AppState` / `update` / `Effect` はまだ無い（PR-3b）。

pub mod reading_pane;
pub mod session;

// **doc とコードでは短いほう（`shoki_core::X`）で書く**。glob を張ってあるので同じ項目に
// 公開パスが 2 本あるが、参照が割れると「どちらが正か」を読み手が判断できなくなる。
pub use reading_pane::*;
pub use session::*;
