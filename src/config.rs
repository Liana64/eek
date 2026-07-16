use std::collections::BTreeMap;
use std::net::SocketAddr;

use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "defaults::listen")]
    pub listen: SocketAddr,
    pub gateway_keys: Vec<String>,
    #[serde(default = "defaults::max_body_bytes")]
    pub max_body_bytes: usize,
    #[serde(default = "defaults::header_timeout_secs")]
    pub header_timeout_secs: u64,
    pub providers: BTreeMap<String, Provider>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provider {
    pub base_url: String,
    #[serde(default = "defaults::auth_header")]
    pub auth_header: String,
    pub api_key: String,
}

mod defaults {
    use std::net::SocketAddr;

    pub fn listen() -> SocketAddr {
        ([127, 0, 0, 1], 8551).into()
    }

    pub fn max_body_bytes() -> usize {
        32 * 1024 * 1024
    }

    pub fn header_timeout_secs() -> u64 {
        300
    }

    pub fn auth_header() -> String {
        "authorization".into()
    }
}

pub fn load(path: &str) -> Result<Config, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    parse(&raw).map_err(|e| format!("{path}: {e}"))
}

fn parse(raw: &str) -> Result<Config, String> {
    let mut cfg: Config = toml::from_str(raw).map_err(|e| e.to_string())?;
    for k in &mut cfg.gateway_keys {
        *k = interpolate(k)?;
    }
    for p in cfg.providers.values_mut() {
        for s in [&mut p.base_url, &mut p.auth_header, &mut p.api_key] {
            *s = interpolate(s)?;
        }
    }
    validate(&cfg)?;
    Ok(cfg)
}

fn interpolate(raw: &str) -> Result<String, String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(i) = rest.find("${") {
        out.push_str(&rest[..i]);
        let end = rest[i..].find('}').ok_or("unclosed ${")? + i;
        let var = &rest[i + 2..end];
        out.push_str(&std::env::var(var).map_err(|_| format!("${{{var}}}: not set"))?);
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn validate(cfg: &Config) -> Result<(), String> {
    if cfg.gateway_keys.is_empty() {
        return Err("gateway_keys: at least one key required".into());
    }
    if cfg.gateway_keys.iter().any(|k| k.len() < 16) {
        return Err("gateway_keys: keys must be 16+ chars".into());
    }
    if cfg.providers.is_empty() {
        return Err("providers: at least one required".into());
    }
    for (name, p) in &cfg.providers {
        if p.base_url
            .strip_prefix("https://")
            .is_none_or(str::is_empty)
        {
            return Err(format!("providers.{name}: base_url must be https://..."));
        }
        match p.base_url.parse::<hyper::Uri>() {
            Ok(u) if u.query().is_none() => {}
            _ => return Err(format!("providers.{name}: base_url invalid")),
        }
        if p.api_key.is_empty() {
            return Err(format!("providers.{name}: api_key empty"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err<T>(r: Result<T, String>) -> String {
        match r {
            Err(e) => e,
            Ok(_) => panic!("expected error"),
        }
    }

    const MINIMAL: &str = r#"
        gateway_keys = ["0123456789abcdef"]
        [providers.anthropic]
        base_url = "https://api.anthropic.com"
        api_key = "sk-test"
    "#;

    #[test]
    fn minimal_config_gets_defaults() {
        let cfg = parse(MINIMAL).unwrap();
        assert_eq!(cfg.listen, ([127, 0, 0, 1], 8551).into());
        assert_eq!(cfg.max_body_bytes, 32 * 1024 * 1024);
        assert_eq!(cfg.header_timeout_secs, 300);
        assert_eq!(cfg.providers["anthropic"].auth_header, "authorization");
    }

    #[test]
    fn interpolates_env_vars() {
        std::env::set_var("AI_GW_TEST_KEY", "sk-from-env");
        let cfg = parse(&MINIMAL.replace("sk-test", "${AI_GW_TEST_KEY}")).unwrap();
        assert_eq!(cfg.providers["anthropic"].api_key, "sk-from-env");
    }

    #[test]
    fn missing_env_var_errors() {
        let e = err(parse(&MINIMAL.replace("sk-test", "${AI_GW_TEST_UNSET}")));
        assert!(e.contains("AI_GW_TEST_UNSET"));
    }

    #[test]
    fn unclosed_interpolation_errors() {
        assert!(err(parse(&MINIMAL.replace("sk-test", "${OOPS"))).contains("unclosed"));
    }

    #[test]
    fn ignores_interpolation_outside_string_fields() {
        parse(&format!("# ${{AI_GW_TEST_UNSET}}\n{MINIMAL}")).unwrap();
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(parse(&format!("{MINIMAL}\nwat = 1")).is_err());
    }

    #[test]
    fn rejects_short_gateway_keys() {
        assert!(err(parse(&MINIMAL.replace("0123456789abcdef", "short"))).contains("16+"));
    }

    #[test]
    fn rejects_non_https_base_url() {
        let raw = MINIMAL.replace("https://api.anthropic.com", "http://api.anthropic.com");
        assert!(err(parse(&raw)).contains("https"));
    }

    #[test]
    fn rejects_empty_providers() {
        let raw = "gateway_keys = [\"0123456789abcdef\"]\n[providers]";
        assert!(err(parse(raw)).contains("providers"));
    }
}
