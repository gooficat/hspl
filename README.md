# hspl

[![CI](https://github.com/gooficat/hspl/actions/workflows/ci.yml/badge.svg)](https://github.com/gooficat/hspl/actions/workflows/ci.yml)

**Hasan's Simple Programming Language** — a small B-like language with six
primitive types that compiles to LLVM IR. It has no operators, but does have
`if` expressions, `jump`, and labels. Inline assembly blocks are the escape
hatch, and the assembly standard library is imported with `extern`.

```hspl
extern print(word) : word;

greeting : word 42;

main() : word print(greeting);
```

```console
$ cargo run -- examples/hello.hspl
wrote a.out
$ ./a.out
42
```

See [`examples/asm.hspl`](examples/asm.hspl) for inline assembly,
[`examples/types.hspl`](examples/types.hspl) for the type system, and the
[examples table](#examples) for everything else.

## Design

HSPL is deliberately stripped down, in the spirit of B:

- **Six types.** Five value types — `word` (64-bit, the default for value
  annotations), `int` (32-bit), `short` (16-bit), `byte` (8-bit), and `bool`
  (1-bit) — plus `void`, which is the default return type and the only
  permitted return-only type. Annotations are written *after* names:
  `f(x : int) : byte`.
- **No operators.** `+`, `-`, `*`, `/`, `%`, comparisons, `&&`, `||`, `!` are
  lexer errors. Assignment is a *statement*, not an expression.
- **Minimal control flow.** `if` is an expression, `jump` transfers to a
  function-scoped label, and backward jumps build loops. There is no `while`,
  `for`, or `switch` yet.
- **No return statements.** A function body is exactly **one statement**, and
  the function returns the value of that statement. Multi-statement bodies use a
  Rust-style statement expression.
- **Widening only.** Booleans zero-extend to integers, narrower integers
  sign-extend to wider ones, and narrowing is a compile error. There are no
  casts yet.
- **Inline assembly** is the escape hatch for arithmetic and anything the
  high-level language cannot express yet.
- **No implicit runtime.** Anything beyond the language itself — including
  printing — is imported with `extern` and supplied by the standard library.

`auto`, `asm`, `extern`, `if`, `else`, `jump`, `label`, `true`, and `false` are
the only keywords. Type names are recognized where a type is expected, so
everything else — including words like `while` and `return` — is an ordinary
identifier.

## Types

Types come after the name they describe.

```hspl
answer : word 42;                 // global
f(x : byte, y : short) : int {    // parameters and return type
    auto n : word;                // local
    n
}
```

| Type | LLVM | Bits | Literal range |
| --- | --- | --- | --- |
| `word` | `i64` | 64 | every 64-bit value |
| `int` | `i32` | 32 | -2³¹ .. 2³²-1 |
| `short` | `i16` | 16 | -2¹⁵ .. 2¹⁶-1 |
| `byte` | `i8` | 8 | -2⁷ .. 2⁸-1 |
| `bool` | `i1` | 1 | `0`, `1`, `true`, `false` |
| `void` | — | — | no value; the default return type |

A missing value-type annotation (parameters, locals, globals) means `word`; a
missing return-type annotation means `void`. Integer literals *adopt* the type required by
context, then are range-checked. Both signed and unsigned spellings are accepted
for a width, so `byte 255` stores `-1` and `int 0xFFFFFFFF` stores `-1`. Since
there is no unary minus, negative values are normally written in
two's-complement hex (`byte 0x80` is `-128`).

`void` produces no value: a `void` function is called as a statement and its
result cannot be used in an expression. `void` cannot be used for locals,
parameters, or globals — only as a function or `extern` return type — and it
is the default when a function or `extern` omits its return type. A `void`
`main` is wrapped by the C-ABI `i32 main()` and exits with status `0`.

Conversions are implicit only when widening:

- `bool` → any integer (zero-extend)
- `byte` → `short` → `int` → `word` (sign-extend)

Anything else — including narrowing, or converting between types of the same
width — is rejected:

```hspl
f(x : word) : byte x;   // error: cannot implicitly convert `word` to `byte`
```

## Standard library

The standard library lives in [`std/print.s`](std/print.s) and is written in
x86-64 assembly. It currently provides:

| Symbol | Signature | Behavior |
| --- | --- | --- |
| `print` | `print(word) : word` | Writes the signed decimal word and a newline to stdout |

It is implemented with the Linux `write(2)` syscall (no libc calls), and is
embedded into the compiler at build time. The driver writes it to a temporary
file and passes it to the linker, so **every executable is linked against it
automatically**.

There is no builtin `print`: a program must import the symbol explicitly.

```hspl
extern print(word) : word;

main() : word print(42);
```

Calling `print` without the declaration is a compile error that suggests the
missing `extern`. Since the library always defines the symbol, defining a
function or global named `print` is also rejected.

## Language

### Items

A program is a sequence of extern declarations, global definitions, and
functions.

```hspl
extern print(word) : word;       // typed import
extern tick() : int;             // no parameters, int result

answer : word 42;                // global of type word
zero : byte;                     // global zero-initialised byte
plain 7;                         // unannotated: word

add(a : word, b : word) : word asm {   // typed function
    movq $a, $out
    addq $b, $out
}
```

**Externs** declare function symbols defined in another translation unit. They
are fully typed: parameter types (without names) and an optional return type
that defaults to `void`. Calls are arity- and type-checked, and arguments are
widened as needed. The standard library is linked automatically; other symbols
must be supplied by whatever you link alongside the generated IR.

**Globals** are a name, an optional `: type`, an optional literal initializer,
and `;`. The initializer may be an integer or `true`/`false`, and is
range-checked against the global's type. Globals are assignable.

**Functions** are a name, a parenthesised typed parameter list, an optional
`: type` return annotation, and exactly one statement. The return type defaults
to `void`.

### Statements

| Form | Meaning |
| --- | --- |
| `auto a : int, b;` | Declare locals (missing annotations mean `word`), zero-initialised |
| `name = expr;` | Assignment, converted to the variable's type if widening |
| `jump name;` | Unconditional jump to a function-scoped label |
| `name:` / `label name;` | Define a jump target |
| `expr;` | Expression statement |
| `{ ... }` | Block / statement expression |
| `;` | Empty statement |

### Expressions

| Form | Meaning |
| --- | --- |
| `42`, `0x2a` | Integer literal; adopts the expected type and is range-checked |
| `true`, `false` | Boolean literal |
| `name` | Variable or global |
| `f(a, b)` | Function call |
| `if cond expr else expr` | Conditional expression; braces are optional |
| `asm { ... }` | Inline assembly block |
| `{ ... }` | Statement expression |

That is the entire expression language — there are no operators to build from.

### Statement expressions

A block yields the value of its trailing expression. Statements before it are
evaluated for their effects. A block with no trailing expression yields zero of
the type expected by context.

```hspl
use_block(x : word) : word {
    auto y : word;
    y = x;
    y          // trailing expression: the block's value
}
```

Because blocks are expressions, they compose — here a statement expression is
passed directly as an argument:

```hspl
print({ auto n : word; n = 21; double(n) });
```

A function whose body is a block returns the block's trailing value; a function
whose body is a plain expression statement returns that expression's value. A
block without a tail, or an empty statement, yields zero of the expected type.

### Control flow

`if` is an expression:

```hspl
choose(flag : bool) : int if flag { 42 } else { 7 };
```

- It yields the value of the taken branch. Without `else`, the false path
  yields zero of the expected type.
- The condition may be `bool`, or any integer, which is true when non-zero.
- Both `if cond ...` and `if (cond) ...` are accepted.
- Each branch is an expression, so braces are optional: `{ ... }` is just a
  statement expression.

  ```hspl
  choose(flag : bool) : int if flag 42 else 7;
  ```

  `if cond jump label;` is also accepted as a conditional jump.

`jump name;` transfers control to a label in the same function. Labels may be
written `name:` or `label name;` / `label name:` and are function-scoped:

```hspl
countdown(n : word) : word {
    auto i : word;
    auto steps : word;
    i = n;
    steps = 0;

    top:
    if i {
        i = decrement(i);
        steps = increment(steps);
        jump top;
    }
    steps
}
```

A jump can leave a block or enter one. Locals are allocated once in the
function's entry block and zero-initialised, so a jump never reaches an
uninitialised slot; a declaration inside a loop still resets on each iteration.
Statements after a jump are unreachable but are still parsed and checked.

### Inline assembly

`asm { ... }` is an expression whose result type comes from context — the
function return type, a parameter type, or the variable being assigned — and
defaults to `word` (in statement position an `asm` block is evaluated for its
side effects and its result is discarded). Its body is raw assembly for the host target (LLVM's
inline-asm parser, AT&T dialect by default). Input operands keep their
variables' types.

```hspl
add(a : word, b : word) : word asm {
    movq $a, $out
    addq $b, $out
}
```

Placeholders:

- `$out` — the block's **output** operand (constraint `=r`); this is the value
  the block produces.
- `$name` — an **input** operand (constraint `r`), loaded from the variable or
  global `name`. Repeating a placeholder reuses the same operand.
- `$$` — an escaped dollar, for literals that need one. AT&T immediates are
  written `$$42`:

  ```hspl
  answer() : word asm {
      movq $$42, $out
  }
  ```

- `$0`, `$1`, ... — explicit LLVM operand numbers also pass through.

Operands are numbered output-first: `$out` becomes `$0`, and inputs become `$1`,
`$2`, ... in first-use order. The example above lowers to:

```llvm
%asm = call i64 asm sideeffect "movq $1, $0\0A  addq $2, $0", "=r,r,r"(i64 %a, i64 %b)
```

Assembly blocks are volatile (`sideeffect`). Clobber lists and multiple outputs
are not supported yet, so instructions with implicit register operands need care.

### `main`

The compiler emits the user's `main` as `main.hspl` and adds a C-ABI
`i32 main()` wrapper that converts the result to an exit status: `word` is
truncated, `short`/`byte` are sign-extended, and `bool` is zero-extended. A
`void` `main` — the default when no return type is given — exits with status
`0`.

```hspl
main() : word 0;
```

```llvm
define i32 @main() {
  %call = call i64 @main.hspl()
  %trunc = trunc i64 %call to i32
  ret i32 %trunc
}
```

## Examples

Each example is self-contained and covers a slice of the language:

| File | Demonstrates | Output |
| --- | --- | --- |
| [`examples/hello.hspl`](examples/hello.hspl) | Typed `extern`, a global, a call | `42` |
| [`examples/words.hspl`](examples/words.hspl) | Zero-initialised globals, hex and two's-complement words | `0`, `-1`, `9223372036854775807`, `42` |
| [`examples/asm.hspl`](examples/asm.hspl) | Asm inputs/outputs, `$$` immediates, explicit `$0`, word and byte operands | `42`, `42`, `-7`, `42`, `42` |
| [`examples/scopes.hspl`](examples/scopes.hspl) | Typed `auto`, assignment, mutable globals, shadowing, widening at a call | `41`, `3`, `7`, `42`, `7` |
| [`examples/functions.hspl`](examples/functions.hspl) | Empty statement, expression bodies, empty body, block bodies, nested calls and blocks, calling a function defined later | `7`, `0`, `42`, `9`, `42`, `42`, `42` |
| [`examples/types.hspl`](examples/types.hspl) | Every primitive, defaults, literal ranges, `bool` literals, all widening conversions, typed asm, zero-valued blocks | `9223372036854775807`, `42`, `1000`, `-56`, `1`, `0`, `7`, `127`, `-128`, `-1`, `1`, `0`, `1`, `127`, `10`, `42`, `0` (exit `42`) |
| [`examples/control.hspl`](examples/control.hspl) | `if` as an expression, integer conditions, `else`, conditional jumps, labels, backward-jump loops, unreachable code | `42`, `7`, `0`, `1`, `5`, `0`, `222`, `11` |
| [`examples/void.hspl`](examples/void.hspl) | `void` functions and the `void` default return type | `5`, `42`, `7`, `9` (exit `0`) |

Run them all:

```sh
for example in examples/*.hspl; do
    cargo run -- "$example" -o /tmp/example
    /tmp/example
done
```

## Install

Prebuilt binaries for Linux x86-64 are attached to every
[GitHub release](https://github.com/gooficat/hspl/releases). Download
`hspl-*.tar.gz`, verify it against the published `.sha256`, unpack, and put
`hspl` on your `PATH`.

Or build from source (requires LLVM 22, Rust 1.85+, and clang):

```sh
cargo install --git https://github.com/gooficat/hspl
```

## Requirements

- Rust 1.85+ (edition 2024)
- **LLVM 22** with `llvm-config` on `PATH` (Inkwell's `llvm22-1` feature)
- **clang** for linking (or set `HSPL_CC` to another compiler driver with a
  compatible interface)

On Fedora:

```sh
sudo dnf install llvm22-devel clang
```

Non-standard LLVM prefix:

```sh
export LLVM_SYS_221_PREFIX=/usr/lib64/llvm22
```

## CLI

```sh
hspl <input.hspl> [-o <output>] [--emit-ir]
```

- By default the driver compiles to IR, links it with the embedded standard
  library, and writes an executable (`a.out` unless `-o` is given).
- `--emit-ir` writes textual LLVM IR instead (default `<input>.ll`).
- `HSPL_CC` overrides the linker driver (default `clang`).

```sh
cargo run -- examples/hello.hspl -o hello
./hello

cargo run -- examples/hello.hspl --emit-ir -o hello.ll
```

The IR is host-triple tagged, so it can also be inspected and linked by hand:

```sh
llc hello.ll -o hello.s
clang hello.ll std/print.s -o hello
```

## Project layout

```
src/
  main.rs      CLI driver: compile, link with the standard library
  lib.rs       Public pipeline API (parse_source, compile_to_ir)
  lexer.rs     Tokenizer; captures `asm { ... }` bodies raw
  parser.rs    Recursive-descent parser (no precedence table needed!)
  ast.rs       AST definitions and the `Type` enum
  codegen.rs   LLVM IR generation (Inkwell), including widening rules
std/
  print.s      Standard library, written in x86-64 assembly
examples/
  hello.hspl       extern + typed globals
  words.hspl       word literals and zero-initialised globals
  asm.hspl         inline assembly operands, including byte-sized ones
  scopes.hspl      locals, assignment, shadowing, widening
  functions.hspl   one-statement bodies and statement expressions
  types.hspl       all primitive types and conversions
  control.hspl     if, jump, labels, and loops
  void.hspl        void functions, the void default return type, and void main
.github/workflows/
  ci.yml     GitHub Actions: build, test, and compile every example
  release.yml  On a `v*` tag: build the Linux x86-64 binary and publish a GitHub release
```

Pipeline:

```
source -> lexer -> parser -> AST -> codegen -> LLVM IR
                                                    |
                                   std/print.s  -> clang -> executable
```

## Development

```sh
cargo test
cargo run -- examples/types.hspl -o /tmp/types && /tmp/types
```

CI runs on every push and pull request: it installs LLVM 22, runs the test
suite, builds the release binary, and compiles every example.

Inspect the stages:

```sh
cargo run -- examples/types.hspl --emit-ir -o /tmp/types.ll
opt -passes=mem2reg -S /tmp/types.ll
llc /tmp/types.ll -o /tmp/types.s
```

`std/print.s` is embedded with `include_str!`, so editing it requires rebuilding
the compiler.

## Roadmap

- `while` and `for` sugar built on labels and jumps
- Explicit casts, so narrowing can be written down
- **More standard library**: `putchar`, word input, and allocation, all in
  assembly
- `extern` data symbols (not just functions)
- Assembly clobber lists, multiple outputs, and typed constraints
- Proper calling-convention support for external C functions
- Strings/byte data and address-of support
- Optimization pipeline and object-file emission
- Source spans and better diagnostics

## License

MIT — see [LICENSE](LICENSE).
