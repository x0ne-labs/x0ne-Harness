//! Which model key the session sends, and to which host. The table
//! is [`KEYS`], shared with the runtime's `auth` (`x0ne_actions::model_keys`):
//! a row per vendor variable that a key is read from when the config
//! names none. Adding a vendor is adding a row there.
//!
//! - A key read from a row's variable without the config naming it goes to
//!   the row's provider or hosts only. Anywhere else it is refused before
//!   any request, and the refusal says how to name it.
//! - A key the config names (`api_key`, `api_key_env`, or
//!   `X0NE_OPENAI_COMPAT_KEY`) goes to the config's host, and [`key_note`]
//!   says so at startup.
//! - A key that holds a pinned row's prefix goes through its provider only,
//!   named or not; a row's provider and hosts take no key of another format.

use anyhow::{bail, Result};

use x0ne_actions::model_keys::{refusal, Refusal};
pub use x0ne_actions::model_keys::{row, KeyRow, KEYS};

use crate::config::LlmConfig;
use crate::llm_quickstart::{mask_key, ANTHROPIC_BASE, OPENAI_BASE};
use crate::redact::url_for_display;

/// The openai-compat key of the harness: it serves nothing else, so the
/// config names it by default.
pub const COMPAT_VAR: &str = "X0NE_OPENAI_COMPAT_KEY";

/// Where a key was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// Handed in by the caller: pasted at the setup question, or found by
    /// its prefix at startup.
    Given,
    /// `api_key` in the config.
    Config,
    /// A variable the config names in `api_key_env`, or [`COMPAT_VAR`].
    Named(String),
    /// A vendor's variable, read because the config names none.
    Implicit(&'static str),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelKey {
    pub value: String,
    pub source: Source,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Target {
    Anthropic,
    OpenAi,
    Ollama,
    Compat,
    Keyless,
}

fn target(provider: &str) -> Target {
    match provider {
        "ollama" => Target::Ollama,
        "openai" => Target::OpenAi,
        "openai-compat" | "openai_compat" | "litellm" => Target::Compat,
        "claude-code" | "codex" | "claude-max" | "anthropic-oauth" => Target::Keyless,
        p if crate::engine_local::is_engine_provider(p) => Target::Keyless,
        _ => Target::Anthropic,
    }
}

impl Target {
    fn provider(self) -> &'static str {
        match self {
            Target::Anthropic => "anthropic",
            Target::OpenAi => "openai",
            Target::Ollama => "ollama",
            Target::Compat => "openai-compat",
            Target::Keyless => "",
        }
    }
    fn section(self) -> &'static str {
        match self {
            Target::Anthropic => "[llm.anthropic]",
            Target::OpenAi => "[llm.openai]",
            Target::Ollama => "[llm.ollama]",
            Target::Compat => "[llm.openai_compat]",
            Target::Keyless => "[llm]",
        }
    }
}

/// The endpoint the active provider of `llm` calls.
fn base_of(llm: &LlmConfig) -> &str {
    match target(&llm.provider) {
        Target::Anthropic => ANTHROPIC_BASE,
        Target::OpenAi => OPENAI_BASE,
        Target::Ollama => &llm.ollama.endpoint,
        Target::Compat => &llm.openai_compat.base_url,
        Target::Keyless => "",
    }
}

fn unnamed(var: &'static str) -> Source {
    match row(var) {
        Some(r) => Source::Implicit(r.var),
        None => Source::Named(var.to_string()),
    }
}

/// The key the active provider of `llm` would send, and where it was read;
/// nothing is checked. `api_key_env` unset reads the provider's default
/// variable; `api_key_env = ""` reads none.
fn find(
    llm: &LlmConfig,
    given: Option<String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Option<ModelKey> {
    let read = |var: &str| {
        env(var)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let key = |value: String, source: Source| Some(ModelKey { value, source });
    let from_var = |named: &Option<String>, default: &'static str| match named.as_deref() {
        Some("") => None,
        Some(var) => read(var).and_then(|v| key(v, Source::Named(var.to_string()))),
        None => read(default).and_then(|v| key(v, unnamed(default))),
    };
    match target(&llm.provider) {
        Target::Keyless => None,
        Target::Ollama => from_var(&llm.ollama.api_key_env, "OLLAMA_API_KEY"),
        Target::OpenAi => from_var(&llm.openai.api_key_env, "OPENAI_API_KEY"),
        Target::Anthropic => {
            let named = llm.anthropic.api_key_env.as_deref();
            if let Some(k) = given
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty())
            {
                return key(k, Source::Given);
            }
            if let Some(v) = read("ANTHROPIC_API_KEY") {
                let source = if named == Some("ANTHROPIC_API_KEY") {
                    Source::Named("ANTHROPIC_API_KEY".into())
                } else {
                    Source::Implicit("ANTHROPIC_API_KEY")
                };
                return key(v, source);
            }
            match named {
                Some(var) if !var.is_empty() => {
                    read(var).and_then(|v| key(v, Source::Named(var.into())))
                }
                _ => None,
            }
        }
        Target::Compat => {
            let c = &llm.openai_compat;
            if let Some(v) = read(COMPAT_VAR) {
                return key(v, Source::Named(COMPAT_VAR.into()));
            }
            if let Some(k) = c
                .api_key
                .as_deref()
                .map(str::trim)
                .filter(|k| !k.is_empty())
            {
                return key(k.to_string(), Source::Config);
            }
            match c.api_key_env.as_deref() {
                Some("") => return None,
                Some(var) => {
                    if let Some(v) = read(var) {
                        return key(v, Source::Named(var.into()));
                    }
                }
                None => {}
            }
            // The fallback of the harness since 0.1: OpenAI's own variable.
            read("OPENAI_API_KEY").and_then(|v| key(v, unnamed("OPENAI_API_KEY")))
        }
    }
}

/// Refuse to send `key`, read from `source`, to `provider` at `base_url`
/// when [`KEYS`] says it does not go there. Called before any request.
pub fn check(key: &str, source: &Source, provider: &str, base_url: &str) -> Result<()> {
    let t = target(provider);
    let implicit = match source {
        Source::Implicit(var) => Some(*var),
        _ => None,
    };
    let Some(why) = refusal(key, implicit, t.provider(), base_url) else {
        return Ok(());
    };
    let shown = match x0ne_actions::model_keys::host_of(base_url) {
        Some((host, _)) if !host.is_empty() => host,
        _ => url_for_display(base_url),
    };
    let k = key.trim();
    match why {
        Refusal::Pinned(r) => bail!(
            "refusing to send {a_key} ({}) to {shown} (provider {provider}): {a_key} goes \
             only to {}, through provider {p}. Use provider {p}.",
            mask_key(k),
            r.hosts.join(", "),
            a_key = r.a_key(),
            p = r.provider.unwrap_or_default(),
        ),
        Refusal::Format(r) => bail!(
            "refusing to send key {} to {}: {a_key} starts with {}. Set {} to {a_key}, \
             or choose the provider that issued this one.",
            mask_key(k),
            if r.provider == Some(t.provider()) {
                r.hosts[0]
            } else {
                shown.as_str()
            },
            r.prefix,
            r.var,
            a_key = r.a_key(),
        ),
        Refusal::Implicit(r) => bail!(
            "refusing to send {var} ({}) to {shown}: the config does not name this key, \
             and read so it goes only to {} over https. To send it to {shown}, name the key \
             in the config: api_key_env = \"{var}\" under {}. For a server that takes no \
             key: api_key_env = \"\".",
            mask_key(k),
            r.hosts.join(", "),
            t.section(),
            var = r.var,
        ),
    }
}

/// The key the active provider of `llm` sends, checked by [`check`].
/// `given`: a key handed in by the caller, used by provider anthropic.
pub fn resolve(llm: &LlmConfig, given: Option<String>) -> Result<Option<String>> {
    Ok(resolve_from(llm, given, &|k| std::env::var(k).ok())?.map(|k| k.value))
}

/// [`resolve`], `env` reading a variable.
pub fn resolve_from(
    llm: &LlmConfig,
    given: Option<String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ModelKey>> {
    let Some(key) = find(llm, given, env) else {
        return Ok(None);
    };
    check(&key.value, &key.source, &llm.provider, base_of(llm))?;
    Ok(Some(key))
}

/// "key from OPENAI_API_KEY → host:port" when the config names the key the
/// session sends; `None` for an implicit key, a refused one, or none. Each
/// surface shows it once at startup; never the key.
pub fn key_note(llm: &LlmConfig) -> Option<String> {
    key_note_from(llm, &|k| std::env::var(k).ok())
}

/// [`key_note`] on stderr, for a surface that starts on a plain terminal.
pub fn announce(llm: &LlmConfig) {
    if let Some(note) = key_note(llm) {
        eprintln!("  {note}");
    }
}

/// [`key_note`], `env` reading a variable.
pub fn key_note_from(llm: &LlmConfig, env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let key = resolve_from(llm, None, env).ok()??;
    let from = match key.source {
        Source::Config => format!("api_key in {}", target(&llm.provider).section()),
        Source::Named(var) => var,
        Source::Given | Source::Implicit(_) => return None,
    };
    let base = base_of(llm);
    let place = reqwest::Url::parse(base.trim())
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_string();
            Some(match u.port_or_known_default() {
                Some(p) => format!("{host}:{p}"),
                None => host,
            })
        })
        .unwrap_or_else(|| url_for_display(base));
    Some(format!("key from {from} → {place}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANT: &str = "sk-ant-api03-FAKE-abcd";
    const OAI: &str = "sk-proj-FAKE-1009-wxyz";

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    }

    fn llm(provider: &str, base: &str) -> LlmConfig {
        let mut l = LlmConfig {
            provider: provider.into(),
            ..LlmConfig::default()
        };
        l.ollama.endpoint = base.into();
        l.openai_compat.base_url = base.into();
        l
    }

    fn refusal(l: &LlmConfig, e: &dyn Fn(&str) -> Option<String>) -> String {
        format!(
            "{:#}",
            resolve_from(l, None, e).expect_err("the key must be refused")
        )
    }

    /// Where `row`'s variable is read without the config naming it, at the
    /// vendor's own endpoint; `true` when that endpoint can be moved.
    fn official(row: &KeyRow) -> (LlmConfig, bool) {
        match row.var {
            "ANTHROPIC_API_KEY" => (llm("anthropic", ""), false),
            "OPENAI_API_KEY" => (llm("openai-compat", "https://api.openai.com/v1"), true),
            "OLLAMA_API_KEY" => (llm("ollama", "https://ollama.com"), true),
            other => panic!("{other}: a new row needs its case here"),
        }
    }

    /// One row: its variable, read implicitly, reaches its own host; at a
    /// loopback host it is refused before any request, with the line to add
    /// and no more of the key than its last four characters.
    fn assert_the_row_keeps_its_key_home(var: &str, key: &str) {
        let r = row(var).expect("a row");
        let e = env(&[(var, key)]);
        let (home, movable) = official(r);
        let sent = resolve_from(&home, None, &e).expect("its own host");
        assert_eq!(
            sent,
            Some(ModelKey {
                value: key.into(),
                source: Source::Implicit(r.var)
            }),
            "{var}"
        );
        assert_eq!(
            key_note_from(&home, &e),
            None,
            "an implicit key is not announced"
        );
        let moved = |base: &str| {
            let mut l = home.clone();
            l.ollama.endpoint = base.into();
            l.openai_compat.base_url = base.into();
            l
        };
        let msg = if movable {
            refusal(&moved("http://127.0.0.1:9"), &e)
        } else {
            // A fixed endpoint: the key reaches another host only through
            // another provider.
            let away = check(
                key,
                &Source::Implicit(r.var),
                "openai-compat",
                "http://127.0.0.1:9",
            );
            format!("{:#}", away.expect_err("refused"))
        };
        assert!(msg.contains("127.0.0.1"), "{var}: {msg}");
        assert!(
            !msg.contains(&key[..key.len() - 4]),
            "{var}: the key shows: {msg}"
        );
        if !r.pinned {
            assert!(
                msg.contains(&format!(
                    "name the key in the config: api_key_env = \"{var}\""
                )),
                "{var}: {msg}"
            );
        }
        if movable {
            let plain = moved(&format!("http://{}", r.hosts[0]));
            assert!(resolve_from(&plain, None, &e).is_err(), "{var} over http");
        }
    }

    #[test]
    fn the_anthropic_row_keeps_its_key_home() {
        assert_the_row_keeps_its_key_home("ANTHROPIC_API_KEY", ANT);
    }

    #[test]
    fn the_openai_row_keeps_its_key_home() {
        assert_the_row_keeps_its_key_home("OPENAI_API_KEY", OAI);
    }

    #[test]
    fn the_ollama_row_keeps_its_key_home() {
        assert_the_row_keeps_its_key_home("OLLAMA_API_KEY", "ollama-FAKE-1009-key");
    }

    #[test]
    fn every_row_has_its_test() {
        let tested = ["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "OLLAMA_API_KEY"];
        let rows: Vec<&str> = KEYS.iter().map(|r| r.var).collect();
        assert_eq!(
            rows, tested,
            "a new row needs its own `..._row_keeps_its_key_home`"
        );
    }

    #[test]
    fn a_named_key_reaches_the_config_host_and_is_announced() {
        let e = env(&[("OPENAI_API_KEY", OAI)]);
        let mut l = llm("openai-compat", "http://127.0.0.1:4000/v1");
        l.openai_compat.api_key_env = Some("OPENAI_API_KEY".into());
        let k = resolve_from(&l, None, &e).unwrap().unwrap();
        assert_eq!(k.source, Source::Named("OPENAI_API_KEY".into()));
        let note = key_note_from(&l, &e).unwrap();
        assert_eq!(note, "key from OPENAI_API_KEY → 127.0.0.1:4000");
        assert!(!note.contains("wxyz"));
        let mut o = llm("ollama", "http://gpu-box:11434");
        o.ollama.api_key_env = Some("OLLAMA_API_KEY".into());
        let e = env(&[("OLLAMA_API_KEY", "ollama-FAKE-1009-key")]);
        assert_eq!(
            key_note_from(&o, &e).unwrap(),
            "key from OLLAMA_API_KEY → gpu-box:11434"
        );
    }

    #[test]
    fn the_compat_key_of_the_harness_and_an_inline_key_are_named() {
        let mut l = llm("openai-compat", "https://api.groq.com/openai");
        let e = env(&[(COMPAT_VAR, "gsk_FAKE_1009")]);
        assert_eq!(
            key_note_from(&l, &e).unwrap(),
            "key from X0NE_OPENAI_COMPAT_KEY → api.groq.com:443"
        );
        l.openai_compat.api_key = Some("gsk_FAKE_inline".into());
        let none = env(&[]);
        assert_eq!(
            key_note_from(&l, &none).unwrap(),
            "key from api_key in [llm.openai_compat] → api.groq.com:443"
        );
    }

    #[test]
    fn an_empty_api_key_env_sends_no_key() {
        let e = env(&[("OPENAI_API_KEY", OAI), ("OLLAMA_API_KEY", "ollama-FAKE")]);
        let mut c = llm("openai-compat", "http://127.0.0.1:4000");
        c.openai_compat.api_key_env = Some(String::new());
        assert_eq!(resolve_from(&c, None, &e).unwrap(), None);
        let mut o = llm("ollama", "http://localhost:11434");
        o.ollama.api_key_env = Some(String::new());
        assert_eq!(resolve_from(&o, None, &e).unwrap(), None);
    }

    #[test]
    fn local_ollama_without_a_key_is_unchanged() {
        let l = llm("ollama", "http://localhost:11434");
        assert_eq!(resolve_from(&l, None, &env(&[])).unwrap(), None);
    }

    #[test]
    fn an_anthropic_key_never_leaves_anthropic_even_named() {
        let e = env(&[("MY_KEY", ANT), (COMPAT_VAR, ANT)]);
        let mut named = llm("openai-compat", "https://openrouter.ai/api");
        named.openai_compat.api_key_env = Some("MY_KEY".into());
        let msg = refusal(&named, &env(&[("MY_KEY", ANT)]));
        assert!(msg.contains("refusing to send an Anthropic key"), "{msg}");
        assert!(!msg.contains("FAKE"), "{msg}");
        let mut inline = llm("openai-compat", "https://api.anthropic.com/v1");
        inline.openai_compat.api_key = Some(ANT.into());
        assert!(resolve_from(&inline, None, &env(&[])).is_err());
        let mut ollama = llm("ollama", "https://api.anthropic.com");
        ollama.ollama.api_key_env = Some("MY_KEY".into());
        assert!(resolve_from(&ollama, None, &e).is_err());
        let mut openai = llm("openai", "");
        openai.openai.api_key_env = Some("MY_KEY".into());
        assert!(resolve_from(&openai, None, &e).is_err());
        assert!(resolve_from(&llm("anthropic", ""), Some(ANT.into()), &e).is_ok());
    }

    #[test]
    fn an_anthropic_key_behind_a_decoration_stays_pinned() {
        for key in [
            format!("Bearer {ANT}"),
            format!("\u{feff}{ANT}"),
            format!("\u{200b}{ANT}"),
            format!("x{ANT}"),
        ] {
            for source in [
                Source::Config,
                Source::Given,
                Source::Named(COMPAT_VAR.into()),
            ] {
                for (p, base) in [
                    ("openai-compat", "http://127.0.0.1:4000"),
                    ("ollama", "http://127.0.0.1:11434"),
                    ("openai", OPENAI_BASE),
                ] {
                    let e = check(&key, &source, p, base).expect_err(p);
                    let msg = format!("{e:#}");
                    assert!(msg.contains("an Anthropic key"), "{key:?} {p}: {msg}");
                    assert!(!msg.contains("FAKE"), "{msg}");
                }
            }
        }
    }

    #[test]
    fn an_anthropic_key_is_never_sent_elsewhere() {
        let e = check(
            ANT,
            &Source::Given,
            "openai-compat",
            "https://api.openai.com",
        )
        .unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("api.openai.com") && msg.contains("anthropic"),
            "{msg}"
        );
        assert!(
            !msg.contains("FAKE"),
            "the refusal must not print the key: {msg}"
        );
        assert!(check(ANT, &Source::Given, "openai", OPENAI_BASE).is_err());
        assert!(check(ANT, &Source::Given, "ollama", "http://localhost:11434").is_err());
        assert!(check(
            ANT,
            &Source::Given,
            "openai-compat",
            "https://openrouter.ai/api"
        )
        .is_err());
        assert!(check(ANT, &Source::Given, "anthropic", ANTHROPIC_BASE).is_ok());
    }

    #[test]
    fn a_foreign_key_is_never_sent_to_anthropic_or_openai() {
        let given = |k: &str, p: &str, b: &str| check(k, &Source::Given, p, b);
        assert!(given("sk-proj-abcd", "anthropic", ANTHROPIC_BASE).is_err());
        assert!(given(
            "sk-proj-abcd",
            "openai-compat",
            "https://api.anthropic.com/v1"
        )
        .is_err());
        assert!(given("gsk_abcd", "openai", OPENAI_BASE).is_err());
        assert!(given("gsk_abcd", "openai-compat", "https://api.openai.com/v1").is_err());
        assert!(given("sk-or-v1-abcd", "openai", OPENAI_BASE).is_err());
        assert!(given("sk-proj-abcd", "openai", OPENAI_BASE).is_ok());
        // Third-party compat endpoints keep their own key formats.
        assert!(given("gsk_abcd", "openai-compat", "https://api.groq.com/openai").is_ok());
    }

    #[test]
    fn the_host_is_the_one_the_client_connects_to() {
        // WHATWG reads `\` as `/`: the host is evil.example, whatever follows.
        let e = env(&[("OPENAI_API_KEY", OAI)]);
        for base in [
            "https://evil.example\\@api.openai.com/v1",
            "https://api.openai.com@evil.example/v1",
            "https://api.openai.com.evil.example/v1",
            "api.openai.com",
        ] {
            assert!(
                resolve_from(&llm("openai-compat", base), None, &e).is_err(),
                "{base}"
            );
        }
        assert!(resolve_from(
            &llm("openai-compat", "https://API.OpenAI.com:443/v1"),
            None,
            &e
        )
        .is_ok());
    }

    #[test]
    fn a_relay_and_a_removed_backend_read_no_key() {
        let e = env(&[("ANTHROPIC_API_KEY", ANT), ("OPENAI_API_KEY", OAI)]);
        for p in ["x0ne-engine", "claude-max", "codex"] {
            assert_eq!(resolve_from(&llm(p, ""), None, &e).unwrap(), None, "{p}");
        }
    }
}
