use std::collections::HashMap;

use serde::Deserialize;

const DEFAULT_MAX_BODY_BYTES: u64 = 33_554_432; // 32 MiB
const DEFAULT_HEADER_TIMEOUT_SECS: u64 = 300;

fn default_listen() -> String {
    "127.0.0.1:8551".to_owned()
}

fn default_max_body_bytes() -> u64 {
    DEFAULT_MAX_BODY_BYTES
}

fn default_header_timeout_secs() -> u64 {
    DEFAULT_HEADER_TIMEOUT_SECS
}

fn default_auth_header() -> String {
    "authorization".to_owned()
}

#[derive(Deserialize)]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    pub gateway_keys: Vec<String>,
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: u64,
    #[serde(default = "default_header_timeout_secs")]
    pub header_timeout_secs: u64,
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,
}

#[derive(Deserialize)]
pub struct ProviderConfig {
    pub base_url: String,
    #[serde(default = "default_auth_header")]
    pub auth_header: String,
    pub api_key: String,
}

impl ProviderConfig {
    pub fn auth(&self) -> (String, String) {
        if self.auth_header.eq_ignore_ascii_case("authorization") {
            (
                "authorization".to_owned(),
                format!("Bearer {}", self.api_key),
            )
        } else {
            (self.auth_header.clone(), self.api_key.clone())
        }
    }
}

impl Config {
    pub fn is_loopback(&self) -> bool {
        self.listen.starts_with("127.")
            || self.listen.starts_with("[::1]")
            || self.listen.starts_with("::1")
            || self.listen.starts_with("localhost")
    }

    fn validate(&self) -> Result<(), String> {
        if self.gateway_keys.is_empty() {
            return Err("gateway_keys must not be empty".into());
        }
        for k in &self.gateway_keys {
            if k.len() < 16 {
                return Err("each gateway key must be at least 16 characters".into());
            }
        }
        if self.providers.is_empty() {
            return Err("at least one provider must be configured".into());
        }
        for (name, p) in &self.providers {
            if !p.base_url.starts_with("https://") {
                return Err(format!(
                    "provider `{name}`: base_url must start with https://"
                ));
            }
            if p.api_key.is_empty() {
                return Err(format!("provider `{name}`: api_key must not be empty"));
            }
        }
        Ok(())
    }
}

fn interpolate_env(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        match rest.find('}') {
            Some(end) => {
                let var = &rest[..end];
                out.push_str(&std::env::var(var).unwrap_or_default());
                rest = &rest[end + 1..];
            }
            None => {
                out.push_str("${");
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

pub fn load(path: &str) -> Result<Config, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read config `{path}`: {e}"))?;
    let text = interpolate_env(&raw);
    let cfg: Config = toml::from_str(&text)
        .map_err(|e| format!("failed to parse config: {e}"))?;
    cfg.validate()?;
    Ok(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- env interpolation ------------------------------------------

    #[test]
    fn interpolate_set_var() {
        std::env::set_var("TEST_GW_KEY", "sekrit1234567890");
        assert_eq!(interpolate_env("k=${TEST_GW_KEY}"), "k=sekrit1234567890");
        std::env::remove_var("TEST_GW_KEY");
    }

    #[test]
    fn interpolate_unset_var() {
        std::env::remove_var("DEFINITELY_NOT_SET_XYZ");
        assert_eq!(interpolate_env("k=${DEFINITELY_NOT_SET_XYZ}"), "k=");
    }

    #[test]
    fn interpolate_multiple_and_literal() {
        std::env::set_var("A_VAR", "1");
        std::env::set_var("B_VAR", "2");
        assert_eq!(interpolate_env("${A_VAR}x${B_VAR}y"), "1x2y");
        std::env::remove_var("A_VAR");
        std::env::remove_var("B_VAR");
    }

    #[test]
    fn interpolate_dollar_without_brace() {
        assert_eq!(interpolate_env("price is $5"), "price is $5");
    }

    // -- defaults ---------------------------------------------------

    #[test]
    fn defaults_applied() {
        std::env::set_var("TEST_K", "0123456789abcdef");
        let toml_text = r#"
gateway_keys = ["${TEST_K}"]
[providers.openai]
base_url = "https://api.openai.com"
api_key = "sk-test"
"#;
        let cfg: Config = toml::from_str(&interpolate_env(toml_text)).unwrap();
        assert_eq!(cfg.listen, "127.0.0.1:8551");
        assert_eq!(cfg.max_body_bytes, DEFAULT_MAX_BODY_BYTES);
        assert_eq!(cfg.header_timeout_secs, DEFAULT_HEADER_TIMEOUT_SECS);
        assert_eq!(cfg.providers["openai"].auth_header, "authorization");
        std::env::remove_var("TEST_K");
    }

    // -- auth header semantics --------------------------------------

    #[test]
    fn auth_bearer_prefix_for_default() {
        let p = ProviderConfig {
            base_url: "https://example.com".into(),
            auth_header: "authorization".into(),
            api_key: "sk-test".into(),
        };
        assert_eq!(p.auth(), ("authorization".into(), "Bearer sk-test".into()));
    }

    #[test]
    fn auth_raw_for_custom_header() {
        let p = ProviderConfig {
            base_url: "https://example.com".into(),
            auth_header: "x-api-key".into(),
            api_key: "sk-test".into(),
        };
        assert_eq!(p.auth(), ("x-api-key".into(), "sk-test".into()));
    }

    // -- validation: bad URL rejection ------------------------------

    #[test]
    fn reject_non_https_base_url() {
        let cfg: Config = toml::from_str(
            r#"
gateway_keys = ["0123456789abcdef"]
[providers.bad]
base_url = "http://insecure.com"
api_key = "sk-test"
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn reject_short_gateway_key() {
        let cfg: Config = toml::from_str(
            r#"
gateway_keys = ["short"]
[providers.openai]
base_url = "https://api.openai.com"
api_key = "sk-test"
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn reject_empty_providers() {
        let cfg: Config =
            toml::from_str(r#"gateway_keys = ["0123456789abcdef"]"#).unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn reject_empty_api_key() {
        let cfg: Config = toml::from_str(
            r#"
gateway_keys = ["0123456789abcdef"]
[providers.openai]
base_url = "https://api.openai.com"
api_key = ""
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_err());
    }

    // -- loopback detection -----------------------------------------

    #[test]
    fn loopback_detection() {
        let mk = |listen: &str| Config {
            listen: listen.into(),
            gateway_keys: vec!["0123456789abcdef".into()],
            max_body_bytes: DEFAULT_MAX_BODY_BYTES,
            header_timeout_secs: DEFAULT_HEADER_TIMEOUT_SECS,
            providers: HashMap::new(),
        };
        assert!(mk("127.0.0.1:8551").is_loopback());
        assert!(mk("[::1]:8551").is_loopback());
        assert!(!mk("0.0.0.0:8551").is_loopback());
    }

    // -- eek! -------------------------------------------------------

    #[test]
    fn valid_config_passes_validation() {
        let cfg: Config = toml::from_str(
            r#"
gateway_keys = ["0123456789abcdef"]
[providers.openai]
base_url = "https://api.openai.com"
api_key = "sk-test"
"#,
        )
        .unwrap();
        assert!(cfg.validate().is_ok());
    }
}
