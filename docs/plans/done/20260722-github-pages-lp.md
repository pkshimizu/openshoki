# アプリ紹介 LP を GitHub Pages に作成する

- 作成日: 2026-07-22
- ステータス: 確定

## 概要

openshoki の紹介用ランディングページ（LP）を作り、GitHub Pages
（`https://pkshimizu.github.io/openshoki/`）で公開する。GitHub リポジトリの README より
手前にある「アプリとして何ができるか」を伝える入り口を用意し、リリース配布（#20 / #74）への
導線にする。

## 背景・前提（コンテキスト）

- リポジトリは **PUBLIC**（GitHub Pages を無料で利用可能）。Pages は**未設定**
  （`gh api repos/.../pages` が 404）。
- **`docs/` は gitignore 済み**（za 用のローカルドキュメント置き場）。そのため Pages の
  「main の /docs フォルダ」ソースは使えない。→ LP は `site/` ディレクトリに置き、
  GitHub Actions の公式デプロイ（`actions/deploy-pages`）で公開する。
- アプリの UI 文言は英語（CONTEXT.md「ユーザーに見せるメッセージは英語」）。README は日本語。
- 配布物: `v*` タグで `.app` zip を GitHub Releases に添付する方針（#20、未実装）。
  未署名（ad-hoc）のため、インストール時に「右クリック→開く」の案内が必要（#20 の
  リリースノート方針と同じ内容を LP にも載せる）。
- 動作要件: macOS 13+、Apple Silicon（arm64）。マイク／画面収録の許可が必要。
- 見た目に使える素材: トレイ・メニューのアイコン素材は `assets/` にあるが、`.app` 用の
  正式アプリアイコンは未整備（#20 でも暫定）。スクリーンショットは実アプリ＋確認用
  example（`examples/transcript_view.rs` など）で撮れる。
- 既存 CI（`ci.yml`）は Rust の検証のみ。LP は Rust ビルドと無関係なので、ワークフローは
  分離し `site/**` の変更でだけ動かす。

## 要件

- `https://pkshimizu.github.io/openshoki/` で LP が公開される。
- **日英両方**のページを持つ: 英語を `/`（`site/index.html`）、日本語を `/ja/` に置き、
  相互に言語切替リンクと `hreflang` を張る。
- 掲載内容（両言語で同等）:
  - ヒーロー: アプリ名・タグライン・ダウンロード CTA（GitHub Releases 最新版への
    リンク）・「ソースからビルド」への代替リンク（README へ）。
  - 主な機能: メニューバー常駐のワンクリック録音／マイクとシステム音声の分離保存／
    会議アプリ連動の自動録音・自動停止／オンデバイス whisper 文字起こし（外部送信なし）／
    録音一覧・再生・文字起こし表示とクリックでのスキップ。
  - プライバシー: 音声・文字起こしを外部送信しない（通信はモデルの初回ダウンロード受信のみ）。
  - 動作要件: macOS 13+（Apple Silicon）、マイク／画面収録の許可。
  - インストール手順: zip 展開と未署名アプリの開き方（右クリック→開く）。
  - フッター: GitHub リポジトリ・ライセンスへのリンク。
- **実アプリのスクリーンショット**を掲載する（メニューバーのトレイメニュー・設定画面・
  Recordings ウィンドウの文字起こし表示）。
- ライト/ダーク両テーマに追従する（`prefers-color-scheme`）。
- `site/**` の push で自動デプロイされる。
- スコープ外:
  - カスタムドメイン（必要になったら CNAME を後続で追加）。
  - 静的サイトジェネレータの導入、ブログ・複数ページ構成。
  - アクセス解析の導入。
  - 正式アプリアイコンのデザイン（暫定素材の流用に留める）。

## 確定した論点

ユーザー確認で決めた事項:

- **言語は日英両方**（英語 `/`＋日本語 `/ja/` の 2 ページ構成で、切替リンクを付ける）。
- **URL は既定の github.io**（DNS 設定不要。カスタムドメインは後続で追加可能）。
- **素の HTML/CSS**（`site/` 配下に静的ファイルを置くだけ。ビルド不要・依存ゼロで
  壊れにくく、1 ページ LP に十分）。
- **実アプリのスクリーンショットを掲載**する。

調査で解消した事項:

- **Pages のソースは Actions デプロイ一択**: `docs/` が gitignore 済みのため
  「/docs フォルダ」方式は使えない。`gh-pages` ブランチ方式より、公式の
  `actions/upload-pages-artifact` + `actions/deploy-pages`（`site/` をそのまま
  アーティファクト化）が履歴も汚さず簡潔。
- **Pages の有効化**: `actions/configure-pages` の `enablement: true` で初回実行時に
  自動有効化できる（失敗する場合はリポジトリ設定から手動で「GitHub Actions」ソースを
  1 回選ぶフォールバック）。
- **ダウンロード CTA のリンク先**は `https://github.com/pkshimizu/openshoki/releases/latest`
  に固定する。リリースが存在しない間（#20 未実装）は Releases 一覧に落ちるため、
  「ソースからビルド」リンクを併記して導線が切れないようにする。LP 側は #20 完了を
  待たずに公開できる。

## 実装方針

- `site/` 配下に静的ファイルを置く:
  - `site/index.html`（英語）・`site/ja/index.html`（日本語）— 同一構造で文言のみ差し替え。
    `<link rel="alternate" hreflang>` と相互の言語切替リンクを持たせる。OGP メタ
    （title / description / og:image）も設定する。
  - `site/style.css` — 共有スタイル。配色はアプリのアクセント（macOS ブルー系）に合わせ、
    `prefers-color-scheme` でライト/ダーク追従。フレームワーク・外部 CDN は使わない
    （フォントはシステムフォントスタック）。
  - `site/assets/` — スクリーンショット PNG・アイコン・favicon。
- スクリーンショットは実アプリ／確認用 example を起動して `screencapture` で撮影する
  （`docs/rules/slint.md` の検証手順と同じ要領。文字起こし表示は
  `examples/transcript_view.rs` のダミーデータで見栄えの良い状態を作れる）。
  ダーク・ライトはどちらか一方（ダーク推奨）に統一して撮る。
- `.github/workflows/pages.yml` を追加する:
  - `on: push: branches: [main], paths: ['site/**', '.github/workflows/pages.yml']` と
    `workflow_dispatch`。
  - `permissions: pages: write, id-token: write`。
  - `actions/configure-pages@v5`（`enablement: true`）→
    `actions/upload-pages-artifact@v3`（`path: site`）→ `actions/deploy-pages@v4`。
  - Rust のビルドは行わない（既存 `ci.yml` とは独立）。

## 実装ステップ

1. **LP の骨組みと文言（英語）**
   `site/index.html` と `site/style.css` を作成する（ヒーロー・機能・プライバシー・
   動作要件・インストール・フッター。スクリーンショットはプレースホルダ）。
   確認: ローカルで `python3 -m http.server -d site` を起動しブラウザで表示、
   ライト/ダーク切替・モバイル幅での崩れがないことを目視確認。
2. **スクリーンショット撮影と差し込み**
   実アプリ（トレイメニュー・設定画面）と `examples/transcript_view.rs`
   （Recordings＋文字起こし）を起動して `screencapture` で撮影し、`site/assets/` に
   配置して LP に組み込む。
   確認: 画像が Retina でも粗くなく、ページ全体の転送量が過大でない（画像は圧縮する）。
3. **日本語ページ**
   `site/ja/index.html` を作成し（構造は英語版と同一・文言のみ日本語）、両ページに
   言語切替リンクと `hreflang` を張る。
   確認: 相互リンクで行き来でき、内容が両言語で同等。
4. **デプロイワークフローと公開**
   `.github/workflows/pages.yml` を追加し、main へのマージで Pages が有効化・デプロイ
   されることを確認する。
   確認: `https://pkshimizu.github.io/openshoki/` と `/ja/` が表示され、以後
   `site/**` の変更 push で自動更新される。README に LP の URL を追記する。

## 影響範囲・リスク

- 影響を受けるファイル/モジュール:
  - 追加: `site/`（`index.html` / `ja/index.html` / `style.css` / `assets/`）、
    `.github/workflows/pages.yml`
  - 変更: `README.md`（LP の URL を追記）、`docs/CONTEXT.md`（LP の設計判断。プラン作成時に
    追記済みなら整合確認のみ）
  - Rust コード・既存 `ci.yml` への変更は**なし**
- リスクと対策:
  - **ダウンロード CTA の先が空**（#20 未実装の間）: 「ソースからビルド」リンクを併記し、
    初回リリース後は自動的に latest へ解決される。LP の公開自体は #20 に依存しない。
  - **Pages 自動有効化の失敗**: `configure-pages` の `enablement: true` が権限で失敗したら、
    リポジトリ設定（Pages → Source: GitHub Actions）を 1 回手動設定するフォールバックを
    実装ステップ 4 に含める。
  - **スクリーンショットの陳腐化**: UI 変更のたびに撮り直しが要る。撮影手順（使う example と
    screencapture のコマンド）を `site/assets/` 内の README に残して再撮影を容易にする。
  - **日英の文言乖離**: 構造を同一に保ち、更新時は両ページを同時に直す旨を site 内
    コメントに明記する（ページが増えたらジェネレータ導入を再検討）。

## 未確定事項

- タグライン等のコピーの最終文言（実装ステップ 1 のドラフトを見て調整）。
- カスタムドメイン（noncore.net 配下等）への移行時期（必要になったら CNAME を追加）。
