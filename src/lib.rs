#[macro_use]
extern crate log;
extern crate libc;
extern crate lru_cache;
extern crate wait_timeout;

use lru_cache::LruCache;

use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::io::Write;
use std::io;
use std::process::{Command, ExitStatus, Stdio};
use std::str::FromStr;
use std::time::Duration;

use docker::Container;

mod docker;

/// Error type holding a description
pub struct StringError(pub String);

impl Error for StringError {
    fn description(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for StringError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl fmt::Display for StringError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Hash, PartialEq, Eq, Copy, Clone)]
pub enum ReleaseChannel {
    Stable = 0,
    Beta = 1,
    Nightly = 2,
}

impl FromStr for ReleaseChannel {
    type Err = StringError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "stable" => Ok(ReleaseChannel::Stable),
            "beta" => Ok(ReleaseChannel::Beta),
            "nightly" => Ok(ReleaseChannel::Nightly),
            _ => Err(StringError(format!("unknown release channel {}", s))),
        }
    }
}

/// Helper method for safely invoking a command inside a playpen
pub fn exec(channel: ReleaseChannel,
            cmd: &str,
            args: Vec<String>,
            input: String)
            -> io::Result<(ExitStatus, Vec<u8>)> {
    #[derive(PartialEq, Eq, Hash)]
    struct CacheKey {
        channel: ReleaseChannel,
        cmd: String,
        args: Vec<String>,
        input: String,
    }

    thread_local! {
        static CACHE: RefCell<LruCache<CacheKey, (ExitStatus, Vec<u8>)>> =
            RefCell::new(LruCache::new(256))
    }

    // Build key to look up
    let key = CacheKey {
        channel: channel,
        cmd: cmd.to_string(),
        args: args,
        input: input,
    };
    let prev = CACHE.with(|cache| {
        cache.borrow_mut().get_mut(&key).map(|x| x.clone())
    });
    if let Some(prev) = prev {
        return Ok(prev)
    }

    let chan = match channel {
        ReleaseChannel::Stable => "stable",
        ReleaseChannel::Beta => "beta",
        ReleaseChannel::Nightly => "nightly",
    };
    let container = format!("rust-{}", chan);

    let container = try!(Container::new(cmd, &key.args, &container));

    let tuple = try!(container.run(key.input.as_bytes(), Duration::new(5, 0)));
    let (status, mut output, timeout) = tuple;
    if timeout {
        output.extend_from_slice(b"\ntimeout triggered!");
    }
    CACHE.with(|cache| {
        cache.borrow_mut().insert(key, (status.clone(), output.clone()));
    });
    Ok((status, output))
}

pub enum AsmFlavor {
    Att,
    Intel,
}

impl AsmFlavor {
    pub fn as_str(&self) -> &'static str {
        match *self {
            AsmFlavor::Att => "att",
            AsmFlavor::Intel => "intel",
        }
    }
}

impl FromStr for AsmFlavor {
    type Err = StringError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "att" => Ok(AsmFlavor::Att),
            "intel" => Ok(AsmFlavor::Intel),
            _ => Err(StringError(format!("unknown asm dialect {}", s))),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum OptLevel {
    O0,
    O1,
    O2,
    O3,
}

impl OptLevel {
    pub fn as_u8(&self) -> u8 {
        match *self {
            OptLevel::O0 => 0,
            OptLevel::O1 => 1,
            OptLevel::O2 => 2,
            OptLevel::O3 => 3,
        }
    }
}

impl FromStr for OptLevel {
    type Err = StringError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "0" => Ok(OptLevel::O0),
            "1" => Ok(OptLevel::O1),
            "2" => Ok(OptLevel::O2),
            "3" => Ok(OptLevel::O3),
            _ => Err(StringError(format!("unknown optimization level {}", s))),
        }
    }
}

pub enum CompileOutput {
    Asm,
    Llvm,
    Mir,
}

impl CompileOutput {
    pub fn as_opts(&self) -> &'static [&'static str] {
        // We use statics here since the borrow checker complains if we put these directly in the
        // match. Pretty ugly, but rvalue promotion might fix this.
        static ASM: &'static [&'static str] = &["--emit=asm"];
        static LLVM: &'static [&'static str] = &["--emit=llvm-ir"];
        static MIR: &'static [&'static str] = &["-Zunstable-options", "--unpretty=mir"];
        match *self {
            CompileOutput::Asm => ASM,
            CompileOutput::Llvm => LLVM,
            CompileOutput::Mir => MIR,
        }
    }
}

impl FromStr for CompileOutput {
    type Err = StringError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "asm" => Ok(CompileOutput::Asm),
            "llvm-ir" => Ok(CompileOutput::Llvm),
            "mir" => Ok(CompileOutput::Mir),
            _ => Err(StringError(format!("unknown output format {}", s))),
        }
    }
}

/// Highlights compiled rustc output according to the given output format
pub fn highlight(output_format: CompileOutput, output: &str) -> String {
    let lexer = match output_format {
        CompileOutput::Asm => "gas",
        CompileOutput::Llvm => "llvm",
        CompileOutput::Mir => "text",
    };

    let mut child = Command::new("pygmentize")
                            .arg("-l")
                            .arg(lexer)
                            .arg("-f")
                            .arg("html")
                            .stdin(Stdio::piped())
                            .stdout(Stdio::piped())
                            .spawn().unwrap();
    child.stdin.take().unwrap().write_all(output.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8(output.stdout).unwrap()
}

#[cfg(test)]
mod tests {
    extern crate env_logger;

    use super::*;

    #[test]
    fn eval() {
        drop(env_logger::init());

        let (status, out) = exec(ReleaseChannel::Stable,
                                 "/usr/local/bin/evaluate.sh",
                                 Vec::new(),
                                 String::from(r#"fn main() { println!("Hello") }"#)).unwrap();
        assert!(status.success());
        assert_eq!(out, &[0xff, b'H', b'e', b'l', b'l', b'o', b'\n']);
    }

    #[test]
    fn timeout() {
        drop(env_logger::init());

        let (status, out) = exec(ReleaseChannel::Stable,
                                 "/usr/local/bin/evaluate.sh",
                                 Vec::new(),
                                 String::from(r#"fn main() {
                                    std::thread::sleep_ms(10_000);
                                 }"#)).unwrap();
        assert!(!status.success());
        assert!(String::from_utf8_lossy(&out).contains("timeout triggered"));
    }

    #[test]
    fn compile() {
        drop(env_logger::init());

        let (status, out) = exec(ReleaseChannel::Stable,
                                 "/usr/local/bin/compile.sh",
                                 vec![String::from("--emit=llvm-ir")],
                                 String::from(r#"fn main() { println!("Hello") }"#)).unwrap();
        assert!(status.success());
        let mut split = out.splitn(2, |b| *b == b'\xff');
        let empty: &[u8] = &[];
        assert_eq!(split.next().unwrap(), empty);
        assert!(String::from_utf8(split.next().unwrap().to_vec()).unwrap()
            .contains("target triple"));
    }

    #[test]
    fn fmt() {
        drop(env_logger::init());

        let (status, out) = exec(ReleaseChannel::Stable,
                                 "rustfmt",
                                 Vec::new(),
                                 String::from(r#"fn main() { println!("Hello") }"#)).unwrap();
        assert!(status.success());
        assert!(String::from_utf8(out).unwrap().contains(r#""Hello""#))
    }

    #[test]
    fn pygmentize() {
        drop(env_logger::init());

        assert!(highlight(CompileOutput::Llvm, "target triple").contains("<span"));
    }
}
