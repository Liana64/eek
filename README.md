# Eek!

Tiny (like a mouse) key-brokering proxy for LLM providers, configurable using
TOML/ENV. Limited to chat/completions, messages, and responses with upstream
requests over TLS. Inbound requests to the proxy are cleartext, so deploy on
kubernetes with TLS-terminating ingress (and probably a networkpolicy).

Dependencies are: hyper, rustls, and tokio. Grrr, no npm.

## Deployment

```sh
eek config.toml                        # or EEK_CONFIG=config.toml
```

## Config

See `config.example.toml`

```toml
listen = "127.0.0.1:8551"
gateway_keys = ["${GATEWAY_KEY}"]      # clients send Authorization: Bearer <key>

[providers.anthropic]
base_url = "https://api.anthropic.com" # https only
auth_header = "x-api-key"              # authorization sent as Bearer <api_key>
api_key = "${ANTHROPIC_API_KEY}"
```

Gateway key(s) can be any string longer than sixteen characters. We recommend
the use of `openssl rand -hex 32`.

## Routes

- `POST /<provider>/v1/{chat/completions,messages,responses}` and eek! swaps
your gateway key for the provider's key and forwards the request.

- `GET /healthz` returns `ok`.
