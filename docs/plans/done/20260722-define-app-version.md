# バージョン番号の定義とリリース運用

- 作成日: 2026-07-22
- ステータス: 確定

## 概要

openshoki のバージョン番号の「正」と運用ルールを定義し、リリース（GitHub Actions での
バイナリ生成）と結びつける。リリースワークフロー本体（`release.yml` / `.app` 生成 /
Releases 添付）は既存 issue #20 がカバーするため、本プランは**その差分**である
「バージョンをどこで定義し、どう上げ、どこに見せるか」に絞る。

## 背景・前提（コンテキスト）

- リリース配布の方針は確定済み（CONTEXT.md「配布は GitHub Actions で macOS `.app`
  バンドルをタグ起動ビルド」、元プラン `docs/plans/done/20260710-release-binary-github-actions.md`）。
  実装は issue #20 としてオープンのまま**未着手**（`.github/workflows/` は `ci.yml` のみ、
  `packaging/` 無し、タグ・リリースもゼロ）。
- #20 には「`v*` タグ push で起動」「タグと `Cargo.toml` の `version` の一致を CI で確認し
  不一致なら fail」「`Info.plist` へタグからバージョン注入」が既に含まれている。
  → タグ⇔バージョン整合の仕組みは #20 側にあり、本プランでは重複させない。
- 現在のバージョン定義は `Cargo.toml` の `version = "0.1.0"` のみ。ソースコードに
  `env!("CARGO_PKG_VERSION")` の参照は無く、アプリ内にバージョンを見せる場所も無い。
- 設定画面は `ui/app-window.slint`（`AppWindow`）。プロパティは Rust（`src/main.rs`）から
  セットする既存パターンがある。UI 文言は英語・タイポグラフィは `Style` トークンに従う
  （`docs/rules/messages.md` / `docs/rules/slint.md`）。

## 要件

- バージョン番号のスキームと「正」を定義し、リリース手順（バンプ→タグ→自動ビルド）を
  ドキュメント化する。
- アプリ内（設定画面）で自分のバージョンを確認できるようにする。
- スコープ外:
  - リリースワークフロー本体（`release.yml`・`.app` 組み立て・ad-hoc 署名・Releases 添付・
    タグ⇔`Cargo.toml` 整合チェック）— **#20 で実装**する。
  - CHANGELOG の自動生成、Conventional Commits によるバンプ自動化（release-plz 等）。
  - 自動更新機構。

## 確定した論点

ユーザー確認で決めた事項:

- **#20 を活かし、本プランは差分のみ**を扱う（リリースワークフローを二重にプラン化しない）。
- **`Cargo.toml` の `version` を唯一の正**とし、**SemVer** で運用する。リリース時は
  バンプコミット→ `v{version}` タグを手動 push。タグとの不一致は #20 の CI チェックが弾く。
  二重管理・自動注入の bot コミットを避け、仕組みが最小で済む。
- **最初のリリースは `v0.1.0`**（現在の `Cargo.toml` の値をそのまま使う。未署名・機能拡充中の
  段階なので 0.x が実態に合う）。
- **バージョンは設定画面に表示する**（ユーザーが自分のバージョンを確認でき、不具合報告に
  役立つ。実装コストが小さい）。トレイメニューには出さない。

調査で解消した事項:

- タグ⇔`Cargo.toml` の整合チェック・`Info.plist` へのバージョン注入は #20 の受け入れ条件に
  既に含まれており、本プランで新設する必要は無い。
- アプリへのバージョン埋め込みは `env!("CARGO_PKG_VERSION")`（コンパイル時定数）で行える。
  ランタイム依存も追加クレートも不要で、`Cargo.toml` が正である方針とも一致する。

## 実装方針

- **スキーム**: SemVer（`MAJOR.MINOR.PATCH`）。0.x の間は「MINOR=機能追加・破壊的変更、
  PATCH=修正」の慣例で運用する。
- **正**: `Cargo.toml` の `version`。他の場所（タグ・`Info.plist`・UI 表示）はすべて
  ここから導出される（タグは手動だが CI が不一致を fail させる。#20）。
- **リリース手順**（README の開発節に記載する）:
  1. `Cargo.toml` の `version` をバンプするコミットを作る（例: `chore: v0.2.0 へバンプする`）。
  2. `main` へマージ後、`git tag v0.2.0 && git push origin v0.2.0`。
  3. `release.yml`（#20）がビルドして GitHub Releases に添付する。
- **UI 表示**: 設定画面（`ui/app-window.slint`）の末尾に補助色の小さいテキストで
  `openshoki v0.1.0` を表示する。Slint に `in property <string> app-version` を追加し、
  Rust 側で `env!("CARGO_PKG_VERSION")` からセットする（フォーマットは Rust 側で組み立てる）。

## 実装ステップ

1. **設定画面へのバージョン表示**
   `ui/app-window.slint` に `app-version` プロパティと末尾の表示（`Style.caption-size`・
   補助色）を追加し、`src/main.rs` で `env!("CARGO_PKG_VERSION")` をセットする。
   確認: `cargo build` が通り、設定ウィンドウを開くと `openshoki v0.1.0` が表示される
   （screencapture で目視。`docs/rules/slint.md` の検証手順）。
2. **バージョン方針とリリース手順のドキュメント化**
   README の開発節に「バージョニング（SemVer・`Cargo.toml` が正）」と「リリース手順
   （バンプ→タグ→自動ビルド）」を追記する。`docs/CONTEXT.md` の設計判断にバージョン管理の
   項を追加する（本プラン作成時に追記済みなら整合だけ確認）。
   確認: README の手順どおりに操作すると #20 のワークフロー（実装後）が起動する構成に
   なっている。
3. **検証**
   `cargo build` / `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` /
   `cargo test` がすべて通る。
   （実際のタグ push での end-to-end 確認は、#20 実装後の初回リリース `v0.1.0` で行う。）

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - `ui/app-window.slint`（`app-version` プロパティと表示追加）
  - `src/main.rs`（バージョンのセット）
  - `README.md` / `docs/CONTEXT.md`（バージョニングとリリース手順の記載）
  - `Cargo.toml` は現状の `0.1.0` のまま（初回リリースまでバンプ不要）
- リスクと対策:
  - **バンプ忘れのままタグを打つ**: #20 の CI 整合チェックが fail させる（本プランでは
    仕組みを足さない。README の手順にも「バンプが先」と明記する）。
  - **表示とバイナリの不一致**: `env!` はコンパイル時定数なので、`Cargo.toml` を上げれば
    ビルドのたびに追従する。不一致が起きる経路は無い。
  - **#20 未実装の間はタグを打ってもリリースされない**: 依存として明示する
    （本プランの成果物は #20 が無くても動くが、リリース手順の end-to-end は #20 完了後）。

## 未確定事項

- 0.x をいつ 1.0.0 に上げるか（正式署名・公証や機能の安定を目安に後続で判断）。
