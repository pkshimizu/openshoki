# エージェント用 docs/ を git 管理から外す

- 作成日: 2026-06-28
- ステータス: ドラフト

## 概要

za エージェント（plan / issue / fix-issue / pr / review / init）が参照・生成する `docs/`
配下のファイルを git の追跡対象から外す。ローカルのファイル自体は残したまま、リポジトリの
共有履歴からは切り離し、今後追加される `docs/` 配下のファイルも自動で無視されるようにする。

## 背景・前提（コンテキスト）

- `docs/` ツリーは全体が za 用のドキュメント群（`za:init` が雛形を作成し、各スキルが
  読み書きする運用ファイル）。現状 16 ファイルが git 管理下にある:
  - 設定・ルール系: `CONTEXT.md` / `PLAN.md` / `ISSUE.md` / `PR.md`、`rules/`、
    `review-perspectives/`
  - 生成物: `plans/done/`（issue 化済みプラン）
- コードからの参照は `src/config.rs:5` のコメント1箇所（`docs/rules/error-handling.md`）のみ。
  CI / GitHub Actions・README からの参照は無い（`.github/` 無し、README 無し）。
- `.gitignore` は現状 `/target` のみ。

## 要件

- `docs/` 配下すべて（16 ファイル）を git の追跡対象から外す。
- ローカルのファイルは削除せず残す（`git rm --cached` 相当の挙動）。
- `.gitignore` に `docs/` を追加し、今後 za が生成・追加するファイルも自動で無視する。
- スコープ外:
  - `docs/` 配下のファイル内容の編集・移動・削除（中身はそのまま残す）。
  - `src/config.rs` のコメント文言の変更（後述「未確定事項」で扱う）。
  - 過去コミット履歴からの完全削除（履歴の書き換え（filter-branch 等）は行わない）。

## 確定した論点

- **対象範囲は `docs/` 全体**（ユーザー確認済み）。CONTEXT.md・rules・review-perspectives・
  plans すべて za エージェント用のため、一括で外す。
- **今後も自動で無視する**（ユーザー確認済み）。`.gitignore` に `docs/` を追加し、将来
  za が生成するファイルも追跡対象に入らないようにする。
- **履歴は書き換えない**: 過去コミットに含まれる docs は残るが、本要件は「以後の追跡を
  外す」ことが目的なので filter-branch 等は行わない（共有ブランチの履歴改変リスクを避ける）。

## 実装方針

`git rm --cached -r docs/` で追跡だけを外し（ワーキングツリーのファイルは保持）、`.gitignore`
に `docs/` を追記して再追跡を防ぐ。両者をまとめて 1 コミットにする。

`.gitignore` のパターンは末尾スラッシュ付きの `docs/` を使い、ディレクトリ配下すべてを
対象にする（先頭スラッシュは付けず、リポジトリ直下の `docs/` を確実に指す書き方で統一）。

## 実装ステップ

1. **追跡解除**: `git rm -r --cached docs/` を実行し、16 ファイルが staged delete に
   なること、かつワーキングツリーに `docs/` のファイルが残っていることを `git status` /
   `ls docs/` で確認する。
2. **.gitignore 更新**: `.gitignore` に `docs/` を追記する。追記後 `git status` で
   `docs/` 配下が untracked にも staged にも現れない（無視されている）ことを確認する。
3. **動作確認**: `git check-ignore docs/CONTEXT.md` がパスを返す（無視対象になっている）
   ことと、`git ls-files docs | wc -l` が 0 になることを確認する。
4. **コミット**: 変更（`.gitignore` 追記 + docs の追跡解除）を 1 コミットにまとめる。
   メッセージ例: `chore: エージェント用 docs/ を git 管理から外す`。

## 影響範囲・リスク

- 影響を受けるファイル:
  - `.gitignore`（`docs/` を追記）
  - git のインデックスから `docs/` 配下 16 ファイルを除外（ファイル実体は不変）
- リスクと対策:
  - **共有知識がリポジトリから消える**: CONTEXT.md やルールは他開発者・将来のクローンから
    見えなくなる。今回はユーザー方針に従い docs 全体を外すが、必要なら別途共有手段
    （別リポジトリ・Wiki 等）を検討する余地は残す。
  - **`src/config.rs:5` のコメントが未追跡ファイルを指す**: クローン直後はリンク先が
    存在せず参照が宙に浮く。コメントは provenance として残す方針だが、気になる場合は
    文言調整を検討（未確定事項）。
  - **作業ブランチ**: 現在のブランチは `feature/3-settings-save-directory`。この作業は
    設定保存機能とは無関係なので、独立したブランチ（例: `chore/untrack-agent-docs`）に
    分けるのが望ましい。

## 未確定事項

- `src/config.rs:5` の `docs/rules/error-handling.md` 参照コメントを残すか、文言を
  変える／削除するか。現方針は「残す」。
- 本作業をどのブランチ・どのタイミングで行うか（現ブランチの作業完了後に独立ブランチ推奨）。
