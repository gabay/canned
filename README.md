# canned

A minimal, high-performance static HTTP mock/stub server written in Rust, configured via a single YAML file.

## Features

- **Fast in-memory matching**: The YAML configuration is parsed once at startup into hash maps for O(1) matching.
- **Priority order**: Resolves rules in order: **`fullurl`** &rarr; **`host`** &rarr; **`path`** &rarr; **default / fallback**.
- **First match wins**: If multiple rules exist within the same tier (e.g. multiple `path: /robots.txt`), the first one defined in the YAML file takes precedence.
- **Configurable responses**: Custom HTTP status code, headers (as a dictionary/map), and static body content.
- **Minimal Docker image**: Multi-stage build producing a completely static binary running on `FROM scratch`.
- **Configurable via environment variables**:
  - `CONFIG` (or `CANNED_CONFIG`): path to the YAML file (defaults to `canned.yaml`).
  - `PORT` (or `CANNED_PORT`): port to listen on (defaults to `7890`).

---

## Configuration (`canned.yaml`)

The configuration is organized hierarchically by matching priority tiers: **`fullUrl`** &rarr; **`host`** &rarr; **`path`** &rarr; **`default`**.
Headers are configured as a dictionary (`key: value`). Multiple values for the same header can be provided as a YAML list under that key.

```yaml
# 1. Full URL matching (highest priority)
fullUrl:
  http://example.com/health:
    status: 200
    headers:
      Content-Type: application/json
      X-Custom: "custom-header-value"
    content: '{"status":"healthy"}'

# 2. Host matching (evaluated if no fullUrl matches)
host:
  example.com:
    status: 302
    headers:
      Location: https://www.example.com

# 3. Path matching (evaluated if no fullUrl or host matches)
path:
  /robots.txt:
    status: 200
    headers:
      Content-Type: text/plain
    content: "Disallow: *"

# 4. Default / fallback response (evaluated if no rules match)
default:
  status: 404
  headers:
    Content-Type: text/plain
  content: "Not Found"
```

---

## Docker

### Build

```bash
docker build -t canned .
```

### Run

```bash
docker run -p 7890:7890 \
  -e CONFIG=/canned.yaml \
  -v $(pwd)/canned.yaml:/canned.yaml:ro \
  canned
```

Or specify a custom port:

```bash
docker run -p 8080:8080 \
  -e PORT=8080 \
  -e CONFIG=/canned.yaml \
  -v $(pwd)/canned.yaml:/canned.yaml:ro \
  canned
```

---

## Running locally

```bash
CONFIG=canned.yaml PORT=7890 cargo run --release
```
