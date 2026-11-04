//! Credential masking for what the session log stores and `ListProcesses`
//! returns.
//!
//! Over-masking is accepted, under-masking is the bug. Four rules, each with
//! a test: private key blocks, `name = value` where the name says secret,
//! credentials in URLs, and words that look like tokens (known prefixes, JWTs,
//! long mixed-case random strings). Lowercase hex (digests) is left alone.

pub const MASK: &str = "[redacted]";

/// True if [`redact`] would change `s`.
pub fn looks_secret(s: &str) -> bool {
    redact(s) != s
}

/// `s` with every credential-looking part replaced by [`MASK`].
pub fn redact(s: &str) -> String {
    let s = mask_private_keys(s);
    let lines: Vec<String> = s.split('\n').map(mask_assignment).collect();
    let s = lines.join("\n");
    let s = mask_url_userinfo(&s);
    mask_words(&s)
}

fn mask_private_keys(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("-----BEGIN") {
        let header_end = rest[start..].find('\n').map_or(rest.len(), |i| start + i);
        if !rest[start..header_end].contains("PRIVATE KEY") {
            out.push_str(&rest[..header_end]);
            rest = &rest[header_end..];
            continue;
        }
        out.push_str(&rest[..start]);
        out.push_str(MASK);
        rest = match rest[start..].find("-----END") {
            Some(e) => {
                let after = start + e + "-----END".len();
                match rest[after..].find("-----") {
                    Some(t) => &rest[after + t + 5..],
                    None => "",
                }
            }
            None => "",
        };
    }
    out.push_str(rest);
    out
}

/// Mask every string in `v`; a string under a key that names a secret is
/// replaced whole, whatever it looks like.
pub fn redact_value(v: &mut serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::String(s) => {
            if looks_secret(s) {
                *s = redact(s);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(redact_value),
        Value::Object(o) => {
            for (k, val) in o.iter_mut() {
                match val {
                    Value::String(s) if sensitive_name(k) && !s.is_empty() => *s = MASK.to_string(),
                    _ => redact_value(val),
                }
            }
        }
        _ => {}
    }
}

/// A process argv, masked one argument at a time so the others stay
/// readable. On top of [`redact`]: `--name=value` and `--name value` when the
/// name says secret, `--user name:pass`, and the flags of `program_flags`.
pub fn redact_argv<S: AsRef<str>>(argv: &[S]) -> Vec<String> {
    let mut masker = ArgvMasker::default();
    argv.iter().map(|a| masker.arg(a.as_ref())).collect()
}

#[derive(Clone, Copy)]
enum FlagValue {
    Secret,
    UserPass,
}

impl FlagValue {
    fn mask(self, v: &str) -> String {
        match (self, v.split_once(':')) {
            (FlagValue::Secret, _) => MASK.to_string(),
            (FlagValue::UserPass, Some((user, pass))) if !pass.is_empty() => {
                format!("{}:{MASK}", redact(user))
            }
            (FlagValue::UserPass, _) => redact(v),
        }
    }
}

/// Flags whose value is `user:password`, whatever the program.
const USER_FLAGS: &[&str] = &["--user", "--proxy-user"];

/// `(flag, value may be the next argument, kind)` for programs whose short
/// flag carries a password. `mysql -p db` prompts: only `-pX` is masked.
fn program_flags(program: &str) -> &'static [(&'static str, bool, FlagValue)] {
    use FlagValue::{Secret, UserPass};
    match program {
        p if p.starts_with("mysql") || p.starts_with("mariadb") => &[("-p", false, Secret)],
        "sshpass" => &[("-p", true, Secret)],
        "redis-cli" => &[("-a", true, Secret)],
        "curl" => &[("-u", true, UserPass), ("-U", true, UserPass)],
        _ => &[],
    }
}

#[derive(Default)]
struct ArgvMasker {
    /// Flags of the last known program seen, so that `sudo mysql -pX` counts.
    flags: &'static [(&'static str, bool, FlagValue)],
    /// The previous word was a flag and this word is its value.
    pending: Option<FlagValue>,
}

impl ArgvMasker {
    fn arg(&mut self, arg: &str) -> String {
        if let Some(masked) = self.word(arg) {
            return masked;
        }
        if let Some((flag, value)) = arg.split_once('=') {
            if !value.is_empty() && secret_flag(flag) {
                return format!("{flag}={MASK}");
            }
        }
        // `sh -c '…'` and a rewritten process title carry a whole command line.
        if arg.contains(' ') {
            let mut inner = ArgvMasker {
                flags: self.flags,
                pending: None,
            };
            let words: Vec<String> = arg
                .split(' ')
                .map(|w| inner.word(w).unwrap_or_else(|| w.to_string()))
                .collect();
            return redact(&words.join(" "));
        }
        redact(arg)
    }

    /// The flag rules for one word: `Some` when they masked it. Updates the
    /// state for the next word either way.
    fn word(&mut self, w: &str) -> Option<String> {
        if let Some(kind) = self.pending.take() {
            if !w.is_empty() && !w.starts_with("--") {
                return Some(kind.mask(w));
            }
        }
        let flags = program_flags(w.rsplit('/').next().unwrap_or(w));
        if !flags.is_empty() {
            self.flags = flags;
            return None;
        }
        for &(flag, next, kind) in self.flags {
            if w == flag {
                self.pending = next.then_some(kind);
                return None;
            }
            if let Some(v) = w.strip_prefix(flag).filter(|v| !v.is_empty()) {
                return Some(format!("{flag}{}", kind.mask(v)));
            }
        }
        if let Some((flag, value)) = w.split_once('=') {
            return USER_FLAGS
                .contains(&flag)
                .then(|| format!("{flag}={}", FlagValue::UserPass.mask(value)));
        }
        if USER_FLAGS.contains(&w) {
            self.pending = Some(FlagValue::UserPass);
        } else if w.starts_with('-') && secret_flag(w) {
            self.pending = Some(FlagValue::Secret);
        }
        None
    }
}

/// Whether an identifier names a secret (`OPENAI_API_KEY`, `password`, …).
pub fn sensitive_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const WORDS: &[&str] = &[
        "password",
        "passwd",
        "passphrase",
        "secret",
        "token",
        "apikey",
        "api_key",
        "api-key",
        "auth",
        "credential",
        "cookie",
        "private",
    ];
    WORDS.iter().any(|w| n.contains(w))
        || n.ends_with("key")
        || n == "pwd"
        || n.ends_with("_pwd")
        || n.ends_with("_pass")
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// `--api-key`, `-password`, `DB_TOKEN`: optional dashes, then a name that
/// says secret. A URL or a path before `=` is not a flag.
fn secret_flag(flag: &str) -> bool {
    let name = flag.trim_start_matches('-');
    !name.is_empty() && name.chars().all(is_ident) && sensitive_name(name)
}

/// `NAME=value`, `"name": "value"`, `Authorization: Bearer x`: the value is
/// masked to the end of the line when the name is sensitive.
fn mask_assignment(line: &str) -> String {
    for (i, c) in line.char_indices() {
        if c != '=' && c != ':' {
            continue;
        }
        if c == ':' && line[i + 1..].starts_with("//") {
            continue;
        }
        // `\"name\":` is how a file's JSON looks inside a tool result.
        let before = line[..i].trim_end_matches([' ', '"', '\'', '\\']);
        let start = before
            .char_indices()
            .rev()
            .find(|&(_, ch)| !is_ident(ch))
            .map_or(0, |(j, ch)| j + ch.len_utf8());
        let name = &before[start..];
        if !name.is_empty() && sensitive_name(name) && !line[i + 1..].trim().is_empty() {
            return format!("{} {MASK}", &line[..=i]);
        }
    }
    line.to_string()
}

/// `scheme://user:pass@host` → `scheme://[redacted]@host`.
fn mask_url_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find("://") {
        let auth_start = p + 3;
        out.push_str(&rest[..auth_start]);
        let tail = &rest[auth_start..];
        let end = tail
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(tail.len());
        let authority = &tail[..end];
        match authority.rfind('@') {
            Some(at) if authority[..at].contains(':') => {
                out.push_str(MASK);
                out.push_str(&authority[at..]);
            }
            _ => out.push_str(authority),
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | '/')
}

const PREFIXES: &[&str] = &[
    "sk-",
    "sk_live_",
    "sk_test_",
    "rk_live_",
    "pk_live_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxa-",
    "xoxb-",
    "xoxp-",
    "xoxr-",
    "hf_",
    "ya29.",
    "npm_",
    "pypi-",
    "AIza",
    "AKIA",
    "ASIA",
];

fn secret_word(w: &str, after_bearer: bool) -> bool {
    let w = w.trim_matches(|c: char| c == '.' || c == '/' || c == '-');
    if after_bearer && w.len() >= 8 {
        return true;
    }
    // Identifier-looking words (`hf_hub_download`) share some prefixes: a
    // token has 20+ chars and a digit or a capital after its prefix.
    if PREFIXES.iter().any(|p| {
        w.len() >= 20
            && w.starts_with(p)
            && w[p.len()..]
                .chars()
                .any(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
    }) {
        return true;
    }
    // JWT: three base64url parts, the first one a JSON object.
    if w.starts_with("eyJ") && w.matches('.').count() == 2 && w.len() >= 30 {
        return true;
    }
    w.split(['/', '.']).any(random_looking)
}

/// 32+ chars with upper, lower and digits: an API key, not a word or a digest.
fn random_looking(part: &str) -> bool {
    part.len() >= 32
        && part.chars().any(|c| c.is_ascii_uppercase())
        && part.chars().any(|c| c.is_ascii_lowercase())
        && part.chars().any(|c| c.is_ascii_digit())
}

fn mask_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_word = String::new();
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !is_word(c) {
            out.push(c);
            continue;
        }
        let mut end = i + c.len_utf8();
        while let Some(&(j, d)) = chars.peek() {
            if !is_word(d) {
                break;
            }
            end = j + d.len_utf8();
            chars.next();
        }
        let word = &s[i..end];
        let after_bearer = prev_word.eq_ignore_ascii_case("bearer");
        if secret_word(word, after_bearer) {
            out.push_str(MASK);
        } else {
            out.push_str(word);
        }
        prev_word = word.to_string();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEAKS: &[(&str, &str)] = &[
        ("OPENAI_API_KEY=sk-proj-Ab3dEf6hIj9kLmNoPq", "sk-proj"),
        ("export ANTHROPIC_API_KEY=\"sk-ant-api03-xyzxyzxyz\"", "api03"),
        ("{\"api_key\": \"abc123secretvalue\"}", "abc123"),
        ("password: hunter22", "hunter22"),
        ("Authorization: Bearer abcdefgh12345678", "abcdefgh"),
        ("curl -H 'x: Bearer zzzzzzzzzz9999' x", "zzzzzzzzzz"),
        ("token ghp_ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 used", "ghp_"),
        ("aws AKIAIOSFODNN7EXAMPLE here", "AKIAIOSF"),
        ("slack xoxb-1234-5678-abcdefgh", "xoxb"),
        ("hf hf_AbCdEfGhIjKlMnOpQrStUvWxYz12", "hf_AbC"),
        ("jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.sgn-abcdefghij", "eyJhbG"),
        ("git clone https://user:s3cr3tpw@example.com/r.git", "s3cr3tpw"),
        ("random Zq8mK2pL9vX4nB7cR1tY6wE3uI0oA5sD here", "Zq8mK2"),
        (
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXk\n-----END OPENSSH PRIVATE KEY-----",
            "b3BlbnNz",
        ),
        ("DB_PASS=letmein99", "letmein99"),
        ("gcp ya29.a0AfH6SMBxxxxxxxxxxxxxxx", "ya29.a0"),
        // A tool result is JSON: a file's own JSON arrives escaped.
        (r#"{"output": "{\"password\": \"hunter22\"}"}"#, "hunter22"),
        (r#"{"output": "{\"auth\": \"dXNlcjpwdw==\"}"}"#, "dXNlcjpwdw"),
    ];

    #[test]
    fn every_known_credential_shape_is_masked() {
        for (input, leak) in LEAKS {
            let out = redact(input);
            assert!(!out.contains(leak), "{input:?} -> {out:?}");
            assert!(out.contains(MASK), "{input:?} -> {out:?}");
            assert!(looks_secret(input), "{input:?}");
        }
    }

    #[test]
    fn ordinary_text_passes_unchanged() {
        for s in [
            "fix the failing test in src/main.rs",
            "cargo test --workspace -- --nocapture",
            "https://github.com/x0ne-labs/x0ne-Harness",
            "sha256 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08",
            "key points: tokens, attention, logits",
            "The model's next token is chosen from the distribution.",
            "/home/user/projects/x0ne/src/agent/context.rs",
            "time: 12:30, ratio 3:4",
            "from huggingface_hub import hf_hub_download_with_retries",
        ] {
            assert_eq!(redact(s), s, "{s:?}");
            assert!(!looks_secret(s), "{s:?}");
        }
    }

    #[test]
    fn a_private_key_without_its_end_marker_is_masked_to_the_end() {
        let out = redact("x\n-----BEGIN RSA PRIVATE KEY-----\nMIIEow\nMIIEpA");
        assert_eq!(out, format!("x\n{MASK}"));
    }

    #[test]
    fn a_public_certificate_is_not_a_secret() {
        let s = "-----BEGIN CERTIFICATE-----\nabc\n-----END CERTIFICATE-----";
        assert_eq!(redact(s), s);
    }

    #[test]
    fn multibyte_text_survives() {
        let s = "clé = valeur, 東京 token";
        assert_eq!(redact(s), s);
        assert_eq!(
            redact("mot_de_passe_token=é東"),
            format!("mot_de_passe_token= {MASK}")
        );
    }

    #[test]
    fn argv_secrets_are_masked_and_the_other_arguments_kept() {
        let cases: &[(&[&str], &str)] = &[
            (
                &["mysql", "--password=CANARY-612", "--host=db.local", "app"],
                "mysql --password=[redacted] --host=db.local app",
            ),
            (
                &["deploy", "--api-key", "CANARY-612", "--verbose"],
                "deploy --api-key [redacted] --verbose",
            ),
            (
                &["vault", "-token", "-CANARY-612", "status"],
                "vault -token [redacted] status",
            ),
            (
                &["psql", "postgresql://app:CANARY-612@db.local:5432/app"],
                "psql postgresql://[redacted]@db.local:5432/app",
            ),
            (
                &["java", "-Dssl.keyStorePassword=CANARY-612", "-jar", "a.jar"],
                "java -Dssl.keyStorePassword=[redacted] -jar a.jar",
            ),
            (
                &["env", "AWS_SECRET_ACCESS_KEY=CANARY-612", "aws", "s3", "ls"],
                "env AWS_SECRET_ACCESS_KEY=[redacted] aws s3 ls",
            ),
            (
                &["curl", "-H", "Authorization: Bearer CANARY-612x", "-s"],
                "curl -H Authorization: [redacted] -s",
            ),
            (
                &["gh", "api", "ghp_CANARY612CANARY612CANARY612", "user"],
                "gh api [redacted] user",
            ),
            (
                &["fetch", "https://x/?q=1&token=CANARY-612", "-o", "out"],
                "fetch https://x/?q=1&token= [redacted] -o out",
            ),
            (
                &["mysql", "-uroot", "-pCANARY-612", "app"],
                "mysql -uroot -p[redacted] app",
            ),
            (
                &["sudo", "/usr/bin/mariadb-dump", "-pCANARY-612", "app"],
                "sudo /usr/bin/mariadb-dump -p[redacted] app",
            ),
            (
                &["sshpass", "-p", "CANARY-612", "ssh", "host"],
                "sshpass -p [redacted] ssh host",
            ),
            (
                &["redis-cli", "-a", "CANARY-612", "ping"],
                "redis-cli -a [redacted] ping",
            ),
            (
                &["curl", "-u", "admin:CANARY-612", "https://x"],
                "curl -u admin:[redacted] https://x",
            ),
            (
                &["curl", "-uadmin:CANARY-612", "https://x"],
                "curl -uadmin:[redacted] https://x",
            ),
            (
                &[
                    "curl",
                    "--proxy-user=p:CANARY-612",
                    "--user",
                    "u:CANARY-612",
                ],
                "curl --proxy-user=p:[redacted] --user u:[redacted]",
            ),
            (
                &["curl", "-u", "ghp_CANARY612CANARY612CANARY612:x", "x"],
                "curl -u [redacted]:[redacted] x",
            ),
            (
                &["sh", "-c", "mysql --password CANARY-612 app && echo ok"],
                "sh -c mysql --password [redacted] app && echo ok",
            ),
            (
                &["bash", "-c", "mysql -uroot -pCANARY-612 app"],
                "bash -c mysql -uroot -p[redacted] app",
            ),
            (
                &["tool", "--password", "two words CANARY-612"],
                "tool --password [redacted]",
            ),
        ];
        for &(argv, want) in cases {
            let got = redact_argv(argv).join(" ");
            assert_eq!(got, want, "{argv:?}");
            assert!(!got.contains("CANARY"), "{got}");
        }
    }

    #[test]
    fn ordinary_argv_passes_unchanged() {
        let cases: &[&[&str]] = &[
            &["cargo", "test", "-p", "x0ne-runtime", "--", "--nocapture"],
            &["ssh", "-p", "22", "git@github.com"],
            &["mkdir", "-p", "/tmp/x"],
            &["git", "clone", "https://github.com/x0ne-labs/x0ne-Harness"],
            &["sh", "-c", "read -r line", "--verbose"],
            &["tool", "--password"],
            &["tool", "--password", "--verbose"],
            &["tool", "--password=", "next"],
            &["tool", "--token", "", "next"],
            &["-bash"],
            &["mysql", "-p", "app"],
            &["curl", "-u", "admin", "https://x"],
            &["git", "push", "-u", "origin", "main"],
            &["ps", "-u", "root"],
            &["sh", "-c", "mkdir -p /tmp/x && cd /tmp/x"],
        ];
        for &argv in cases {
            assert_eq!(redact_argv(argv), argv.to_vec(), "{argv:?}");
        }
    }
}
