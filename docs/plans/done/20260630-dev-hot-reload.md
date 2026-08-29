# 開発時のホットリロード（自動 再ビルド＋再起動）

- 作成日: 2026-06-30
- ステータス: ドラフト

## 概要

開発中にソースを編集したら、手動で停止・再起動しなくても自動で再ビルドしてアプリを
再起動できるようにする。`cargo-watch` でソースを監視し、変更検知で `cargo run` をやり直す
方式を採る。狙いは UI（`.slint`）やロジック（Rust）の試行錯誤を速く回すこと。

## 背景・前提（コンテキスト）

- openshoki は `cargo run` で起動する **常駐**型アプリ（`docs/CONTEXT.md`）。GUI は Slint、UI は
  `build.rs` の `slint_build::compile` でビルド時にコンパイルする（`include_modules!`）。
- `slint-build` 1.17 は `cargo:rerun-if-changed=ui/app-window.slint` を出すため、`.slint` を編集すると
  cargo の再ビルド対象になる。一方、**実行時に `.slint` を差し替えるライブリロード機構は
  この版には無い**（`slint-interpreter` 未導入）。そのため「保存→自動で再ビルド＆再起動」で
  ホットリロード相当を実現するのが現実的。
- `docs/` は `.gitignore` 済み（issue #7）。`target/` も無視。`cargo-watch` は既定で `.gitignore` を
  尊重するため、これらは監視ノイズにならない。
- 録音機能（cpal / libmp3lame）を含むためフルビルドは重いが、差分ビルドは速い。

## 要件

- `src/` / `ui/` / `build.rs` / `Cargo.toml` の変更を検知して、自動で再ビルドしアプリを再起動する。
- 開発者は短いコマンド一発（例: `cargo dev`）で監視付き起動できる。
- 本体のコードや本番ビルドには影響を与えない（開発専用の仕組み）。
- スコープ外:
  - 実行時の UI ライブリロード（`slint-interpreter` で `.slint` を即時差し替える方式）。
  - 状態を保持したままのリロード（再起動で常駐・録音セッションはリセットされる）。
  - CI への組み込み（ホットリロードは開発時のみ。CI は既存の build/clippy/test/audit のまま）。

## 確定した論点

ユーザー確認で決定:

1. **方式**: ファイル監視で自動 再ビルド＋再起動。Rust ロジックと `.slint` UI の両方の変更に
   対応でき、コード変更がほぼ不要で導入が軽い。常駐・録音状態は再起動で失うが、開発
   イテレーションには十分。
2. **ツール**: `cargo-watch`（`cargo watch -x run`）。定番で設定が最小。
3. **提供方法**: `.cargo/config.toml` に `cargo dev` エイリアスを置く（追跡対象。`docs/` 配下では
   ないのでリポジトリに入る）。`cargo-watch` 自体は各自 `cargo install cargo-watch` で導入。

調査で解消した点:
- `slint-build` 1.17 に実行時ライブリロードは無く、`rerun-if-changed` による再ビルドのみ
  （`slint-build` の `lib.rs` を確認）。よって自動再起動方式が妥当。
- `cargo-watch` は `.gitignore` を尊重するため、`target/`・`docs/` は自動的に監視対象外。

## 実装方針

- **`.cargo/config.toml` に開発用エイリアスを追加**する。
  ```toml
  [alias]
  # 開発用ホットリロード: ソース変更を監視して自動で再ビルド＆再起動する。
  # 事前に `cargo install cargo-watch` が必要。
  dev = "watch -w src -w ui -w build.rs -w Cargo.toml -x run"
  ```
  - 監視対象を明示（`-w`）して、無関係な変更での再起動を避ける。`.slint` 変更は `slint-build` の
    `rerun-if-changed` でビルドに反映される。
- **導入手順を追跡されるファイルに残す**。`docs/` は追跡外なので、`.cargo/config.toml` の
  コメントに前提（`cargo install cargo-watch`）を書く。必要なら `README.md`（無ければ新規）に
  「開発」節を設けて `cargo dev` を案内する（最小構成ではコメントのみでも可）。
- 本体コード（`src/`・`build.rs`）は変更しない。ホットリロードはあくまで開発時のコマンド層で
  完結させる。

## 実装ステップ

1. **`.cargo/config.toml` に `dev` エイリアスを追加**する（無ければ新規作成）。`cargo dev` が
   `cargo watch -w src -w ui -w build.rs -w Cargo.toml -x run` に展開されることを確認する。
2. **導入手順を記載**する。`.cargo/config.toml` のコメントに `cargo install cargo-watch` の前提を
   書く（必要なら `README.md` に開発節を足す）。
3. **動作確認**する。`cargo install cargo-watch` 後に `cargo dev` で起動し、`src/` または
   `ui/app-window.slint` を編集して保存すると、自動で再ビルドされアプリが再起動することを
   確認する。`.gitignore` 配下（`docs/`・`target/`）の変更では再起動しないことも確認する。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - 新規: `.cargo/config.toml`（`dev` エイリアス）。必要なら `README.md`。
  - 本体コード（`src/`・`build.rs`・`ui/`）への変更なし。
- リスクと対策:
  - **`cargo-watch` の別途インストールが必要**: 開発者環境依存。コメント/README で前提を明示。
    CI には不要（開発専用）。
  - **再ビルド中はアプリが落ちている**: フルビルドは重い（Slint＋libmp3lame）。差分ビルドは速い
    ため通常は許容。重さが問題なら将来 `bacon` 等の差分最適化を検討。
  - **再起動で録音セッションが中断**: 録音中にソースを編集すると録音が切れる（開発時の自明な
    挙動）。本番には影響しない。
  - **macOS の権限（マイク/画面収録）**: `cargo-watch` は同じ `target/debug/openshoki` を再ビルド・
    再実行するためバイナリパスは不変で、一度付与した TCC 権限は基本的に維持される見込み。
    挙動はステップ 3 で確認する。

## 未確定事項

- `README.md` を新規に設けるか、`.cargo/config.toml` のコメントのみに留めるか（実装時に、
  既存ドキュメントの有無を見て判断）。
- 将来 UI を本格化させた場合に、実行時 UI ライブリロード（`slint-interpreter`）を別途検討するか。
