# Tine

Linux 向けの Git クライアントです（仮称）。Fork の画面構成と操作感を参考に、Rust と egui で作っています。

## ビルドと起動

```bash
cargo build --release
./target/release/tine /path/to/repo   # リポジトリを指定して開く
./target/release/tine                 # 前回開いていたリポジトリを開く
```

- 必要なもの: Rust 1.95 以上、git、C リンカ（gcc）、fontconfig（`fc-match`）
- 開発用ヘッダ（GTK や Qt）は不要です。Wayland/X11 と Vulkan/GL は実行時に読み込みます。

## 操作

| キー | 動作 |
|---|---|
| Ctrl+O | リポジトリを開く |
| F5 / Ctrl+R | 再読込 |
| ↑ ↓ PageUp PageDown Home End | コミット一覧の移動 |
| Ctrl+W | タブを閉じる |
| Ctrl+Enter | コミット（コミットメッセージ入力中） |
| ダブルクリック | ファイルを stage / unstage |
| Ctrl+クリック / Shift+クリック | ファイルを複数選択 |
| 差分の行をクリック / ドラッグ | 行を選択（Ctrl でトグル、Shift で範囲） |
| F12 | デバッグ表示（フレーム時間・メモリ） |

## 動作上の注意

**commit-graph キャッシュの自動作成**
- 開いたリポジトリに git の commit-graph キャッシュがない場合、バックグラウンドで `git commit-graph write --reachable` を実行します。
- これは `git gc` や `git maintenance` が通常作成するものと同じファイルです。
- キャッシュがないと、大きなリポジトリでは履歴の最初の行が出るまで数秒かかります。

**変更の破棄**
- 破棄する前に確認ダイアログを出します。
- 追跡中のファイルの変更は元に戻せません。
- 未追跡ファイルは削除せず、デスクトップのゴミ箱に移します。

**hunk・行単位の操作**
- 差分の hunk 見出しにあるボタンで、hunk 全体または選択した行を stage / unstage / 破棄できます。破棄は確認してから実行します。
- 対象は、変更されたテキストファイルだけです。
- 新規・削除・名前変更・バイナリ・サブモジュール・モード変更のファイルは、ファイル単位で操作します。
- 「Ignore whitespace」がオンの間は、行単位の操作はできません。
- 実行する直前に差分を取り直し、表示していた差分と同じであることを確認します。表示後にファイルが変わっていた場合は、何も変更せずにエラーを表示します。
- 操作した直後は、差分を読み直すまでの間、行単位の操作を無効にします。
- 末尾に改行がない行の後ろに行を足すような選択は、行がつながってしまうため実行しません。改行の変更を含む隣の行も一緒に選んでください。

**コミットの作成者**
- git の設定（`user.name` / `user.email`）をそのまま使います。
- 未設定のリポジトリでは、git のエラーがそのまま表示されます。

**自動更新**
- 作業ツリーと `.git` の変更を監視し、ステータス・ブランチ・履歴を自動で再読込します。
- git が無視するファイル（ビルド出力など）の変更では再読込しません。
- inotify の監視数の上限などで監視できない場合は、F5 キーかウィンドウへのフォーカスで再読込します。

## 開発

```bash
cargo test                                               # 単体テスト・結合テスト
cargo clippy --all-targets -- -D warnings
cargo test -p tine --test screenshots -- --ignored       # 画面を target/screenshots/*.png に出力
TINE_PERF_REPO=/path/to/big/repo cargo test --release -p tine --test perf -- --ignored --nocapture
cargo run --release -p tine-bench -- /path/to/repo       # 履歴読込とグラフ計算の計測
./target/debug/tine --ime-probe                          # 日本語入力（IME）の確認画面
```

### 構成

| クレート | 内容 |
|---|---|
| `crates/tine-core` | GUI に依存しない git 層（CLI 実行、log/refs/status/diff の解析、コミットグラフのレーン計算） |
| `crates/tine` | eframe/egui のアプリ本体 |
| `crates/tine-bench` | 計測用の CLI |
