# Network efficiency

- Minimize service API requests. Seek and adjacent-track navigation must reuse
  audio already in RAM. Share pending loads and resolved stream metadata rather
  than issuing another request for the same track.
- Limit speculative preloading to adjacent tracks. Wave/session IDs belong to
  feedback; they must not prevent reuse of the same track's audio.
- Honor HTTP 429 and Retry-After. Use bounded backoff when the header is absent,
  avoid immediate speculative retries, and preserve cooldowns across restarts.
- Keep persistent caches bounded, private, and scoped to the account and quality
  policy. Give signed URLs a short TTL and detect expiry before reuse. Never log
  credentials, stream keys, or complete signed URLs.
- Verify request counts with local fixtures, including failed prefetch, cache
  hits, expiry, and rate limits. Do not stress live services to test throttling.
