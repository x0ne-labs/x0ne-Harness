//! Static reading of a shell line for the PreToolUse hook.
//!
//! The line is never run. It is cut into the gestures it would make (a
//! program started, a file redirected to, a path named in an argument), and
//! anything this reader cannot follow without running it is REFUSED rather
//! than guessed: command and process substitution, control structures,
//! `eval`/`source`, a program name or a program argument whose value is not
//! known here, a shell reading a script or stdin. Fail closed: the agent can
//! always write the line out.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Why a line was refused before any policy was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// Stable rule name, logged as `X0NE_RULE`.
    pub rule: &'static str,
    pub reason: String,
}

fn refuse(rule: &'static str, reason: impl Into<String>) -> Refusal {
    Refusal {
        rule,
        reason: reason.into(),
    }
}

/// Variable reference inside `Word::text`: `VAR_OPEN name VAR_CLOSE`.
const VAR_OPEN: char = '\u{E000}';
const VAR_CLOSE: char = '\u{E001}';

/// Placeholder for an unknown value inside a probe.
const UNKNOWN: &str = "/__x0ne_unknown__";

/// Most paths a glob argument may expand to before the line is refused.
const MAX_GLOB: usize = 512;

/// One word after quote removal, before parameter expansion.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Word {
    /// Literal text; variable references are kept as `VAR_OPEN name VAR_CLOSE`.
    text: String,
    /// Holds an expansion no value can be given to (`$@`, `~user`, `$'\c…'`).
    opaque: bool,
    /// Unquoted `*`, `?` or `[`.
    glob: bool,
    /// Unquoted `{…,…}` or `{a..b}`.
    brace: bool,
}

impl Word {
    pub fn literal(s: &str) -> Self {
        Self {
            text: s.replace([VAR_OPEN, VAR_CLOSE], ""),
            ..Self::default()
        }
    }
}

/// A word with its variables replaced.
struct Expanded {
    /// Value; unknown variables shown as `${NAME}`.
    text: String,
    /// Value; unknown variables replaced by `UNKNOWN`: what deny patterns see.
    probe: String,
    /// Every part of the value is known.
    known: bool,
    glob: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(Word),
    /// `;` `&` `&&` `||` `|` `|&` newline `(` `)`
    Op(&'static str),
    /// A redirection to or from a file. `target` is `None` for an fd dup or
    /// a here-document/here-string (data, not a path).
    Redir {
        write: bool,
        target: Option<Word>,
    },
}

/// What the line would do, in the order written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Gesture {
    /// A program started, with the assignments written in front of it.
    Run {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
        cwd: Option<PathBuf>,
    },
    /// A file opened by a redirection.
    Open { path: PathBuf, write: bool },
    /// A path named in an argument: matched against the deny patterns, the
    /// protected files and the read+write roots (the hook cannot tell a
    /// program's reads from its writes).
    PathArg {
        probe: String,
        resolved: Option<PathBuf>,
    },
}

// ── Tokenizer ────────────────────────────────────────────────────────────────

struct Lexer {
    c: Vec<char>,
    i: usize,
    /// Here-documents announced on the current line: (delimiter, quoted, strip tabs).
    pending: Vec<(String, bool, bool)>,
}

fn is_meta(ch: char) -> bool {
    matches!(
        ch,
        ' ' | '\t' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>'
    )
}

fn is_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

fn backtick() -> Refusal {
    refuse(
        "CommandSubstitution",
        "a backtick runs a command this hook cannot see before it runs; run it as its own command",
    )
}

impl Lexer {
    fn peek(&self, k: usize) -> Option<char> {
        self.c.get(self.i + k).copied()
    }

    fn tokens(mut self) -> Result<Vec<Tok>, Refusal> {
        let mut out = Vec::new();
        loop {
            while let Some(ch) = self.peek(0) {
                if ch == ' ' || ch == '\t' {
                    self.i += 1;
                } else if ch == '\\' && self.peek(1) == Some('\n') {
                    self.i += 2;
                } else {
                    break;
                }
            }
            let Some(ch) = self.peek(0) else { break };
            match ch {
                '#' => {
                    while self.peek(0).is_some_and(|c| c != '\n') {
                        self.i += 1;
                    }
                }
                '\n' => {
                    self.i += 1;
                    out.push(Tok::Op("\n"));
                    self.heredoc_bodies()?;
                }
                ';' => {
                    if self.peek(1) == Some(';') {
                        return Err(refuse(
                            "ShellSyntax",
                            "`;;` belongs to a case statement, which this hook does not analyse",
                        ));
                    }
                    self.i += 1;
                    out.push(Tok::Op(";"));
                }
                '&' => {
                    if self.peek(1) == Some('&') {
                        self.i += 2;
                        out.push(Tok::Op("&&"));
                    } else if self.peek(1) == Some('>') {
                        self.i += 2;
                        if self.peek(0) == Some('>') {
                            self.i += 1;
                        }
                        let target = self.redir_target()?;
                        out.push(Tok::Redir {
                            write: true,
                            target: Some(target),
                        });
                    } else {
                        self.i += 1;
                        out.push(Tok::Op("&"));
                    }
                }
                '|' => {
                    let op = match self.peek(1) {
                        Some('|') => "||",
                        Some('&') => "|&",
                        _ => "|",
                    };
                    self.i += op.len();
                    out.push(Tok::Op(op));
                }
                '(' => {
                    self.i += 1;
                    out.push(Tok::Op("("));
                }
                ')' => {
                    self.i += 1;
                    out.push(Tok::Op(")"));
                }
                '<' | '>' => out.push(self.redirection()?),
                _ => {
                    // `2>file`: digits glued to `<`/`>` are an fd number.
                    let start = self.i;
                    while self.peek(0).is_some_and(|c| c.is_ascii_digit()) {
                        self.i += 1;
                    }
                    if self.i > start && matches!(self.peek(0), Some('<') | Some('>')) {
                        out.push(self.redirection()?);
                        continue;
                    }
                    self.i = start;
                    let w = self.word()?;
                    out.push(Tok::Word(w));
                }
            }
        }
        if let Some((delim, _, _)) = self.pending.first() {
            return Err(refuse(
                "ShellSyntax",
                format!("here-document `{delim}` has no body: it is read from the lines after the command"),
            ));
        }
        Ok(out)
    }

    /// At `<` or `>`.
    fn redirection(&mut self) -> Result<Tok, Refusal> {
        let first = self.peek(0).unwrap_or('>');
        if self.peek(1) == Some('(') {
            return Err(refuse(
                "ProcessSubstitution",
                format!("process substitution `{first}(…)` runs a command this hook cannot see"),
            ));
        }
        self.i += 1;
        if first == '>' {
            match self.peek(0) {
                Some('>') | Some('|') => self.i += 1,
                Some('&') => {
                    self.i += 1;
                    return self.dup_or_file(true);
                }
                _ => {}
            }
            let target = self.redir_target()?;
            return Ok(Tok::Redir {
                write: true,
                target: Some(target),
            });
        }
        match self.peek(0) {
            Some('<') => {
                self.i += 1;
                if self.peek(0) == Some('<') {
                    // Here-string: the word is data fed to stdin.
                    self.i += 1;
                    self.redir_target()?;
                    return Ok(Tok::Redir {
                        write: false,
                        target: None,
                    });
                }
                let strip = self.peek(0) == Some('-');
                if strip {
                    self.i += 1;
                }
                self.skip_blanks();
                let start = self.i;
                let w = self.word()?;
                let raw: String = self.c[start..self.i].iter().collect();
                let quoted = raw.contains(['\'', '"', '\\']);
                if w.text.is_empty() || w.text.contains(VAR_OPEN) || w.opaque {
                    return Err(refuse(
                        "ShellSyntax",
                        "here-document delimiter is empty or expands",
                    ));
                }
                self.pending.push((w.text, quoted, strip));
                Ok(Tok::Redir {
                    write: false,
                    target: None,
                })
            }
            Some('&') => {
                self.i += 1;
                self.dup_or_file(false)
            }
            Some('>') => {
                // `<>`: opened read-write.
                self.i += 1;
                let target = self.redir_target()?;
                Ok(Tok::Redir {
                    write: true,
                    target: Some(target),
                })
            }
            _ => {
                let target = self.redir_target()?;
                Ok(Tok::Redir {
                    write: false,
                    target: Some(target),
                })
            }
        }
    }

    /// After `>&` or `<&`: a digit or `-` duplicates an fd, anything else is a file.
    fn dup_or_file(&mut self, write: bool) -> Result<Tok, Refusal> {
        let w = self.redir_target()?;
        let t = w.text.trim_end_matches('-');
        let dup = !w.text.is_empty() && t.chars().all(|c| c.is_ascii_digit());
        Ok(Tok::Redir {
            write,
            target: if dup { None } else { Some(w) },
        })
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(0), Some(' ') | Some('\t')) {
            self.i += 1;
        }
    }

    fn redir_target(&mut self) -> Result<Word, Refusal> {
        self.skip_blanks();
        match self.peek(0) {
            None | Some('\n') | Some(';') | Some('&') | Some('|') | Some(')') => {
                Err(refuse("ShellSyntax", "a redirection names no target"))
            }
            Some('<') | Some('>') | Some('(') => Err(refuse(
                "ProcessSubstitution",
                "a redirection target is itself a redirection or a substitution",
            )),
            _ => self.word(),
        }
    }

    /// Consume the bodies of the here-documents announced on the line just ended.
    fn heredoc_bodies(&mut self) -> Result<(), Refusal> {
        for (delim, quoted, strip) in std::mem::take(&mut self.pending) {
            let mut found = false;
            while self.i < self.c.len() {
                let start = self.i;
                while self.peek(0).is_some_and(|c| c != '\n') {
                    self.i += 1;
                }
                let line: String = self.c[start..self.i].iter().collect();
                if self.peek(0) == Some('\n') {
                    self.i += 1;
                }
                let cmp = if strip {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if cmp == delim {
                    found = true;
                    break;
                }
                if !quoted && (line.contains("$(") || line.contains('`')) {
                    return Err(refuse(
                        "CommandSubstitution",
                        format!(
                            "here-document `{delim}` is unquoted and its body holds `$(` or a backtick: \
                             the shell would run it. Quote the delimiter (<<'{delim}') to pass it as text"
                        ),
                    ));
                }
            }
            if !found {
                return Err(refuse(
                    "ShellSyntax",
                    format!("here-document `{delim}` is not terminated"),
                ));
            }
        }
        Ok(())
    }

    fn word(&mut self) -> Result<Word, Refusal> {
        let mut w = Word::default();
        // Unquoted characters only (quoted ones as NUL): brace detection.
        let mut bare = String::new();
        let start = self.i;
        while let Some(ch) = self.peek(0) {
            if is_meta(ch) {
                break;
            }
            match ch {
                '\\' => {
                    self.i += 1;
                    match self.peek(0) {
                        Some('\n') => self.i += 1,
                        Some(c) => {
                            self.i += 1;
                            w.text.push(c);
                            bare.push('\0');
                        }
                        None => {}
                    }
                }
                '\'' => {
                    self.i += 1;
                    loop {
                        match self.peek(0) {
                            None => return Err(refuse("ShellSyntax", "unterminated single quote")),
                            Some('\'') => {
                                self.i += 1;
                                break;
                            }
                            Some(c) => {
                                self.i += 1;
                                w.text.push(c);
                                bare.push('\0');
                            }
                        }
                    }
                }
                '"' => {
                    self.i += 1;
                    self.double_quoted(&mut w)?;
                    bare.push('\0');
                }
                '`' => return Err(backtick()),
                '$' => {
                    self.dollar(&mut w)?;
                    bare.push('\0');
                }
                '~' if self.i == start => {
                    self.i += 1;
                    match self.peek(0) {
                        Some(c) if c != '/' && !is_meta(c) => {
                            // `~user`: another home, unknown here.
                            w.opaque = true;
                            w.text.push('~');
                        }
                        _ => var_ref(&mut w, "HOME"),
                    }
                    bare.push('\0');
                }
                c => {
                    self.i += 1;
                    if matches!(c, '*' | '?' | '[') {
                        w.glob = true;
                    }
                    w.text.push(c);
                    bare.push(c);
                }
            }
        }
        w.brace = has_brace_expansion(&bare);
        Ok(w)
    }

    /// After the opening `"`.
    fn double_quoted(&mut self, w: &mut Word) -> Result<(), Refusal> {
        loop {
            match self.peek(0) {
                None => return Err(refuse("ShellSyntax", "unterminated double quote")),
                Some('"') => {
                    self.i += 1;
                    return Ok(());
                }
                Some('\\') => {
                    self.i += 1;
                    match self.peek(0) {
                        Some(c @ ('$' | '`' | '"' | '\\')) => {
                            self.i += 1;
                            w.text.push(c);
                        }
                        Some('\n') => self.i += 1,
                        _ => w.text.push('\\'),
                    }
                }
                Some('`') => return Err(backtick()),
                Some('$') => self.dollar(w)?,
                Some(c) => {
                    self.i += 1;
                    w.text.push(c);
                }
            }
        }
    }

    /// At `$`.
    fn dollar(&mut self, w: &mut Word) -> Result<(), Refusal> {
        self.i += 1;
        match self.peek(0) {
            Some('(') => Err(refuse(
                "CommandSubstitution",
                "`$(…)` runs a command this hook cannot see before it runs; run it as its own command",
            )),
            Some('[') => Err(refuse(
                "CommandSubstitution",
                "`$[…]` arithmetic is not analysed",
            )),
            Some('{') => {
                self.i += 1;
                let start = self.i;
                while self.peek(0).is_some_and(is_name_char) {
                    self.i += 1;
                }
                let name: String = self.c[start..self.i].iter().collect();
                if self.peek(0) != Some('}') || name.is_empty() {
                    return Err(refuse(
                        "ParameterExpansion",
                        "`${…}` with operators can run or hide a command; use a plain `$NAME`",
                    ));
                }
                self.i += 1;
                var_ref(w, &name);
                Ok(())
            }
            Some('\'') => {
                self.i += 1;
                self.ansi_c(w)
            }
            Some('"') => {
                // `$"…"`: a translatable string, the text itself here.
                self.i += 1;
                self.double_quoted(w)
            }
            Some(c) if c.is_ascii_digit() => {
                self.i += 1;
                w.opaque = true;
                w.text.push('$');
                w.text.push(c);
                Ok(())
            }
            Some(c) if is_name_char(c) => {
                let start = self.i;
                while self.peek(0).is_some_and(is_name_char) {
                    self.i += 1;
                }
                let name: String = self.c[start..self.i].iter().collect();
                var_ref(w, &name);
                Ok(())
            }
            Some(c @ ('@' | '*' | '#' | '?' | '$' | '!' | '-')) => {
                self.i += 1;
                w.opaque = true;
                w.text.push('$');
                w.text.push(c);
                Ok(())
            }
            _ => {
                w.text.push('$');
                Ok(())
            }
        }
    }

    /// After `$'`: decode as bash does, so `$'\x72m'` is read as `rm`.
    fn ansi_c(&mut self, w: &mut Word) -> Result<(), Refusal> {
        loop {
            let Some(c) = self.peek(0) else {
                return Err(refuse("ShellSyntax", "unterminated $'…'"));
            };
            self.i += 1;
            match c {
                '\'' => return Ok(()),
                '\\' => {
                    let Some(e) = self.peek(0) else { continue };
                    self.i += 1;
                    let simple = match e {
                        'a' => Some('\x07'),
                        'b' => Some('\x08'),
                        'e' | 'E' => Some('\x1b'),
                        'f' => Some('\x0c'),
                        'n' => Some('\n'),
                        'r' => Some('\r'),
                        't' => Some('\t'),
                        'v' => Some('\x0b'),
                        '\\' | '\'' | '"' | '?' => Some(e),
                        _ => None,
                    };
                    if let Some(s) = simple {
                        w.text.push(s);
                        continue;
                    }
                    let (radix, max) = match e {
                        'x' => (16, 2),
                        'u' => (16, 4),
                        'U' => (16, 8),
                        '0'..='7' => {
                            self.i -= 1;
                            (8, 3)
                        }
                        _ => {
                            // `\cX` and the rest: not decoded, so not known.
                            w.opaque = true;
                            w.text.push('\\');
                            w.text.push(e);
                            continue;
                        }
                    };
                    let mut n = 0u32;
                    let mut digits = 0;
                    while digits < max {
                        match self.peek(0).and_then(|d| d.to_digit(radix)) {
                            Some(d) => {
                                n = n * radix + d;
                                self.i += 1;
                                digits += 1;
                            }
                            None => break,
                        }
                    }
                    match char::from_u32(n).filter(|_| digits > 0) {
                        Some(ch) => w.text.push(ch),
                        None => w.opaque = true,
                    }
                }
                c => w.text.push(c),
            }
        }
    }
}

fn var_ref(w: &mut Word, name: &str) {
    w.text.push(VAR_OPEN);
    w.text.push_str(name);
    w.text.push(VAR_CLOSE);
}

/// Unquoted `{a,b}` or `{a..b}`.
fn has_brace_expansion(bare: &str) -> bool {
    let b: Vec<char> = bare.chars().collect();
    for (i, &c) in b.iter().enumerate() {
        if c != '{' {
            continue;
        }
        if let Some(j) = b[i + 1..].iter().position(|&x| x == '}') {
            let inner: String = b[i + 1..i + 1 + j].iter().collect();
            if inner.contains(',') || inner.contains("..") {
                return true;
            }
        }
    }
    false
}

// ── Analysis ─────────────────────────────────────────────────────────────────

/// Builtins that start no program and open no file by themselves.
const INERT_BUILTINS: &[&str] = &[
    ":", "true", "false", "echo", "printf", "test", "[", "pwd", "set", "unset", "wait", "read",
    "shift", "exit", "return", "export", "declare", "typeset", "local", "readonly",
];

/// Builtins and keywords whose effect cannot be read without running them.
const OPAQUE: &[(&str, &str)] = &[
    ("eval", "`eval` runs text built at run time"),
    ("source", "`source` runs a file this hook does not read"),
    (".", "`.` runs a file this hook does not read"),
    ("trap", "`trap` registers code run later"),
    ("alias", "an alias changes what a later word runs"),
    ("unalias", "aliases are not analysed"),
    ("fc", "`fc` re-runs commands from history"),
    ("enable", "`enable` swaps builtins for loadable code"),
    ("hash", "`hash -p` binds a name to any file"),
    ("shopt", "`shopt` can turn on alias expansion"),
    ("coproc", "a coprocess is not analysed"),
    ("function", "a function definition is not analysed"),
    (
        "if",
        "control structures are not analysed: run the commands one by one",
    ),
    ("then", "control structures are not analysed"),
    ("else", "control structures are not analysed"),
    ("elif", "control structures are not analysed"),
    ("fi", "control structures are not analysed"),
    ("for", "loops are not analysed: run the commands one by one"),
    (
        "while",
        "loops are not analysed: run the commands one by one",
    ),
    (
        "until",
        "loops are not analysed: run the commands one by one",
    ),
    ("do", "loops are not analysed"),
    ("done", "loops are not analysed"),
    ("case", "case statements are not analysed"),
    ("esac", "case statements are not analysed"),
    ("select", "select loops are not analysed"),
    ("[[", "`[[ … ]]` is not analysed: use `test`"),
    ("{", "command groups are not analysed"),
    ("}", "command groups are not analysed"),
];

/// Shells: accepted only as `-c '<line>'`, and the line is analysed in turn.
const SHELLS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "mksh", "ash", "busybox"];

/// Programs that start the program named in their arguments.
/// (name, options taking a value, positional words before the command)
const WRAPPERS: &[(&str, &[&str], usize)] = &[
    ("env", &["-u", "--unset"], 0),
    ("nice", &["-n", "--adjustment"], 0),
    ("nohup", &[], 0),
    ("timeout", &["-s", "--signal", "-k", "--kill-after"], 1),
    (
        "stdbuf",
        &["-i", "-o", "-e", "--input", "--output", "--error"],
        0,
    ),
    ("ionice", &["-c", "-n", "--class", "--classdata"], 0),
    ("setsid", &[], 0),
    (
        "xargs",
        &[
            "-a",
            "-d",
            "-E",
            "-I",
            "-L",
            "-n",
            "-P",
            "-s",
            "--arg-file",
            "--delimiter",
            "--eof",
            "--replace",
            "--max-lines",
            "--max-args",
            "--max-procs",
            "--max-chars",
            "--process-slot-var",
        ],
        0,
    ),
    (
        "sudo",
        &[
            "-u",
            "-g",
            "-C",
            "-D",
            "-h",
            "-p",
            "-R",
            "-T",
            "-U",
            "-r",
            "-t",
            "--user",
            "--group",
            "--close-from",
            "--chdir",
            "--host",
            "--prompt",
            "--chroot",
            "--command-timeout",
            "--other-user",
            "--role",
            "--type",
        ],
        0,
    ),
    ("doas", &["-u", "-C"], 0),
    (
        "flock",
        &["-E", "-w", "--conflict-exit-code", "--timeout"],
        1,
    ),
];

/// Shell keywords and builtins that run the command after them.
const SHELL_WRAPPERS: &[&str] = &["exec", "command", "builtin", "time", "!"];

const MAX_DEPTH: usize = 4;

/// What the reader knows while it walks the line.
pub struct Ctx {
    /// Directory relative paths resolve against; `None` after a `cd` it
    /// cannot follow.
    pub cwd: Option<PathBuf>,
    /// The environment the line starts with (the agent's, as inherited).
    pub env: HashMap<String, String>,
    /// Variables the line itself set; `None` = set to a value unknown here.
    pub vars: HashMap<String, Option<String>>,
}

impl Ctx {
    pub fn new(cwd: PathBuf, env: HashMap<String, String>) -> Self {
        Self {
            cwd: Some(cwd),
            env,
            vars: HashMap::new(),
        }
    }

    fn lookup(&self, name: &str) -> Option<String> {
        if name == "PWD" {
            return self.cwd.as_ref().map(|c| c.to_string_lossy().into_owned());
        }
        match self.vars.get(name) {
            Some(v) => v.clone(),
            None => self.env.get(name).cloned(),
        }
    }

    fn expand(&self, w: &Word) -> Expanded {
        let mut e = Expanded {
            text: String::new(),
            probe: String::new(),
            known: !w.opaque && !w.brace,
            glob: w.glob,
        };
        let mut chars = w.text.chars();
        while let Some(c) = chars.next() {
            if c != VAR_OPEN {
                e.text.push(c);
                e.probe.push(c);
                continue;
            }
            let name: String = chars.by_ref().take_while(|&x| x != VAR_CLOSE).collect();
            match self.lookup(&name) {
                Some(v) => {
                    e.text.push_str(&v);
                    e.probe.push_str(&v);
                }
                None => {
                    e.known = false;
                    e.text.push_str(&format!("${{{name}}}"));
                    e.probe.push_str(UNKNOWN);
                }
            }
        }
        e
    }

    /// The context a `bash -c` child starts with: the line's variables
    /// reach it only if exported, which is not tracked, so they are unknown.
    fn child(&self) -> Self {
        Self {
            cwd: self.cwd.clone(),
            env: self.env.clone(),
            vars: self.vars.keys().map(|k| (k.clone(), None)).collect(),
        }
    }
}

/// Every gesture of `line`, or the reason it cannot be read.
pub fn analyse(line: &str, ctx: &mut Ctx) -> Result<Vec<Gesture>, Refusal> {
    analyse_at(line, ctx, 0)
}

/// Gestures of an argv given as a list (Codex `shell` tool): no shell parses
/// it, so each element is one literal word.
pub fn analyse_argv(argv: &[String], ctx: &mut Ctx) -> Result<Vec<Gesture>, Refusal> {
    let words: Vec<Word> = argv.iter().map(|a| Word::literal(a)).collect();
    let mut out = Vec::new();
    command(&words, HashMap::new(), ctx, 0, &mut out)?;
    Ok(out)
}

fn analyse_at(line: &str, ctx: &mut Ctx, depth: usize) -> Result<Vec<Gesture>, Refusal> {
    if depth > MAX_DEPTH {
        return Err(refuse("ShellSyntax", "shells nested more than four deep"));
    }
    if line.contains(['\0', VAR_OPEN, VAR_CLOSE]) {
        return Err(refuse(
            "ShellSyntax",
            "NUL or private-use character in the command",
        ));
    }
    let toks = Lexer {
        c: line.chars().collect(),
        i: 0,
        pending: Vec::new(),
    }
    .tokens()?;

    let mut out = Vec::new();
    let mut words: Vec<Word> = Vec::new();
    let mut redirs: Vec<(bool, Option<Word>)> = Vec::new();
    let mut toks = toks.into_iter();
    loop {
        let t = toks.next();
        match t {
            Some(Tok::Word(w)) => words.push(w),
            Some(Tok::Redir { write, target }) => redirs.push((write, target)),
            Some(Tok::Op(op)) if op == "(" || op == ")" => {
                return Err(refuse(
                    "ShellSyntax",
                    "subshells `( … )` are not analysed: run the commands directly",
                ))
            }
            Some(Tok::Op(_)) | None => {
                if !words.is_empty() || !redirs.is_empty() {
                    simple(
                        std::mem::take(&mut words),
                        std::mem::take(&mut redirs),
                        ctx,
                        depth,
                        &mut out,
                    )?;
                }
                if t.is_none() {
                    break;
                }
            }
        }
    }
    Ok(out)
}

/// `NAME=value` (or `NAME+=value`) as written: (name, the word holding the value).
fn split_assignment(w: &Word) -> Option<(String, Word)> {
    let eq = w.text.find('=')?;
    let name = w.text[..eq].trim_end_matches('+');
    let mut ch = name.chars();
    let ok = ch
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && ch.all(is_name_char);
    ok.then(|| {
        (
            name.to_string(),
            Word {
                text: w.text[eq + 1..].to_string(),
                ..w.clone()
            },
        )
    })
}

fn check_env(env: &HashMap<String, String>) -> Result<(), Refusal> {
    x0ne_actions::env_keys::check_caller_env(env).map_err(|why| refuse("EnvironmentVariable", why))
}

/// Assignments as a checked map; values shown as written when unknown.
fn assignments(pairs: &[(String, Word)], ctx: &Ctx) -> Result<HashMap<String, String>, Refusal> {
    let map: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.clone(), ctx.expand(v).text))
        .collect();
    check_env(&map)?;
    Ok(map)
}

/// Record shell variables set by the line.
fn remember(pairs: &[(String, Word)], ctx: &mut Ctx) {
    for (k, v) in pairs {
        let e = ctx.expand(v);
        let value = (e.known && !e.glob).then_some(e.text);
        ctx.vars.insert(k.clone(), value);
    }
}

fn resolve(ctx: &Ctx, text: &str) -> Option<PathBuf> {
    let p = Path::new(text);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        ctx.cwd.as_ref().map(|c| c.join(p))
    }
}

/// Devices a redirection may name without being a file of the workspace.
fn harmless_device(p: &Path) -> bool {
    let s = p.to_string_lossy();
    matches!(
        s.as_ref(),
        "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/stdin" | "/dev/tty" | "/dev/zero"
    ) || s
        .strip_prefix("/dev/fd/")
        .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()))
}

fn simple(
    words: Vec<Word>,
    redirs: Vec<(bool, Option<Word>)>,
    ctx: &mut Ctx,
    depth: usize,
    out: &mut Vec<Gesture>,
) -> Result<(), Refusal> {
    for (write, target) in redirs {
        let Some(t) = target else { continue };
        let e = ctx.expand(&t);
        if !e.known || e.glob {
            return Err(refuse(
                "PathFromVariable",
                format!(
                    "redirection target `{}` expands to a value this hook does not know",
                    e.text
                ),
            ));
        }
        let Some(path) = resolve(ctx, &e.text) else {
            return Err(refuse(
                "UnknownDirectory",
                format!(
                    "relative redirection `{}` after a `cd` this hook cannot follow",
                    e.text
                ),
            ));
        };
        if !harmless_device(&path) {
            out.push(Gesture::Open { path, write });
        }
    }

    let mut pairs = Vec::new();
    let mut i = 0;
    while let Some(p) = words.get(i).and_then(split_assignment) {
        pairs.push(p);
        i += 1;
    }
    if i == words.len() {
        // `NAME=value` alone sets a shell variable.
        assignments(&pairs, ctx)?;
        remember(&pairs, ctx);
        return Ok(());
    }
    let env = assignments(&pairs, ctx)?;
    command(&words[i..], env, ctx, depth, out)
}

/// Path arguments. `strict`: an argument whose value is unknown is refused
/// (a program would open whatever it turns out to be).
fn path_args(
    words: &[Word],
    ctx: &Ctx,
    strict: bool,
    out: &mut Vec<Gesture>,
) -> Result<(), Refusal> {
    for w in words {
        let e = ctx.expand(w);
        if !e.known {
            if strict {
                return Err(refuse(
                    "PathFromVariable",
                    format!(
                        "argument `{}` expands to a value this hook does not know: write the value out",
                        e.text
                    ),
                ));
            }
            out.push(Gesture::PathArg {
                probe: e.probe,
                resolved: None,
            });
            continue;
        }
        // `--opt=value`: the value is the path.
        let (text, probe) = match e.text.split_once('=') {
            Some((_, v)) if e.text.starts_with('-') => (
                v.to_string(),
                e.probe
                    .split_once('=')
                    .map(|(_, p)| p)
                    .unwrap_or(v)
                    .to_string(),
            ),
            _ => (e.text.clone(), e.probe.clone()),
        };
        if e.glob {
            glob_args(&text, ctx, out)?;
            continue;
        }
        let looks_like_path = text.contains('/')
            || text.starts_with('.')
            || ctx.cwd.as_ref().is_some_and(|c| c.join(&text).exists());
        if !looks_like_path || text.is_empty() {
            continue;
        }
        let resolved = resolve(ctx, &text);
        let probe = match &resolved {
            Some(r) => r.to_string_lossy().into_owned(),
            None => format!("{UNKNOWN}/{probe}"),
        };
        out.push(Gesture::PathArg { probe, resolved });
    }
    Ok(())
}

/// A glob argument: its fixed directory, and every path it matches now.
fn glob_args(text: &str, ctx: &Ctx, out: &mut Vec<Gesture>) -> Result<(), Refusal> {
    let Some(abs) = resolve(ctx, text) else {
        return Err(refuse(
            "UnknownDirectory",
            format!("relative glob `{text}` after a `cd` this hook cannot follow"),
        ));
    };
    let abs_s = abs.to_string_lossy().into_owned();
    // Deepest directory with no glob character: every match is under it.
    let fixed: PathBuf = abs
        .components()
        .take_while(|c| !c.as_os_str().to_string_lossy().contains(['*', '?', '[']))
        .collect();
    out.push(Gesture::PathArg {
        probe: abs_s.clone(),
        resolved: Some(fixed),
    });
    let Ok(paths) = glob::glob(&abs_s) else {
        return Ok(()); // not a valid pattern: bash passes it as text
    };
    for (n, p) in paths.flatten().enumerate() {
        if n >= MAX_GLOB {
            return Err(refuse(
                "GlobTooWide",
                format!("`{text}` matches more than {MAX_GLOB} paths: narrow it"),
            ));
        }
        out.push(Gesture::PathArg {
            probe: p.to_string_lossy().into_owned(),
            resolved: Some(p),
        });
    }
    Ok(())
}

fn command(
    words: &[Word],
    env: HashMap<String, String>,
    ctx: &mut Ctx,
    depth: usize,
    out: &mut Vec<Gesture>,
) -> Result<(), Refusal> {
    let Some(first) = words.first() else {
        return Ok(()); // redirections only
    };
    let e = ctx.expand(first);
    // `[`, `[[`, `{`, `}` carry glob or brace characters but are names here.
    let bracket = matches!(e.text.as_str(), "[" | "[[" | "{" | "}");
    if !bracket && (!e.known || e.glob) {
        return Err(refuse(
            "CommandFromVariable",
            format!("the program name `{}` expands at run time", e.text),
        ));
    }
    let name = e.text.as_str();
    let rest = &words[1..];

    if let Some((_, why)) = OPAQUE.iter().find(|(n, _)| *n == name) {
        return Err(refuse("ShellSyntax", *why));
    }

    if matches!(name, "cd" | "pushd" | "popd") {
        path_args(rest, ctx, false, out)?;
        let target = rest
            .iter()
            .map(|w| ctx.expand(w))
            .find(|w| !w.text.starts_with('-') || w.text == "-");
        ctx.cwd = match (name, target) {
            ("popd", _) => None,
            (_, None) => ctx.lookup("HOME").map(PathBuf::from),
            (_, Some(w)) if !w.known || w.glob || w.text == "-" => None,
            (_, Some(w)) => resolve(ctx, &w.text),
        };
        return Ok(());
    }

    if INERT_BUILTINS.contains(&name) {
        match name {
            "export" | "declare" | "typeset" | "local" | "readonly" => {
                let pairs: Vec<(String, Word)> = rest.iter().filter_map(split_assignment).collect();
                assignments(&pairs, ctx)?;
                remember(&pairs, ctx);
            }
            "read" | "unset" => {
                for w in rest.iter().filter(|w| !w.text.starts_with('-')) {
                    let v = if name == "unset" {
                        Some(String::new())
                    } else {
                        None
                    };
                    ctx.vars.insert(w.text.clone(), v);
                }
            }
            _ => {}
        }
        return path_args(rest, ctx, false, out);
    }

    if SHELL_WRAPPERS.contains(&name) {
        if name == "command" && rest.iter().any(|w| w.text == "-v" || w.text == "-V") {
            return Ok(()); // a lookup, nothing runs
        }
        let mut j = 0;
        while j < rest.len() && rest[j].text.starts_with('-') && rest[j].text != "-" {
            if name == "exec" && rest[j].text == "-a" {
                j += 1;
            }
            j += 1;
        }
        if name == "builtin" {
            let inner = rest.get(j).map(|w| w.text.as_str()).unwrap_or("");
            if !INERT_BUILTINS.contains(&inner) && inner != "cd" {
                return Err(refuse(
                    "ShellSyntax",
                    format!("`builtin {inner}` is not analysed"),
                ));
            }
        }
        return command(&rest[j.min(rest.len())..], env, ctx, depth, out);
    }

    // A program: the policy judges it by its exact name.
    let args: Vec<String> = rest.iter().map(|w| ctx.expand(w).text).collect();
    out.push(Gesture::Run {
        command: name.to_string(),
        args,
        env,
        cwd: ctx.cwd.clone(),
    });
    let base = name.rsplit('/').next().unwrap_or(name).to_string();
    if SHELLS.contains(&base.as_str()) {
        return shell_c(&base, rest, ctx, depth, out);
    }
    if let Some((_, value_opts, positionals)) = WRAPPERS.iter().find(|(n, _, _)| *n == base) {
        return wrapper(&base, value_opts, *positionals, rest, ctx, depth, out);
    }
    if base == "find" {
        let mut k = 0;
        let mut plain = Vec::new();
        while k < rest.len() {
            if matches!(
                rest[k].text.as_str(),
                "-exec" | "-execdir" | "-ok" | "-okdir"
            ) {
                let start = k + 1;
                let mut end = start;
                while end < rest.len() && rest[end].text != ";" && rest[end].text != "+" {
                    end += 1;
                }
                // `{}` is find's placeholder, not a brace expansion.
                let inner: Vec<Word> = rest[start..end].to_vec();
                command(&inner, HashMap::new(), ctx, depth, out)?;
                k = end + 1;
                continue;
            }
            plain.push(rest[k].clone());
            k += 1;
        }
        return path_args(&plain, ctx, true, out);
    }
    path_args(rest, ctx, true, out)
}

/// `bash -c '<line>'`: the line is analysed like the outer one. Anything
/// else a shell can be asked to run (a script file, stdin) is refused.
fn shell_c(
    base: &str,
    rest: &[Word],
    ctx: &mut Ctx,
    depth: usize,
    out: &mut Vec<Gesture>,
) -> Result<(), Refusal> {
    let mut args = rest;
    if base == "busybox" {
        match args.first().map(|w| w.text.as_str()) {
            Some("sh") | Some("ash") => args = &args[1..],
            // An applet runs like the program of that name.
            _ => return command(args, HashMap::new(), ctx, depth, out),
        }
    }
    let mut k = 0;
    let mut wants_c = false;
    while k < args.len() {
        let t = &args[k].text;
        if t == "-c" || (t.starts_with('-') && !t.starts_with("--") && t.contains('c')) {
            wants_c = true;
            k += 1;
            break;
        }
        if t == "-o" || t == "+o" {
            k += 2;
            continue;
        }
        if t.starts_with('-') || t.starts_with('+') {
            k += 1;
            continue;
        }
        break;
    }
    if !wants_c {
        return Err(refuse(
            "ShellScript",
            format!(
                "`{base}` would run a script file or read commands from stdin, which this hook \
                 cannot read: run the commands directly, or as `{base} -c '<line>'`"
            ),
        ));
    }
    sub_line(args.get(k), base, ctx, depth, out)
}

/// The line handed to `sh -c` or `flock -c`.
fn sub_line(
    script: Option<&Word>,
    by: &str,
    ctx: &Ctx,
    depth: usize,
    out: &mut Vec<Gesture>,
) -> Result<(), Refusal> {
    let Some(script) = script else {
        return Err(refuse(
            "ShellSyntax",
            format!("`{by} -c` without a command"),
        ));
    };
    let e = ctx.expand(script);
    if !e.known {
        return Err(refuse(
            "CommandFromVariable",
            format!("the line given to `{by} -c` expands to a value this hook does not know"),
        ));
    }
    let mut inner = ctx.child();
    out.extend(analyse_at(&e.text, &mut inner, depth + 1)?);
    Ok(())
}

fn wrapper(
    base: &str,
    value_opts: &[&str],
    positionals: usize,
    rest: &[Word],
    ctx: &mut Ctx,
    depth: usize,
    out: &mut Vec<Gesture>,
) -> Result<(), Refusal> {
    let mut pairs = Vec::new();
    let mut k = 0;
    while k < rest.len() {
        let t = rest[k].text.as_str();
        if t == "--" {
            k += 1;
            break;
        }
        if base == "env" {
            if t.starts_with("-S") || t == "--split-string" || t.starts_with("--split-string=") {
                return Err(refuse(
                    "ShellSyntax",
                    "`env -S` splits a string into a command line this hook does not parse",
                ));
            }
            if t == "-C" || t == "--chdir" || t.starts_with("--chdir=") {
                return Err(refuse(
                    "UnknownDirectory",
                    "`env -C` changes directory: run `cd` first",
                ));
            }
            if let Some(p) = split_assignment(&rest[k]) {
                pairs.push(p);
                k += 1;
                continue;
            }
        }
        if base == "flock" && (t == "-c" || t == "--command") {
            return sub_line(rest.get(k + 1), base, ctx, depth, out);
        }
        if matches!(base, "sudo" | "doas") && matches!(t, "-s" | "-i" | "--shell" | "--login") {
            return Err(refuse(
                "ShellScript",
                format!("`{base} {t}` opens a shell this hook cannot read"),
            ));
        }
        if t.starts_with("--") {
            if !t.contains('=') && value_opts.contains(&t) {
                k += 1;
            }
            k += 1;
            continue;
        }
        if t.starts_with('-') && t.len() > 1 {
            if value_opts.contains(&t) {
                k += 1;
            }
            k += 1;
            continue;
        }
        break;
    }
    let env = assignments(&pairs, ctx)?;
    // Positional words (a duration, a lock file) are paths or plain values.
    let end = (k + positionals).min(rest.len());
    path_args(&rest[k..end], ctx, true, out)?;
    k = end;
    let Some(next) = rest.get(k) else {
        return Ok(()); // nothing to start (`xargs` alone runs `echo`)
    };
    // `flock FILE -c LINE`: options may follow the lock file.
    if base == "flock" && (next.text == "-c" || next.text == "--command") {
        return sub_line(rest.get(k + 1), base, ctx, depth, out);
    }
    if next.text.starts_with('-') {
        return Err(refuse(
            "ShellSyntax",
            format!(
                "`{base}`: option `{}` after its positional words is not analysed",
                next.text
            ),
        ));
    }
    command(&rest[k..], env, ctx, depth, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        let env = HashMap::from([
            ("HOME".to_string(), "/home/u".to_string()),
            ("EDITOR".to_string(), "vim".to_string()),
        ]);
        Ctx::new(PathBuf::from("/w"), env)
    }

    fn run(line: &str) -> Result<Vec<Gesture>, Refusal> {
        analyse(line, &mut ctx())
    }

    fn programs(line: &str) -> Vec<String> {
        run(line)
            .unwrap_or_else(|r| panic!("{line}: {r:?}"))
            .into_iter()
            .filter_map(|g| match g {
                Gesture::Run { command, .. } => Some(command),
                _ => None,
            })
            .collect()
    }

    fn probes(line: &str) -> Vec<String> {
        run(line)
            .unwrap()
            .into_iter()
            .filter_map(|g| match g {
                Gesture::PathArg { probe, .. } => Some(probe),
                _ => None,
            })
            .collect()
    }

    fn rule(line: &str) -> &'static str {
        run(line).expect_err(line).rule
    }

    #[test]
    fn every_program_of_a_list_is_seen() {
        assert_eq!(
            programs("ls -la && cat a | grep x; rm -rf b || true"),
            ["ls", "cat", "grep", "rm"]
        );
        assert_eq!(programs("ls\nrm x &"), ["ls", "rm"]);
    }

    #[test]
    fn substitutions_are_refused() {
        assert_eq!(rule("echo $(rm -rf /)"), "CommandSubstitution");
        assert_eq!(rule("echo \"$(id)\""), "CommandSubstitution");
        assert_eq!(rule("echo `id`"), "CommandSubstitution");
        assert_eq!(rule("diff <(ls) x"), "ProcessSubstitution");
        assert_eq!(rule("echo ${X:-$(id)}"), "ParameterExpansion");
        assert_eq!(rule("echo $((1+2))"), "CommandSubstitution");
    }

    #[test]
    fn a_program_named_by_a_variable_is_read_or_refused() {
        // Known values are read: the policy then judges the real name.
        assert_eq!(programs("X=rm; $X -rf /w/a"), ["rm"]);
        assert_eq!(programs("$EDITOR f"), ["vim"]);
        assert_eq!(programs("$'\\x72m' f"), ["rm"]);
        assert_eq!(programs("$'\\162m' f"), ["rm"]);
        // Unknown values are refused.
        assert_eq!(rule("${NOPE} a"), "CommandFromVariable");
        assert_eq!(rule("read X; $X a"), "CommandFromVariable");
        assert_eq!(rule("r? x"), "CommandFromVariable");
        assert_eq!(rule("{rm,-rf} x"), "CommandFromVariable");
        assert_eq!(rule("$'\\cx' a"), "CommandFromVariable");
        assert_eq!(rule("$1 a"), "CommandFromVariable");
    }

    #[test]
    fn quoting_does_not_hide_the_program() {
        assert_eq!(programs("'r'\"m\" -rf x"), ["rm"]);
        assert_eq!(programs("r\\m x"), ["rm"]);
    }

    #[test]
    fn shells_are_read_through_dash_c() {
        assert_eq!(programs("bash -c 'ls; rm -rf x'"), ["bash", "ls", "rm"]);
        assert_eq!(programs("sh -ec \"cat a\""), ["sh", "cat"]);
        assert_eq!(rule("bash script.sh"), "ShellScript");
        assert_eq!(rule("bash"), "ShellScript");
        assert_eq!(rule("bash -c \"$CMD\""), "CommandFromVariable");
        // A variable of the outer line is not known inside the child.
        assert_eq!(rule("X=rm; bash -c '$X y'"), "CommandFromVariable");
    }

    #[test]
    fn wrappers_are_unwrapped() {
        assert_eq!(
            programs("env FOO=1 nice -n 5 timeout 10 rm x"),
            ["env", "nice", "timeout", "rm"]
        );
        assert_eq!(programs("xargs -n1 rm < list"), ["xargs", "rm"]);
        assert_eq!(programs("sudo -u root rm x"), ["sudo", "rm"]);
        assert_eq!(programs("find . -name a -exec rm {} \\;"), ["find", "rm"]);
        assert_eq!(programs("exec rm x"), ["rm"]);
        assert_eq!(programs("command rm x"), ["rm"]);
        assert_eq!(programs("time rm x"), ["rm"]);
        assert_eq!(programs("! rm x"), ["rm"]);
        assert!(programs("command -v rm").is_empty());
        assert_eq!(programs("busybox rm x"), ["busybox", "rm"]);
        assert!(programs("[ -f x ] && ls").contains(&"ls".to_string()));
    }

    #[test]
    fn wrappers_that_hide_a_line_are_refused() {
        assert_eq!(rule("env -S 'rm -rf x'"), "ShellSyntax");
        assert_eq!(rule("sudo -s"), "ShellScript");
        assert_eq!(rule("flock /tmp/l -c \"$C\""), "CommandFromVariable");
        assert_eq!(programs("flock /tmp/l -c 'rm x'"), ["flock", "rm"]);
        assert_eq!(programs("flock -w 3 /tmp/l rm x"), ["flock", "rm"]);
    }

    #[test]
    fn dangerous_assignments_are_refused() {
        assert_eq!(rule("LD_PRELOAD=/tmp/x.so ls"), "EnvironmentVariable");
        assert_eq!(rule("PATH=/tmp/evil:$PATH ls"), "EnvironmentVariable");
        assert_eq!(rule("PATH=/tmp/evil; ls"), "EnvironmentVariable");
        assert_eq!(rule("export PATH=/tmp"), "EnvironmentVariable");
        assert_eq!(
            rule("env BASH_ENV=/tmp/x bash -c ls"),
            "EnvironmentVariable"
        );
        assert!(run("FOO=1 ls").is_ok());
    }

    #[test]
    fn opaque_constructs_are_refused() {
        for l in [
            "eval rm x",
            "source x.sh",
            ". x.sh",
            "if true; then rm x; fi",
            "for f in *; do rm $f; done",
            "(rm x)",
            "{ rm x; }",
            "alias ls=rm",
        ] {
            assert!(run(l).is_err(), "{l}");
        }
    }

    #[test]
    fn redirections_are_file_opens() {
        let g = run("echo hi > out.txt 2>&1 >> /abs/log < in 2>/dev/null").unwrap();
        let opens: Vec<_> = g
            .iter()
            .filter_map(|g| match g {
                Gesture::Open { path, write } => {
                    Some((path.to_string_lossy().into_owned(), *write))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            opens,
            [
                ("/w/out.txt".into(), true),
                ("/abs/log".into(), true),
                ("/w/in".into(), false)
            ]
        );
        assert_eq!(rule("echo x > $F"), "PathFromVariable");
        assert_eq!(rule("echo x &> ~$U/x"), "PathFromVariable");
        assert_eq!(rule("echo x > *.txt"), "PathFromVariable");
        let g = run("F=/abs/f; echo x > $F").unwrap();
        assert!(g.contains(&Gesture::Open {
            path: "/abs/f".into(),
            write: true
        }));
    }

    #[test]
    fn heredocs() {
        assert!(run("cat <<'EOF' > f\n$(rm -rf /)\nEOF\nls").is_ok());
        assert_eq!(rule("cat <<EOF\n$(rm -rf /)\nEOF"), "CommandSubstitution");
        assert_eq!(rule("cat <<EOF\nno end"), "ShellSyntax");
        assert_eq!(
            programs("cat <<EOF | bash -c 'ls'\nx\nEOF\nrm y"),
            ["cat", "bash", "ls", "rm"]
        );
        assert_eq!(rule("bash <<EOF\nrm -rf x\nEOF"), "ShellScript");
    }

    #[test]
    fn cd_moves_the_base_of_relative_paths() {
        let g = run("cd sub && echo x > f").unwrap();
        assert!(g.contains(&Gesture::Open {
            path: "/w/sub/f".into(),
            write: true
        }));
        assert_eq!(rule("cd - && echo x > f"), "UnknownDirectory");
    }

    #[test]
    fn variables_reach_the_probe_or_are_refused() {
        assert_eq!(
            probes("cat ~/.ssh/id_rsa $HOME/.env \"${HOME}/x\""),
            ["/home/u/.ssh/id_rsa", "/home/u/.env", "/home/u/x"]
        );
        assert_eq!(probes("D=/etc; cat $D/passwd"), ["/etc/passwd"]);
        assert_eq!(probes("cat $'/home/u/\\x2essh/k'"), ["/home/u/.ssh/k"]);
        // A program argument with an unknown value is refused…
        assert_eq!(rule("cat \"$NOPE/.ssh/k\""), "PathFromVariable");
        assert_eq!(rule("cat $NOPE"), "PathFromVariable");
        assert_eq!(rule("cat a/{b,c}"), "PathFromVariable");
        // …a builtin's is only probed.
        assert_eq!(probes("echo $NOPE/.ssh/k"), ["/__x0ne_unknown__/.ssh/k"]);
    }

    #[test]
    fn globs_name_their_fixed_directory() {
        assert_eq!(probes("cat /etc/*.conf")[0], "/etc/*.conf");
        let g = run("ls /e?c/x").unwrap();
        assert!(g.contains(&Gesture::PathArg {
            probe: "/e?c/x".into(),
            resolved: Some("/".into())
        }));
    }

    #[test]
    fn comments_and_continuations() {
        assert_eq!(programs("ls # ; rm x"), ["ls"]);
        assert_eq!(programs("r\\\nm x"), ["rm"]);
    }
}
