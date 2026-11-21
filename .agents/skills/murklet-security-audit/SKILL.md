---
name: murklet-security-audit
description: >-
  Security boundary audit checklist for Murklet as an image proxy. Use whenever
  implementing or reviewing origin adapters, HTTP fetching, filesystem access,
  request signing, URL parsing, policy enforcement, or image decoding.
---

# Murklet Security Audit Checklist

Murklet processes untrusted network inputs and image bytes. Audit every change touching security boundaries against the controls below.

## 1. HTTP(S) origin and SSRF defences

- Enforce explicit origin allowlists; never enable arbitrary public URL fetching by default.
- Reject non-`http`/`https` schemes and embedded credentials in URLs.
- Block loopback (`127.0.0.0/8`, `::1`), RFC 1918 private ranges, link-local / cloud metadata (`169.254.0.0/16`, `fe80::/10`), CGNAT (`100.64.0.0/10`), unique local IPv6 (`fc00::/7`), and IPv4-mapped IPv6 addresses (`::ffff:0:0/96`).
- Prevent DNS rebinding: resolve DNS once, validate the resolved IP(s), and pin the validated IP at connection time rather than letting the HTTP client re-resolve.
- Validate every redirect hop against the same allowlist and IP rules, and cap redirect count.

## 2. Local filesystem origin

- Reject null bytes, absolute paths, and `..` traversal segments before path resolution.
- Canonicalise both root and target paths and verify the resolved target remains strictly inside the configured root directory (preventing symlink escapes).
- Reject non-regular files (directories, FIFOs, device nodes).

## 3. Image decoding and resource exhaustion

- Enforce `max_input_bytes` while streaming from the origin (never buffer an unbounded response body).
- Inspect image headers and enforce `max_decoded_pixels` and dimension limits *before* allocating full pixel buffers (decompression bomb defence).
- Enforce upstream fetch timeouts and bounded concurrency to prevent thread/task starvation.
- Ensure animated inputs follow the documented fallback policy (ADR 0001) without unbounded frame decoding.

## 4. Authentication, signing, and error hygiene

- Compare request signatures (HMAC) and administrative tokens (such as cache cleanup tokens) strictly in constant time, and fail closed when a secret is empty or unconfigured.
- Map internal and upstream failures to clean HTTP status codes (`400`, `401`, `403`, `404`, `413`, `415`, `502`, `504`) without leaking internal file paths, stack details, or raw upstream response bodies.
- Verify sensitive headers or query tokens are never logged or embedded in cache keys/metrics labels.

## Output format

Report findings by severity (**Critical**, **High**, **Medium**, **Note**) with concrete exploit scenarios and minimal remediation steps.
