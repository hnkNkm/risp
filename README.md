# Risp

Rustの型システムを持つLispライクなプログラミング言語。LLVM経由でネイティブバイナリにコンパイルされる。

- 言語名: **Risp** (Rust + Lisp)
- 拡張子: **`.rsp`**
- バックエンド: **LLVM** (inkwell)

## ゴール

- S式構文を採用したLisp系言語
- Rust由来の静的型システム（i32 / i64 / f32 / f64 / bool / str など）
- LLVMバックエンドによるネイティブコード生成
- 構文マクロ（`defmacro`、非衛生的な置換）と struct / enum / trait/impl / ジェネリクス単相化は MVP 済み

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
| `str` | 動的文字列（参照カウント / `runtime/risp_rt.c`） |
| `(Array T N)` | 固定長配列（要素は数値 / `bool`。ローカルのみ。代入は参照意味論） |
| 名前付き型 | `struct` / `enum`（フィールド・ペイロードは数値 / `bool` のみ） |

### メモリ管理

- 数値・`bool`・固定長配列・`struct` / `enum` は値 / スタック
- **`str` は参照カウント**（C ランタイム `risp_str_*`）。GC ではなく Rc 方針（将来の所有権検査へ寄せやすい）
- 借用チェッカ（Rust的所有権）は未導入（Phase 5）

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

;; 可変代入（ローカル / 仮引数のみ。`def` 定数には不可）
(set! x (+ x 1))

;; ループ
(while (< i n)
  (do
    (set! acc (+ acc i))
    (set! i (+ i 1))))

(loop
  (if done (break) (set! i (+ i 1))))

;; 固定長配列
(let [a: (Array i32 3) (array i32 1 2 3)]
  (do
    (aset! a 0 10)
    (+ (aget a 0) (alen a))))

;; 構造体
(struct Point [x: i32, y: i32])
(let [p: Point (Point 3 4)]
  (+ (field p x) (field p y)))

;; 列挙型 + パターンマッチ（網羅必須。unit / 単一ペイロード）
(enum Opt
  None
  Some i32)
(match o
  (None 0)
  (Some n n))

;; trait / impl（静的ディスパッチ。メソッド名は trait 間でグローバルに一意）
(trait Show
  (show [self] -> str))
(impl Show for i32
  (show [self] -> str
    "i32"))
(println (show 1))

;; ジェネリクス（型パラメータは値パラメータの前の `[]`。単相化）
(defn id [T] [x: T] -> T x)
(defn print-show [T: Show] [x: T] -> unit
  (println (show x)))
(print-show (id 1))

;; 構文マクロ（型検査前に展開。仮引数名をテンプレート内の Var に素朴置換）
(defmacro when [cond body]
  (if cond body (do)))
(when true (println "ok"))

;; モジュール（1ファイル = 1モジュール。任意の先頭 `(module name)`）
;; math.rsp:
;;   (module math)
;;   (defn add [x: i32, y: i32] -> i32 (+ x y))
(import math)
(math/add 2 3)

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
- 識別子: 英字 / `_` / 演算子文字で開始。継続に `/` を含める（修飾名 `math/add` は1トークン）
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
| 代入 | `(set! name value)`（ローカル / 仮引数。型は Unit） |
| ループ | `(while cond body)` / `(loop body)` / `(break)`（値は Unit） |
| 配列 | `(array T ...)` / `aget` / `aset!` / `alen`（関数の引数・戻り値には未対応） |
| ADT | `(struct Name [f: T ...])` / `(enum Name V ...)` / `(field e f)` / `(match e ...)` |
| trait | `(trait Name (method [self ...] -> T)*)` / `(impl Name for T ...)`（静的ディスパッチ。先頭引数は bare `self` 可） |
| ジェネリクス | `(defn f [T] [x: T] -> T ...)` / `(defn f [T: Trait] [x: T] -> ...)`（呼び出し時に単相化。struct/enum は未対応） |
| マクロ | `(defmacro name [params] template)`（型検査前に Call をテンプレートへ置換。非衛生的） |
| モジュール | `(module name)` / `(import name)`（同ディレクトリの `name.rsp` を読込。定義は `name/` 接頭辞。REPL では import 不可） |
| FFI | `(extern "C" name [params] -> ret)`（プリミティブ / `str`。`str` は C 文字列に変換） |
| 文字列 | `str-concat` / `str-len`（`str` は Rc） |
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
  → Resolve (import / module 接頭辞マージ)
  → MacroExpand (defmacro)
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
risp repl                   # 対話 REPL（LLVM JIT）
```

開発中は `cargo risp` エイリアス（`.cargo/config.toml` で定義）が便利:

```sh
cargo risp run examples/hello.rsp
cargo risp emit-llvm examples/hello.rsp
cargo risp repl
```

### REPL

```text
risp> (defn add [x: i32, y: i32] -> i32 (+ x y))
; ok
risp> (add 1 2)
3
risp> :quit
```

- `defn` / `def` / `defmacro` / `struct` / `enum` / `extern` / `trait` / `impl` はセッションに蓄積される（`:clear` で破棄、`:defs` で一覧）
- それ以外の式は JIT 評価して `println` する
- 括弧が閉じるまで複数行入力可

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
- [x] 動的 String（Rc ランタイム）
- [x] struct / enum（フィールド・ペイロードはプリミティブのみ）
- [x] パターンマッチ（match：unit / 単一ペイロード、網羅必須）
- [x] while / loop / break

### Phase 4 — 抽象化
- [x] trait / impl（静的ディスパッチ MVP。メソッド名は trait 間で一意）
- [x] ジェネリクス（`defn` の単相化 MVP。型パラメータ + trait bound）
- [x] マクロ（`defmacro`、非衛生的な構文置換 MVP）
- [x] REPL（inkwell::execution_engine でJIT）
- [x] モジュール（`(import name)` / 修飾名 `mod/f` MVP）

### Phase 5 — Nice to have
- [ ] 所有権・借用検査
- [x] FFI（`(extern "C" …)`）
- [ ] モジュール検索パスの拡張 / 再エクスポート

## ライセンス

MIT
