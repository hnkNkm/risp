# Risp

Rustの型システムを持つLispライクなプログラミング言語。LLVM経由でネイティブバイナリにコンパイルされる。

- 言語名: **Risp** (Rust + Lisp)
- 拡張子: **`.rsp`**
- バックエンド: **LLVM** (inkwell)

## ゴール

- S式構文を採用したLisp系言語
- Rust由来の静的型システム（i32 / i64 / f32 / f64 / bool / str など）
- LLVMバックエンドによるネイティブコード生成
- 将来的にマクロ・struct・enum・trait/implを導入

## 現状（Phase 1: MVP 完了）

最小限「LLVMでネイティブコンパイルして実行できる」ところまで動作。

```sh
$ cargo risp run examples/hello.rsp
Hello, Risp!

$ cargo risp run examples/cmp_i64.rsp
ok: i64 cmp

$ cargo risp run examples/cmp_f64.rsp
ok: f64 cmp
```

生成物は本物の Mach-O / ELF 実行可能バイナリ（Rustランタイム不要）。

### サポートする型

| 型 | 説明 |
|---|---|
| `i32` | 32bit符号付き整数 |
| `i64` | 64bit符号付き整数 |
| `f32` | 32bit浮動小数点 |
| `f64` | 64bit浮動小数点 |
| `bool` | 真偽値 |
| `str` | 静的文字列（コンパイル時定数のみ。動的Stringは将来対応） |

### メモリ管理（MVP方針）

- MVPは**値型のみ**＋**静的文字列リテラル**で開始
- 文字列は LLVM のグローバル定数として埋め込む
- ヒープ動的データが必要になった時点で、Rc/Arc 方式 or Boehm GC 方式を判断
- 借用チェッカ（Rust的所有権）はMVPでは導入しない

### 構文

予約語・関数定義スタイルはLispに寄せる。型注釈は `name: type` 形式（Rust風）。

```lisp
;; コメントは ; から行末

;; 関数定義
(defn add [x: i32, y: i32] -> i32
  (+ x y))

;; トップレベル定数
(def PI: f64 3.14159)

;; ローカル束縛（複数束縛OK）
(let [x: i32 10, y: i32 20]
  (+ x y))

;; 条件分岐
(if (< x 0) (- x) x)

;; 数値キャスト
(as i64 x)
(as f64 n)

;; 逐次実行
(do
  (println "hello")
  (+ 1 2))

;; エントリポイント
(defn main [] -> i32
  (do
    (println "Hello, Risp!")
    0))
```

#### 字句要素

- 行コメント: `;` から行末まで
- 数値リテラル: `42`, `42i64`, `3.14`, `3.14f32`
- 文字列リテラル: `"hello"`（エスケープ: `\n` `\t` `\\` `\"`）
- 真偽値: `true`, `false`
- 識別子: `[a-zA-Z_][a-zA-Z0-9_-]*`（kebab-case可）
- 記号: `( ) [ ] : , ->`

#### 引数リスト・束縛リストは `[]`

Clojure風に、関数の仮引数と `let` の束縛は角括弧で囲む。

#### 型注釈

- 関数の引数・戻り値: **必須**
- `def` / `let`: **必須**（型推論はMVPでは入れない。後で拡張）

### 組み込み演算子・関数

| カテゴリ | トークン |
|---|---|
| 算術 | `+` `-` `*` `/` `mod`（`+`/`*`/`-` は n 項可。`(- x)` は単項マイナス。`/` `mod` は2項） |
| 比較 | `<` `<=` `>` `>=` `=` `!=` |
| 論理 | `and` `or` `not`（`and` / `or` は短絡評価） |
| キャスト | `(as T e)`（数値型間） |
| I/O | `print` `println`（`str` / 整数 / 浮動小数 / `bool`） |

### 評価戦略

正格評価（applicative order）。ただし `and` / `or` は短絡する（左が決まれば右を評価しない）。

自己末尾再帰は codegen でループに変換される（TCO）。末尾でない再帰や相互再帰は通常の呼び出しのまま。

## コンパイラパイプライン

```
.rsp
  → Lexer
  → Parser (S式)
  → AST
  → TypeChecker
  → LLVM IR (inkwell)
  → object file
  → 実行可能バイナリ (cc でリンク)
```

## CLI

ビルド済みバイナリ (`./target/debug/risp` または `cargo install --path .` 後の `risp`):

```sh
risp build hello.rsp        # ./hello を生成
risp run   hello.rsp        # ビルドして実行
risp emit-llvm hello.rsp    # LLVM IR を stdout に出力
risp emit-ast  hello.rsp    # AST を dump（デバッグ用）
```

開発中は `cargo risp` エイリアス（`.cargo/config.toml` で定義）が便利:

```sh
cargo risp run examples/hello.rsp
cargo risp emit-llvm examples/hello.rsp
```

## エラー表示

字句・構文・型エラーは、ソースの行・列とキャレットで表示される:

```
error: undefined variable "missing"
 --> examples/foo.rsp:3:14
  |
3 |     (println missing)
  |              ^^^^^^^
```

## テスト

```sh
cargo test                  # unit + e2e
cargo test --test examples  # e2e のみ（examples/*.rsp をビルド・実行・出力検証）
```

e2e テストは各 `examples/*.rsp` 先頭の `;;!` ヘッダで期待値を宣言する:

```lisp
;;! stdout: Hello, Risp!
;;! exit: 0
```

エラー診断は `examples/errors/*.rsp` で確認できる（コンパイルが失敗し、指定の行・列に診断が出ることを検証）:

```lisp
;;! error_at: 7:14
;;! error_contains: undefined variable
```

## 実装方針

- パーサ: 手書き（S式は単純なので外部クレート不要）
- LLVMバインディング: [`inkwell`](https://github.com/TheDan64/inkwell)
- CLI: `clap`

## 開発環境

Nix flake 同梱。LLVM 18 と Rust toolchain（rust-analyzer 含む）が入る。

```sh
nix develop          # devShellに入る
direnv allow         # direnv 利用時
cargo build          # コンパイラをビルド
cargo run -- run examples/hello.rsp
```

## ロードマップ

### Phase 1 — MVP ✅
- [x] Lexer / Parser
- [x] AST 定義
- [x] 型検査（プリミティブのみ）
- [x] LLVM IR 生成
- [x] `defn` / `def` / `let` / `if` / `do`
- [x] 算術・比較・論理演算
- [x] `println` (str)
- [x] ネイティブバイナリ出力
- [x] Hello World が動く

### Phase 2 — 短期改善
- [x] 比較演算子の型伝播バグ修正（i64/floatでも動くように）
- [x] ASTに型情報を埋め込む構造へリファクタ（`Expr.ty`）
- [x] エラー報告（行・列・ソース表示・キャレット）
- [x] e2e テスト（`examples/` を実行して期待出力と比較）

### Phase 3 — 言語機能
- [ ] 動的String（Rc or GC）
- [ ] struct / enum
- [ ] パターンマッチ（match）
- [ ] while / loop / break

### Phase 4 — 抽象化
- [ ] trait / impl
- [ ] ジェネリクス（モノモーフィゼーション）
- [ ] マクロ（defmacro）
- [ ] REPL（inkwell::execution_engine でJIT）

### Phase 5 — Nice to have
- [ ] 所有権・借用検査
- [ ] FFI（Cライブラリ呼び出し）
- [ ] モジュールシステム

## ライセンス

MIT
