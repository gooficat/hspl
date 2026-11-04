//! `hspl` driver: compile a `.hspl` file to LLVM IR and, by default, link it
//! with the assembly standard library into a native executable.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{SystemTime, UNIX_EPOCH};

/// The standard library is embedded at build time and linked into every
/// executable, so the compiler binary is self-contained.
const STDLIB: &str = include_str!("../std/print.s");

const USAGE: &str = "\
hspl - Hasan's Simple Programming Language

A B-like, word-oriented language with inline assembly.

Usage:
  hspl <input.hspl> [-o <output>] [--emit-ir]

Options:
  -o, --output <path>   Output path. Defaults to `a.out` when linking, and to
                        `<input>.ll` with --emit-ir.
      --emit-ir         Write LLVM IR instead of linking an executable.
  -h, --help            Show this help
  -v, --version         Show the version

The assembly standard library (std/print.s) is linked automatically.
Set HSPL_CC to use a different C compiler/linker (default: clang).
";

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

struct Options {
    input: PathBuf,
    output: Option<PathBuf>,
    emit_ir: bool,
}

fn run() -> Result<(), String> {
    let Some(options) = parse_args()? else {
        return Ok(()); // --help or --version
    };

    let source = fs::read_to_string(&options.input)
        .map_err(|err| format!("could not read `{}`: {err}", options.input.display()))?;

    let module_name = options
        .input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("hspl");

    let ir = hspl::compile_to_ir_named(&source, module_name)?;

    if options.emit_ir {
        let output = options
            .output
            .unwrap_or_else(|| options.input.with_extension("ll"));
        fs::write(&output, ir)
            .map_err(|err| format!("could not write `{}`: {err}", output.display()))?;
        println!("wrote {}", output.display());
        return Ok(());
    }

    let output = options
        .output
        .unwrap_or_else(|| PathBuf::from("a.out"));
    link(&ir, &output)?;
    println!("wrote {}", output.display());
    Ok(())
}

fn parse_args() -> Result<Option<Options>, String> {
    let args: Vec<String> = env::args().skip(1).collect();

    if args.is_empty() {
        print!("{USAGE}");
        return Err("no input file given".to_string());
    }

    let mut input: Option<PathBuf> = None;
    let mut output: Option<PathBuf> = None;
    let mut emit_ir = false;
    let mut index = 0;

    while index < args.len() {
        match args[index].as_str() {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-v" | "--version" => {
                println!("hspl {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--emit-ir" => emit_ir = true,
            "-o" | "--output" => {
                index += 1;
                let path = args
                    .get(index)
                    .ok_or_else(|| "`-o` needs a path argument".to_string())?;
                output = Some(PathBuf::from(path));
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                return Err(format!("unknown option `{flag}`\n\n{USAGE}"));
            }
            path => {
                if input.is_some() {
                    return Err("only one input file can be compiled at a time".to_string());
                }
                input = Some(PathBuf::from(path));
            }
        }
        index += 1;
    }

    let input = input.ok_or_else(|| "no input file given".to_string())?;
    Ok(Some(Options {
        input,
        output,
        emit_ir,
    }))
}

/// Write the module and the standard library to temporary files and hand them
/// to the system linker driver.
fn link(ir: &str, output: &Path) -> Result<(), String> {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let base = format!("hspl-{}-{stamp}", std::process::id());

    let temp = env::temp_dir();
    let ir_path = temp.join(format!("{base}.ll"));
    let stdlib_path = temp.join(format!("{base}.s"));

    fs::write(&ir_path, ir)
        .map_err(|err| format!("could not write `{}`: {err}", ir_path.display()))?;
    fs::write(&stdlib_path, STDLIB)
        .map_err(|err| format!("could not write `{}`: {err}", stdlib_path.display()))?;

    let compiler = env::var("HSPL_CC").unwrap_or_else(|_| "clang".to_string());
    let result = Command::new(&compiler)
        .arg(&ir_path)
        .arg(&stdlib_path)
        .arg("-o")
        .arg(output)
        .status();

    let _ = fs::remove_file(&ir_path);
    let _ = fs::remove_file(&stdlib_path);

    let status = result.map_err(|err| {
        format!("could not run `{compiler}`: {err}\nHint: install clang or set HSPL_CC")
    })?;

    if !status.success() {
        return Err(format!("`{compiler}` failed while linking ({status})"));
    }

    Ok(())
}
