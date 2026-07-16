# Eek!

Tiny (like a mouse) LLM proxy, configurable using TOML and ENV. Limited to chat/completions, messages, and responses with requests over TLS. Dependencies are: hyper, rustls, and tokio. Grrr, no npm.

We recommend that you deploy on Kubernetes and apply a networkpolicy. Or don't, whatever.

## Deployment

```sh
eek config.toml                          # or EEK_CONFIG=config.toml
```

## Config

See `config.example.toml`

```toml
listen = "127.0.0.1:8551"
gateway_keys = ["${GATEWAY_KEY}"]        # clients send Authorization: Bearer <key>

[providers.anthropic]
base_url = "https://api.anthropic.com"   # https only
auth_header = "x-api-key"                # default "authorization" (sent as Bearer <api_key>)
api_key = "${ANTHROPIC_API_KEY}"
```

## Routes

- `POST /<provider>/v1/{chat/completions,messages,responses}` and eek! swaps your gateway key for the provider's key and forwards the request.
- `GET /healthz` returns `ok`.

MIT.
