# Service setup

Connection profiles are saved with permissions 0600 in `~/.config/plavno`
(or `$XDG_CONFIG_HOME/plavno`). Paste credentials into the hidden login prompt;
keep cookie files private with `chmod 600 FILE`. Restart plavno after profile changes.
Service integrations use unofficial APIs and depend on account access.

## Yandex Music

Run `plavno login yandex` (`plavno login` is an alias) and paste an OAuth token
from the [token guide](https://ym.marshal.dev/token). `YANDEX_MUSIC_TOKEN` overrides
`yandex.token`. Full tracks require appropriate account access / Yandex Plus.
The player requests lossless and falls back to the best available supported
format; the TUI shows the actual quality.

Use `plavno --source yandex` for My Vibe or `plavno --source yandex --favorites`
for library shuffle. Yandex Music is also available in the Sources menu and Wave Shuffle.

For connection errors, run `plavno diagnose yandex`. It checks account access,
favorites, stream resolution and a 1 KiB audio CDN download without playing
or changing your library. The report omits credentials and signed audio URLs.

Yandex stream metadata is cached for 5 minutes (up to 64 tracks per token and
quality policy) in `$XDG_CACHE_HOME/plavno/yandex-streams`, or
`~/.cache/plavno/yandex-streams`. Files use permissions 0600; they contain signed
URLs and stream keys, so keep them private. A 1-byte CDN probe checks cached links
before reuse; expired links are resolved again once. Audio buffers remain in RAM.
HTTP 429 pauses Yandex API requests according to `Retry-After`; without that
header, backoff grows from 60 seconds to 15 minutes. The pause survives restart
and concurrent player/diagnostic runs; valid cached CDN links remain usable.

## VK and SoundCloud

`plavno login vk` and `plavno login soundcloud` open installed Chrome/Chromium
with a temporary profile. Complete sign-in, including any 2FA or QR steps.
`PLAVNO_BROWSER=/absolute/path/to/browser` overrides detection. The temporary
profile is removed afterward; playback needs no running browser.
SoundCloud asks for a station seed if none was detected.

For manual setup, use your regular browser's DevTools → **Network**:

```sh
plavno login vk --manual
plavno login soundcloud --manual
```

### Manual VK session

Sign in at `vk.ru/audio` or `vk.com/audio`. Copy **Cookie** and **User-Agent**
from a request to that site. In DevTools **Console**, run
`Number(window.vk?.id || window.vk?.user_id || 0)` for your numeric user ID.
Paste this profile, using the domain you signed into:

```json
{"cookie":"COPIED_COOKIE_VALUE","user_id":123456,"site":"https://vk.ru","user_agent":"COPIED_USER_AGENT"}
```

A Netscape cookie export can replace `cookie` with
`"cookies":"/absolute/path/vk-cookies.txt"`. Include cookies for the site and
its login host (`login.vk.ru` or `login.vk.com`) to support automatic renewal
of short-lived audio tokens. Check access with `plavno diagnose vk`.

If library access fails, open **My Music**, find a `/method/audio.*` request,
copy `access_token` from its payload, and add `"token":"COPIED_ACCESS_TOKEN"`
to the profile. A copied token needs valid session cookies to renew;
otherwise repeat manual setup when it expires.

`f` shuffles tracks from **My Music**, including manually added tracks;
`l` adds or removes the current track. `w` starts a new recommendation wave.

### Manual SoundCloud session

Open a track and start playback. From an `api-v2.soundcloud.com` request, copy
`client_id` from the query and, if signed in, the token after `OAuth ` in
**Authorization**:

```json
{"seed":"https://soundcloud.com/ARTIST/TRACK","client_id":"COPIED_CLIENT_ID","token":"COPIED_TOKEN"}
```

For public tracks, `seed` alone is enough; `client_id` is discovered automatically.
Token or Netscape `cookies` may be needed for account access.

## Spotify and YouTube Music

Build only the helper you need. These dependencies do not affect normal player edits.

```sh
cargo build --locked --release --manifest-path adapters/spotify/Cargo.toml --target-dir target/spotify
export PLAVNO_SPOTIFY_HELPER="$PWD/target/spotify/release/plavno-spotify"
cargo build --locked --release --manifest-path adapters/youtube/Cargo.toml --target-dir target/youtube
export PLAVNO_YOUTUBE_HELPER="$PWD/target/youtube/release/plavno-youtube"
```

Alternatively, put `plavno-spotify` / `plavno-youtube` on PATH. Run
`plavno login spotify` or `plavno login ytmusic` and paste a profile:

| Service | Profile example | Requirements |
|---|---|---|
| Spotify | `{"seed":"spotify:track:TRACK_ID","token":"ACCESS_TOKEN","client_id":"OAUTH_CLIENT_ID"}` | Premium; OAuth token with `streaming` scope. Reusable credentials are saved privately. |
| YouTube Music | `{"cookies":"/absolute/path/youtube-cookies.txt"}` | Netscape cookies for home mixes. Public radio also accepts `{"seed":"VIDEO_ID_OR_URL"}`; playlists accept `{"playlist":"MIX_PLAYLIST_ID"}`. |

YouTube web streams may need
[rustypipe-botguard](https://codeberg.org/ThetaDev/rustypipe-botguard) on PATH,
or `"botguard":"/absolute/path/rustypipe-botguard"` in the profile.
Both helpers select supported full audio at the best available quality.

## Diagnostics

`plavno sources` lists all five supported services, including Yandex Music,
with their connection status. `plavno diagnose vk` checks response
structure and library authentication without printing session values.
For expired sessions, repeat browser login or manual import.

Optional read-only checks using your configured VK account:

```sh
cargo test live_session_restarts_without_repeating_the_previous_wave -- --ignored --nocapture
cargo test live_session_reads_my_music_shuffle -- --ignored --nocapture
```

For HTTP/HLS transport checks with ffmpeg: `cargo test --test native_stream -- --ignored`.
