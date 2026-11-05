# hspl

[![CI](https://github.com/gooficat/hspl/actions/workflows/ci.yml/badge.svg)](https://github.com/gooficat/hspl/actions/workflows/ci.yml)

**Hasan's Simple Programming Language** — a small, B-like systems language
that compiles to LLVM IR. HSPL has eight primitive types plus named struct
types, a small C-like operator set (arithmetic, bitwise, and reference
operators), control flow via `if` expressions and labels, and no `return`
statements: every function body is exactly one statement whose value is the
function's result. Programs link against a tiny assembly standard library and
produce native executables.

```hspl
extern print(word) : word;

main() : word print(42);
```

```console
$ cargo run -- examples/hello.hspl
wrote a.out
$ ./a.out
42
```

## Contents

- [Install](#install)
- [Quick start](#quick-start)
- [Why HSPL is shaped the way it is](#why-hspl-is-shaped-the-way-it-is)
- [The language](#the-language)
  - [Program structure](#program-structure)
  - [Types and conversions](#types-and-conversions)
  - [Items: externs, globals, functions, structs](#items-externs-globals-functions-structs)
  - [Statements](#statements)
  - [Expressions](#expressions)
  - [Statement expressions](#statement-expressions)
  - [Control flow](#control-flow)
  - [Inline assembly](#inline-assembly)
  - [`main` and the C ABI](#main-and-the-c-abi)
- [Standard library](#standard-library)
- [Command-line interface](#command-line-interface)
- [Examples](#examples)
- [Building from source](#building-from-source)
- [Development](#development)
- [Troubleshooting](#troubleshooting)
- [Roadmap](#roadmap)
- [License](#license)

## Install

**Prebuilt binaries** for Linux x86-64 are attached to every
[GitHub release](https://github.com/gooficat/hspl/releases). Download
`hspl-*.tar.gz`, verify it against the published `.sha256`, unpack, and put
`hspl` on your `PATH`.

**From source** (requires Rust 1.85+, LLVM 22, and clang — see
[Building from source](#building-from-source)):

```sh
cargo install --git https://github.com/gooficat/hspl
```

## Quick start

Write `hello.hspl`:

```hspl
extern print(word) : word;   // the standard library's symbol, typed

greeting : word 42;          // a global of type word, initialised to 42

main() : word print(greeting);
```

Compile and run it:

```console
$ hspl hello.hspl -o hello
wrote hello
$ ./hello
42
```

Every example in [`examples/`](examples) is self-contained and prints a small,
known output; [the examples table](#examples) lists what each one covers.

## Why HSPL is shaped the way it is

HSPL is deliberately stripped down, in the spirit of B:

- **No comparison or logical operators.** Comparisons (`==`, `<`, ...), the
  logical operators (`&&`, `||`, `!`) and casts are not part of the language
  yet. Assignment is a statement, not an expression. Arithmetic
  (`+ - * / %`), bitwise (`& | ^ ~ << >>`) and reference (`&` address-of, `*`
  dereference) operations are expressions; anything else still has to be
  expressed through inline assembly or in called functions.
- **Structs, but only structs.** Structs are named, never anonymous, never
  declared inline, and there are no generics or `impl` blocks. They are
  declared at the top level with `name : struct { field : type, ... };`,
  values are created by declaring a zero-initialised variable or global and
  assigning fields, and fields are read and written with postfix `.`.
- **No `return` statements.** A function body is exactly **one statement**,
  and the function returns the value of that statement. Multi-statement bodies
  are written as Rust-style statement expressions.
- **Explicit conversions only where they widen.** `bool` zero-extends to any
  integer or float; narrower integers sign-extend to wider ones; `float`
  widens to `double`; an integer widens to a float type only when exactly
  representable. Narrowing is a compile error, and there are no casts yet.
- **Minimal control flow.** `if` is an expression; `jump` transfers to a
  function-scoped label; backward jumps build loops. There is no `while`,
  `for`, or `switch` yet.
- **No implicit runtime.** Anything beyond the language itself — including
  printing — is imported with `extern` and supplied by the standard library,
  which is written in x86-64 assembly and linked automatically.
- **Inline assembly is the escape hatch.** Anything the high-level language
  cannot express yet, it can express in `asm { ... }` blocks. Arithmetic,
  bitwise, and reference operations no longer need it; it remains for machine
  operations the operator set does not cover.

The only keywords are `auto`, `asm`, `extern`, `if`, `else`, `jump`, `label`,
`true`, and `false`. Type names — including the names of structs you have
declared — are recognized where a type is expected, so everything else,
including words like `while` and `return`, is an ordinary identifier.

## The language

### Program structure

A program is a sequence of *items*: `extern` declarations, global definitions,
functions, and struct declarations. Items may appear in any order, but a struct
can only mention structs declared earlier in the same file.

```hspl
extern print(word) : word;    // typed import from another translation unit
extern tick() : int;          // no parameters, int result

point : struct { x : word, y : word };   // a named struct type
offset : struct { base : point, dx : word };

answer : word 42;             // a global of type word with an initializer
zero : byte;                  // a global with no initializer: zeroed on load
plain 7;                      // no annotation on a global means word
origin : point;               // struct globals are zero-initialised

add(a : word, b : word) : word asm {  // a function
    movq $a, $out
    addq $b, $out
}
```

An informal grammar:

```text
program   := item*
item      := 'extern' ident '(' [type (',' type)*] ')' [':' type] ';'   // import
           | ident '(' [binding (',' binding)*] ')' [':' type] statement // function
           | ident [':' type] [integer | 'true' | 'false'] ';'           // global
           | ident ':' 'struct' '{' field (',' field)* [','] '}' ';'      // struct

binding   := ident [':' type]
field     := binding

statement := 'auto' binding (',' binding)* ';'
           | assign_path '=' expression ';'    // assign_path is ident ('.' ident)*
           | 'jump' ident ';'                  // goto
           | ident ':'                         // label
           | 'label' ident (';' | ':')         // label, keyword form
           | expression ';'
           | '{' statement* [expression] '}'   // block, as a statement or tail
           | ';'                              // empty statement
           | expression                        // block-like expression, no ';'

expression := integer | float | 'true' | 'false'
           | ident | ident '(' [expression (',' expression)*] ')'
           | expression '.' ident                // field access
           | 'asm' '{' raw assembly '}'
           | 'if' ['('] expression [')'] expression ['else' expression]
           | '{' statement* [expression] '}'
           | unary_op expression
           | expression binary_op expression

unary_op  := '-' | '~' | '&' | '*'
binary_op := '|' | '^' | '&' | '<<' | '>>' | '+' | '-' | '*' | '/' | '%'
```

Precedence, loosest to tightest: `|` < `^` < `&` < shifts < `+`/`-` <
`*`/`/`/`%` < unary. Binary operators are left-associative.

Annotations are always written *after* the name they describe:
`f(x : int) : byte`, `y : word`, `answer : word 42`. A missing **value-type**
annotation (on a parameter, local, or global) means `word`; a missing
**return-type** annotation (on a function or `extern`) means `void`.

### Types and conversions

HSPL has eight primitive types plus named struct types. Types come after the
name they describe.

```hspl
answer : word 42;                // global
f(x : byte, y : short) : int {   // parameters and return type
    auto n : word;               // local
    n
}
```

| Type   | LLVM   | Bits | Literal range                     |
| ------ | ------ | ---- | --------------------------------- |
| `word` | `i64`  | 64   | every 64-bit value                |
| `int`  | `i32`  | 32   | -2³¹ .. 2³²-1                     |
| `short`| `i16`  | 16   | -2¹⁵ .. 2¹⁶-1                     |
| `byte` | `i8`   | 8    | -2⁷ .. 2⁸-1                       |
| `bool` | `i1`   | 1    | `0`, `1`, `true`, `false`         |
| `float`| `f32`  | 32   | any finite IEEE-754 single        |
| `double`| `f64` | 64   | any finite IEEE-754 double        |
| `void` | —      | —    | no value; the default return type |
| `point` | `%point = type { i64, i64 }` in the IR | — | a named struct type: `point : struct { x : word, y : word };` |

**Integer literals** adopt the type required by context — a literal is never
inferred in isolation — and are then range-checked. Both the signed and the
unsigned spelling of a width's range are accepted, so `byte 255` stores `-1`
and `int 0xFFFFFFFF` stores `-1`. Hex literals wrap to 64 bits, which is how
the full range of `word` is written. A unary minus operator exists, so a
negative literal such as `-128` written against a `byte` context is accepted;
values are stored in the type's width as usual.

**Floating-point literals** are decimal, with an optional fraction and an
optional exponent: `1.5`, `1.`, `0.5`, `1e10`, `1.5E-3`, `2e+5`. A literal must
start with a digit (`.5` is not accepted). Values are parsed with correct
rounding — the nearest representable value, round to nearest even — so the
stored constant is exactly what was written; `0.1` stores the exact f64 nearest
0.1. Hex literals are integers only; hexadecimal float literals are on the
[roadmap](#roadmap). A floating-point literal adopts its expected type and is
then range-checked, exactly like an integer literal: it defaults to `double`,
is rejected in `float` context when the value is out of range or not finite
(e.g. `1e999`), and cannot be used as an integer. An integer literal adopts a
floating-point context only when exactly representable: `f() : double 2;` is
fine; `f() : double 9007199254740993;` is an error, because 2⁵³+1 is not
representable.

**`void`** produces no value. A `void` function is called as a statement and
its result cannot be used in an expression. `void` is only valid as a function
or `extern` return type — never for locals, parameters, globals, or struct
fields — and it is the default when a function or `extern` omits its return
type. A `void` `main` is wrapped by the C-ABI `i32 main()` and exits with
status `0`.

**Conversions** are implicit only when widening:

- `bool` → any integer (zero-extend) or float/double (exact `0.0`/`1.0`)
- `byte` → `short` → `int` → `word` (sign-extend)
- `float` → `double` (widen)
- `byte` → `float`, `short` → `float` (exactly representable)
- `byte` → `double`, `short` → `double`, `int` → `double` (exactly representable)
- struct → same struct (identity, how a field is assigned)

Anything else is rejected, including narrowing, same-width conversions, any
float-to-integer conversion, and any conversion to or from a struct (a struct
value can only flow to a `struct` of the same name, e.g. assignment; an
explicit conversion does not exist yet):

```hspl
f(x : word) : byte x;   // error: cannot implicitly convert `word` to `byte`
```

### Items: externs, globals, functions, structs

**Externs** declare symbols defined in another translation unit. They are
fully typed: parameter types (without names) and an optional return type,
which defaults to `void`. Calls are arity- and type-checked and arguments are
widened as needed. Symbols other than the standard library's must be supplied
by whatever you link alongside the generated IR. `extern` signatures cannot
mention struct types.

**Structs** are declared at the top level, before being used. The name becomes
the type used in later annotations (`auto p : point;`, `f(p : point)`, return
types). A struct may only mention structs declared earlier in the same file:

```hspl
point : struct { x : word, y : word };
offset : struct { base : point, dx : word };
```

**Globals** are a name, an optional `: type`, an optional literal initializer,
and `;`. The initializer may be an integer or `true`/`false` and is
range-checked against the global's type. Globals without an initializer are
zeroed — struct globals become the all-zero struct (`@origin = global %point
zeroinitializer`). Globals are assignable, like locals.

**Structs in memory** are packed as laid out of their fields of ordinary LLVM
types, so `point : struct { x : word, y : word };` is `{ i64, i64 }`. Fields are
addressed with `getelementptr` through the named (never anonymous) struct type.

**Functions** are a name, a parenthesised typed parameter list, an optional
`: type` return annotation (which defaults to `void`), and exactly one
statement. The function's result is the value of that statement:

```hspl
identity(x : word) : word x;       // expression body

nothing() { }                      // no return annotation: void, empty body

constant() : word 42;

add(p : point, q : point) : point {   // struct parameters pass by value
    auto out : point;
    out.x = p.x + q.x;
    out.y = p.y + q.y;
    out
}
```

Functions and globals are assignable as expected, but a function is not a
first-class value: it cannot be stored or passed as an argument yet.

### Statements

| Form                       | Meaning                                                              |
| -------------------------- | ------------------------------------------------------------------- |
| `auto a : int, b;`         | Declare locals (missing annotations mean `word`), zero-initialised    |
| `path = expr;`             | Assignment to a variable or struct field (`p.x = 5;`), converted to the target's type if widening |
| `jump name;`               | Unconditional jump to a function-scoped label                         |
| `name:` / `label name;`    | Define a jump target                                                  |
| `expr;`                    | Expression statement                                                  |
| `{ ... }`                  | Block (a statement expression) used as a statement                    |
| `;`                        | Empty statement; yields zero of the expected type                     |

`auto` may declare several bindings in one statement, each with its own
annotation; the initializer, if any, is given by `;` or by the enclosing
context. Locals are scoped to the block in which they are declared; a nested
block may shadow an outer local. Declaring the same name twice in one block is
an error.

`path` on the left of an assignment is either a single variable name (`v = 1;`)
or one or more dotted segments (`p.x = 1;`, `s.base.inner.x = 42;`). The base of
the path must resolve to a variable or a field access on one; anything else is a
compile error. `p.x = 1;` is not sugar for anything — it stores through a GEP to
the field of the variable's slot.

### Expressions

| Form                        | Meaning                                                  |
| --------------------------- | -------------------------------------------------------- |
| `42`, `0x2a`                | Integer literal; adopts the expected type, range-checked  |
| `1.5`, `1e10`               | Floating-point literal; adopts the expected type (`double` by default), range-checked |
| `true`, `false`             | Boolean literal                                          |
| `name`                      | Variable or global                                        |
| `name.field`, `a.b.c`       | Field access on a struct value (chaining works)           |
| `f(a, b)`                   | Function call                                             |
| `if cond expr else expr`    | Conditional expression; braces are optional               |
| `asm { ... }`               | Inline assembly block                                     |
| `{ ... }`                   | Statement expression                                      |
| `-x`, `~x`                  | Unary operations: negate, bitwise not                    |
| `a + b`, `a - b`, `a * b`, `a / b`, `a % b` | Arithmetic                     |
| `a & b`, `a \| b`, `a ^ b`, `a << b`, `a >> b` | Bitwise and shifts        |
| `&x`, `*x`                  | Reference: address-of a `word`, dereference it         |

Binary operators combine two operands; unary operators prefix one. Precedence,
from loosest to tightest, is `|` < `^` < `&` < shifts < `+`/`-` < `*`/`/`/`%` <
unary. For example, `2 + 3 * 4` is `14`, and `100 - 10 - 5` is `85`
(left-associative).

The operations run on the usual types: arithmetic and bitwise operations on
all the integer types (`word`, `int`, `short`, `byte`) and on `float`/`double`
(floating point); reference operations on `word`. The boolean type is not an
arithmetic type; structs are never operands to an operator (operate on their
fields instead). Everything else produces a compile error.

That is the rest of the expression language. There are no comparison or
logical operators yet, so conditions are integer, floating-point, or boolean
values compared against zero by `if`.

Reference operations are intentionally narrow: `&x` takes the address of a
`word` variable or global, and `*p` loads through a `word` holding an address.
The result of address-of is a plain `word` — it is not a typed pointer, and
`*p` always loads a `word`.

### Statement expressions

A block evaluates its statements for their effects and yields the value of its
trailing expression, or zero of the expected type when there is no trailing
expression.

```hspl
use_block(x : word) : word {
    auto y : word;
    y = x;
    y            // trailing expression: the block's value
}
```

Because blocks are expressions, they compose — one can be passed directly as an
argument:

```hspl
double(n : word) : word n + n;

extern print(word) : word;

main() : word print({ auto n : word; n = 21; double(n) });
```

The body of a function is a single statement, so a block body both computes the
function's effects and yields its result. A function whose body is a plain
expression statement returns that expression's value.

### Control flow

`if` is an expression:

```hspl
choose(flag : bool) : int if flag { 42 } else { 7 };
```

- It yields the value of the taken branch. Without `else`, the false path
  yields zero of the expected type.
- The condition may be `bool`, or any integer, which is true when non-zero; a floating-point condition is true when it is not zero (NaN is false, as in IEEE-754 comparisons). A struct is rejected as a condition.
- Both `if cond ...` and `if (cond) ...` are accepted.
- Each branch is an expression, and `{ ... }` is itself an expression, so
  braces are optional: `choose(flag) : int if flag 42 else 7;`
- A bare `jump label;` is also accepted as a branch, which makes
  `if cond jump label;` a conditional jump.
- `if` expressions take their type from context (the function's return type,
  an assignment target, or a call argument), exactly like literals.

`jump name;` transfers control to a label in the same function. Labels may be
written `name:` or `label name;` (also `label name:`) and are function-scoped:

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

A jump may leave a block or enter one. Locals are allocated once in the
function's entry block and zero-initialised there, so a jump never reaches an
uninitialised slot; a declaration inside a loop still resets on each
iteration. Statements following a jump are unreachable but are still parsed
and checked. A jump to a label that does not exist anywhere in the function is
an error.

### Inline assembly

`asm { ... }` is an expression whose result type comes from context — the
function return type, a parameter type, or the variable being assigned — and
defaults to `word`. In statement position it is evaluated for its side effects
and its result is discarded. The body is raw assembly for the host target,
parsed by LLVM's inline-asm parser (AT&T dialect by default); input operands
keep their variables' types. Floating-point operands use the `x` constraint
(an SSE register on x86-64); integer operands use `r`. Structs cannot be
operands — load their fields into variables instead.

```hspl
add(a : word, b : word) : word asm {
    movq $a, $out
    addq $b, $out
}
```

Placeholders:

- `$out` — the block's **output** operand (constraint `=r` for integers,
  `=x` for floats); the value the block produces.
- `$name` — an **input** operand (constraint `r` for integers, `x` for
  floats), loaded from the variable or global `name`. Repeating a placeholder
  reuses the same operand.
- `$$` — an escaped dollar, for literals that need one. AT&T immediates are
  written `$$42`:

  ```hspl
  answer() : word asm {
      movq $$42, $out
  }
  ```

- `$0`, `$1`, ... — explicit LLVM operand numbers, passed through unchanged.

Operands are numbered output-first: `$out` becomes `$0`, and inputs become
`$1`, `$2`, ... in first-use order. The example above lowers to:

```llvm
%asm = call i64 asm sideeffect "movq $1, $0\0A  addq $2, $0", "=r,r,r"(i64 %a, i64 %b)
```

Assembly blocks are volatile (`sideeffect`). Clobber lists and multiple
outputs are not supported yet, so instructions with implicit register operands
need care.

### `main` and the C ABI

The compiler emits the user's `main` under a distinct symbol (`main.hspl`) and
adds a C-ABI `i32 main()` wrapper that converts the result to the process
exit status: `word` is truncated, `short`/`byte` are sign-extended, and
`bool` is zero-extended. A `void` `main` — the default when no return type is
given — exits with status `0`.

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

`main` cannot take parameters, and cannot return `float`, `double`, or a
struct — use an integer type (or omit the annotation for `void`).

## Standard library

The standard library lives in [`std/print.s`](std/print.s) and is written in
x86-64 assembly (System V ABI). It is embedded into the compiler at build
time and linked into every executable the driver produces — you never pass it
to the linker yourself. It is implemented with the Linux `write(2)` syscall
and calls no libc.

| Symbol  | Signature           | Behavior                                              |
| ------- | ------------------- | ----------------------------------------------------- |
| `print` | `print(word) : word` | Writes the signed decimal word and a newline to stdout; returns `0` |

There is no builtin `print`: a program must import the symbol explicitly.

```hspl
extern print(word) : word;

main() : word print(42);
```

Calling `print` without the declaration is a compile error that suggests the
missing `extern`. Because the library always defines the symbol, defining a
function or global named `print` is also rejected — as is naming a struct
`print`.

Other symbols must be supplied by whatever you link alongside the generated
IR. Extern data symbols, richer I/O, and allocation are on the
[roadmap](#roadmap).

## Command-line interface

```sh
hspl <input.hspl> [-o <output>] [--emit-ir]
```

- By default the driver compiles to IR, links it with the embedded standard
  library, and writes an executable (`a.out` unless `-o` is given).
- `--emit-ir` writes textual LLVM IR instead of linking (default `<input>.ll`).
- `-h`, `--help` and `-v`, `--version` print help and the version.
- `HSPL_CC` overrides the linker driver (default: `clang`).

```console
$ hspl examples/hello.hspl -o hello
wrote hello
$ ./hello
42

$ hspl examples/hello.hspl --emit-ir -o hello.ll
wrote hello.ll
```

The IR is host-triple tagged, so it can also be inspected and linked by hand:

```sh
llc hello.ll -o hello.s
clang hello.ll std/print.s -o hello
```

## Examples

Each example is self-contained and covers a slice of the language.

| File | Demonstrates | Output |
| ---- | ------------ | ------ |
| [`examples/hello.hspl`](examples/hello.hspl) | Typed `extern`, a global, a call | `42` |
| [`examples/words.hspl`](examples/words.hspl) | Zero-initialised globals, hex and two's-complement words | `0`, `-1`, `9223372036854775807`, `42` |
| [`examples/asm.hspl`](examples/asm.hspl) | Asm inputs/outputs, `$$` immediates, explicit `$0`, word and byte operands | `42`, `42`, `-7`, `42`, `42` |
| [`examples/floats.hspl`](examples/floats.hspl) | `float`/`double`, float literals, exact integer-to-float widening, float conditions, float asm operands | `1`, `1`, `0`, `1`, `1`, `1` (exit `0`) |
| [`examples/ops.hspl`](examples/ops.hspl) | Arithmetic, bitwise, shift, and reference (`&`/`*`) operators | `42`, `42`, `90`, `42`, `10`, `1`, `8`, `14`, `6`, `-1`, `48`, `63`, `-5`, `7`, `5`, `42` |
| [`examples/structs.hspl`](examples/structs.hspl) | Named struct declarations, field access, field assignment, nesting, by-value parameters and returns | `42`, `21`, `24`, `31`, `15`, `21` |
| [`examples/scopes.hspl`](examples/scopes.hspl) | Typed `auto`, assignment, mutable globals, shadowing, widening at a call | `41`, `3`, `7`, `42`, `7` |
| [`examples/functions.hspl`](examples/functions.hspl) | Empty statement, expression bodies, empty body, block bodies, nested calls and blocks, calling a function defined later | `7`, `0`, `42`, `9`, `42`, `42`, `42` |
| [`examples/types.hspl`](examples/types.hspl) | Every primitive, defaults, literal ranges, `bool` literals, all widening conversions, typed asm, zero-valued blocks | `9223372036854775807`, `42`, `1000`, `-56`, `1`, `0`, `7`, `127`, `-128`, `-1`, `1`, `0`, `1`, `127`, `10`, `42`, `0` (exit `42`) |
| [`examples/control.hspl`](examples/control.hspl) | `if` as an expression, integer conditions, `else`, conditional jumps, labels, backward-jump loops, unreachable code | `42`, `7`, `0`, `1`, `5`, `0`, `222`, `11` |
| [`examples/void.hspl`](examples/void.hspl) | `void` functions, the `void` default return type, `void` `main` | `5`, `42`, `7`, `9` (exit `0`) |

Run them all:

```sh
for example in examples/*.hspl; do
    name="$(basename "$example" .hspl)"
    cargo run -- "$example" -o "/tmp/hspl-$name"
    "/tmp/hspl-$name"
done
```

## Building from source

Requirements:

- **Rust 1.85+** (edition 2024; `rustc` alone is enough)
- **LLVM 22** with `llvm-config` discoverable by Inkwell's `llvm-sys`
  (the `llvm22-1` feature)
- **clang** for linking the produced IR (or set `HSPL_CC` to another driver)

On Fedora:

```sh
sudo dnf install llvm22-devel clang
```

On Ubuntu 24.04 (as used by CI), from apt.llvm.org:

```sh
wget -qO- https://apt.llvm.org/llvm-snapshot.gpg.key \
  | sudo tee /etc/apt/trusted.gpg.d/apt.llvm.org.asc
echo "deb http://apt.llvm.org/noble/ llvm-toolchain-noble-22 main" \
  | sudo tee /etc/apt/sources.list.d/llvm22.list
sudo apt-get update
sudo apt-get install -y llvm-22-dev clang-22 libpolly-22-dev
```

`llvm-sys` finds LLVM through `LLVM_SYS_221_PREFIX` when `llvm-config` is not
on `PATH` or a non-default prefix is used, e.g.:

```sh
export LLVM_SYS_221_PREFIX=/usr/lib64/llvm22   # Fedora non-standard prefix
export LLVM_SYS_221_PREFIX=/usr/lib/llvm-22    # apt.llvm.org
```

Then:

```sh
cargo build --release
cargo test
```

## Development

```sh
cargo test                                    # the unit/doc test suite
cargo run -- examples/types.hspl -o /tmp/types && /tmp/types
```

CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) runs on every push
and pull request to `main`: it installs LLVM 22 on Ubuntu 24.04, runs the test
suite with `--locked`, builds the release binary, and compiles every example.
Releases are built by [`.github/workflows/release.yml`](.github/workflows/release.yml):
pushing a `v*` tag builds the Linux x86-64 binary, packages it with `LICENSE`,
this README and the examples, and publishes a GitHub release with a sha256
checksum.

Project layout:

```text
src/
  main.rs      CLI driver: compile, link with the standard library
  lib.rs       Public pipeline API (parse_source, compile_to_ir)
  lexer.rs     Tokenizer; captures `asm { ... }` bodies raw
  parser.rs    Recursive-descent parser (precedence climbing for operators)
  ast.rs       AST definitions and the `Type` enum (primitives + named structs)
  codegen.rs   LLVM IR generation (Inkwell), including widening rules and struct GEP addressing
std/
  print.s      Standard library, written in x86-64 assembly
examples/      One self-contained program per language feature
.github/workflows/
  ci.yml       build, test, and compile every example
  release.yml  on a v* tag: publish a GitHub release with the binary
```

Pipeline:

```text
source -> lexer -> parser -> AST -> codegen -> LLVM IR
                                                    |
                                   std/print.s  -> clang -> executable
```

`std/print.s` is embedded with `include_str!`, so editing it requires
rebuilding the compiler. To inspect the pipeline stages:

```sh
cargo run -- examples/types.hspl --emit-ir -o /tmp/types.ll
opt -passes=mem2reg -S /tmp/types.ll
llc /tmp/types.ll -o /tmp/types.s
```

## Troubleshooting

- **`cargo build` fails with `could not find native static library 'Polly'`** —
  apt.llvm.org's `llvm-22-dev` needs `libpolly-22-dev` installed alongside it.
- **`error: could not run 'clang'`** — install clang, or set `HSPL_CC` to the
  compiler driver you have, e.g. `HSPL_CC=clang-22`.
- **The driver warns `overriding the module target triple with ...`** — the IR
  is tagged with the triple of the machine that built the compiler, which may
  differ from yours. The warning is emitted by the linker and is harmless;
  the resulting binary is native to your machine.
- **`call to undefined function 'print'`** — add `extern print(word) : word;`
  (or whatever signature you need) at the top of your program.
- **`jump to undefined label 'x'`** — jumps are function-scoped; the label must
  be defined somewhere in the same function body.
- **`error: struct X has no field Y`** — field access is checked against the
  struct's declaration; the error lists the fields it does know.
- **`error: unknown type 'foo' (… or a declared struct)`** — struct names are
  resolved lexically by declaration order in the file: the struct must be
  declared before (not after) the position in which the name appears.
- **`error: 'extern' signatures cannot use struct types`** — `extern` currently
  only carries primitive types. Pass struct data to foreign code through
  scalars, or keep the API inside HSPL (structs pass by value between HSPL
  functions).
- **`error: cannot implicitly convert 'point' to 'word'`** — struct values can
  only be assigned to structs of the same shape. Use a field (`p.x = v;`)
  rather than a coercion.

## Roadmap

- `while` and `for` sugar built on labels and jumps
- Hexadecimal floating-point literals (`0x1.8p3`)
- Explicit casts, so narrowing can be written down
- Struct literals and constructors (e.g. `point { x : 1, y : 2 }`); today
  structs are built by declaring a zero-initialised value and assigning fields
- Struct types in `extern` signatures (needs `byval`/`sret` attributes)
- Comparisons and logical operators
- A `printdouble`-style symbol in the standard library (today `print` handles integers only)
- **More standard library**: `putchar`, word input, and allocation, all in
  assembly
- `extern` data symbols (not just functions)
- Reference operations on non-`word` values (today `&`/`*` only work on `word`)
- Assembly clobber lists, multiple outputs, and typed constraints
- Proper calling-convention support for external C functions
- Strings/byte data
- Optimization pipeline and object-file emission
- Source spans and better diagnostics

## License

MIT — see [LICENSE](LICENSE).
