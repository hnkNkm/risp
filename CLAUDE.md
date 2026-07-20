# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## プロジェクト概要

Risp (Rust + Lisp) — Rust の型システムを持つ Lisp ライク言語（拡張子 `.rsp`）。コンパイラは Rust + LLVM 18（inkwell）で、本物の ELF/Mach-O バイナリを生成する。README.md に言語仕様・ロードマップが詳しく書かれている。

## 開発環境・コマンド

LLVM 18 が必須。Nix flake 同梱: `nix develop`（または `direnv allow`）で LLVM 18 と `LLVM_SYS_181_PREFIX` がセットされる。

```sh
cargo build
cargo test                        # unit + e2e 全部
cargo test --test examples        # e2e のみ（examples/*.rsp をビルド・実行・検証）
cargo test examples_match_expectations       # 単一テスト（名前指定）
cargo test error_examples_match_diagnostics  # エラー診断 e2e のみ

# cargo risp = `cargo run --quiet --` のエイリアス（.cargo/config.toml）
cargo risp run examples/hello.rsp
cargo risp emit-llvm examples/hello.rsp      # LLVM IR を stdout へ
cargo risp emit-ast examples/hello.rsp       # AST dump
cargo risp repl                              # JIT REPL
```

lint / format: `cargo clippy` / `cargo fmt`。

## アーキテクチャ

パイプライン（`src/main.rs` が接続）:

```
.rsp → Lexer → Parser (S式) → AST → Resolve (module/import)
     → MacroExpand (defmacro) → TypeCk → Codegen (LLVM IR)
     → object file → cc リンク（runtime/risp_rt.c と）
```

| ステージ | ファイル | 補足 |
|---|---|---|
| Lexer | `src/lexer.rs` | 手書き。`math/add` のような修飾名は1トークン |
| Parser | `src/parser.rs` | 手書き再帰下降。トップレベルは `parse_toplevel`、特殊形式は `parse_expr` で分岐 |
| AST | `src/ast.rs` | `Type` / `ExprKind` / `TopLevel`。関数呼び出し・builtin・struct/enum 構築はすべて `Call` に集約。`Expr.ty` に型検査結果が入る |
| Resolve | `src/resolve.rs` | `(import name)` で同ディレクトリの `name.rsp` を読み込み、`name/` 接頭辞でマージ |
| MacroExpand | `src/macroexpand.rs` | 型検査前に非衛生的な置換で展開 |
| TypeCk | `src/typeck.rs` | 型検査 + 所有権/借用検査。builtin の分岐は `check_call` |
| Codegen | `src/codegen.rs` | 最大のファイル。IR 生成 + drop/retain + TCO |
| ランタイム | `runtime/risp_rt.{c,h}` | `risp_str_*` / `risp_rc_*` / `risp_vec_i32_*` / `risp_box_*`。build.rs が cc でビルドしリンク |
| 診断 | `src/diagnostic.rs` | 行・列・キャレット表示 |
| REPL | `src/repl.rs` | inkwell の JIT |

### メモリ管理（typeck と codegen をまたぐ）

- Copy 型: 数値 / bool / Array / str / Rc / Weak / unit（str/Rc/Weak は使用時 retain）。ムーブ型: `Named`(struct/enum) / `Box` / `Vec` — typeck の `is_move_type` と `Local.moved` フラグで検査
- codegen 側の機構: `needs_drop` / `emit_drop` / `emit_retain` / `store_owned` / `load_owned`（Move=take で alloca を zero、Place=観測のみ）。再帰 enum は `emit_drop_fn_bodies` が名前付き drop 関数を生成してコンパイル時再帰を回避
- `Ref`(&T) は関数から返せず、ADT フィールド/グローバルにも置けない（延命禁止の制限付き保証）

### 新機能の配線順

lexer（新トークンがあれば）→ parser → ast → typeck（ムーブ型なら `is_move_type` も）→ codegen（ヒープ所有なら drop/retain も）→ 必要なら runtime/risp_rt.c → e2e フィクスチャ追加。既存の手書きスタイルに合わせる。

## テスト規約

e2e は `examples/*.rsp`（正常系、1機能1ファイル）と `examples/errors/*.rsp`（診断）で、期待値をファイル先頭の `;;!` ヘッダに書く:

```lisp
;;! stdout: Hello, Risp!      ← 繰り返し可
;;! exit: 0
```

```lisp
;;! error_at: 7:14
;;! error_contains: undefined variable
```

`tests/examples.rs` がこれらを走らせる。lexer / parser / macroexpand には inline unit test もある。

## Git / PR 規約

- コミットは **ユーザーが明示的に依頼したときだけ**
- コミットメッセージ: `<type>: <概要> - <詳細>`（type は feat/fix/refactor/chore/docs/build、概要は日本語60文字以内）
- ブランチは `feat/<name>` 等で切り、Issue 単位の小さい PR にする
- PR 本文の見出し（日本語）: `### 概要` / `### 変更内容` / `### 動作確認` / `### 備考`
