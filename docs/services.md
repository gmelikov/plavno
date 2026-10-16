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
On macOS, Chrome/Chromium may be installed in `/Applications` or `~/Applications`;
`PLAVNO_BROWSER` can also point inside an app bundle, for example
`/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`.
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

Release archives include the Spotify helper. Keep `plavno-spotify` next to the
player, or put it on PATH. `PLAVNO_SPOTIFY_HELPER` overrides discovery; otherwise
the player searches its own directory first, then PATH.

For a source checkout or Cargo installation, build it separately:

```sh
cargo build --locked --release --manifest-path adapters/spotify/Cargo.toml --target-dir target/spotify
export PLAVNO_SPOTIFY_HELPER="$PWD/target/spotify/release/plavno-spotify"
```

YouTube uses an external [yt-dlp](https://github.com/yt-dlp/yt-dlp#installation),
installed by the user. On Debian/Ubuntu, run `sudo apt install yt-dlp`; on macOS,
run `brew install yt-dlp`. On Windows, put `yt-dlp.exe` on PATH. yt-dlp is optional
for other sources and is not included in release archives or installed by the
Homebrew formula. `PLAVNO_YT_DLP` overrides its executable path.

For full YouTube support, install a supported JavaScript runtime: **Node.js 22+**
or **Deno 2.3+**, available on PATH. plavno passes `--js-runtimes node`, retaining
yt-dlp's default Deno support. Older yt-dlp versions that reject this optional flag
are retried locally without it; this happens before any service request, once per
backend instance. Challenge failures do not trigger repeated metadata requests.

You also need a matching **yt-dlp-ejs** installation. Official yt-dlp executables
bundle it; distribution packages may provide it separately. Follow the
[yt-dlp EJS instructions](https://github.com/yt-dlp/yt-dlp/wiki/EJS) for your
installation method. If JavaScript challenge solving fails, `--list-formats` can
still exit successfully while listing only images; that does not confirm that
audio is available. plavno reports the missing solver requirement explicitly.

Run `plavno login spotify` or `plavno login ytmusic` and paste a profile:

| Service | Profile example | Requirements |
|---|---|---|
| Spotify | `{"token":"ACCESS_TOKEN","client_id":"OAUTH_CLIENT_ID"}` | Premium; OAuth token with `streaming` scope. Pass `--seed spotify:track:TRACK_ID` when starting playback, or add `seed` to the profile. Reusable credentials are saved privately. |
| YouTube Music | `{"cookies":"/absolute/path/youtube-cookies.txt"}` | Private Netscape cookies for the personal `RDMM` mix. Saved seeds (`{"seed":"VIDEO_ID_OR_URL"}`) and playlists (`{"playlist":"PLAYLIST_ID"}`) are also supported. Cookies are passed through a temporary private copy; the original file is preserved. Legacy `headers` profiles with a Cookie header remain supported. |

To start public YouTube Music radio without creating a connection profile:

```sh
plavno --source ytmusic --seed VIDEO_ID_OR_URL
```

`--seed` also works with `--source soundcloud` (a track URL) and `--source spotify`
(a `spotify:track:` URI). It takes priority over the saved seed or mix, reuses
saved authentication, and lasts for the current process, including wave restarts
and source switching. The connection profile is not changed. SoundCloud and
YouTube Music can use a public seed without login; Spotify requires saved
credentials and Premium. Select one of these sources explicitly; `--seed`
cannot be combined with `--source mixed`, `--favorites`, or `--file`.
Rebuild the Spotify helper when updating a source installation.

With cookies alone, plavno reads YouTube Music home recommendations to choose a
starting song, preferring a music radio endpoint, then opens the personal `RDMM`
mix with that video ID. If Music home has no song, it reads one metadata page of
Liked Music to find an anchor. The starting song stays in RAM; the profile is not
changed, and later radio batches reuse the anchor without another home request.
An empty music account needs recommendations or liked songs on music.youtube.com,
or an explicit seed or playlist. A rejected session is reported as a sign-in
error rather than an empty recommendation feed.

yt-dlp fetches bounded flat playlist batches and resolves audio URLs only for
tracks selected for playback or adjacent preloading. It does not download audio.
The existing Rust RAM cache handles download, seeking, and replay; the yt-dlp
process exits after each metadata request. No signed URL cache is written to disk.
Before downloading, plavno honors yt-dlp's `available_at` timestamp for the selected
audio format. Some YouTube streams reject early downloads with HTTP 403; waiting
does not require another metadata request or a CDN probe. A wait longer than two
minutes is reported as unavailable instead of keeping playback pending.
HTTP 429 pauses further metadata and likes requests, with a private account-scoped cooldown
that survives restarts. Update yt-dlp if YouTube changes break extraction.

With signed-in YouTube cookies, `l` reads and toggles the track's like in your
account, including while playing `mixed`. Public radio without cookies has no
likes. If the export lacks `SAPISID` or `__Secure-3PAPISID`, or YouTube rejects the
session, refresh the export. For multiple Google accounts, the optional profile
field `"auth_user":"1"` selects the account index; the default is `"0"`. Legacy
browser headers retain their `X-Goog-AuthUser` selection.

Press `f`, or run `plavno --source ytmusic --favorites`, to shuffle YouTube Music's
**Liked Music** (`LM`). This does not load YouTube's general Liked videos (`LL`).
Music videos in Liked Music are included; podcast episodes and unavailable entries
are skipped. The player loads paginated metadata into RAM and shuffles the whole
collection, with a limit of 100,000 tracks. Audio still loads only for playback and
adjacent preloading. Each new shuffle cycle refreshes the collection; `f` restarts
it manually, and `w` returns to radio. A single selected service is required for
Favorites Shuffle. Like states are cached in RAM, and changes appear only after
YouTube accepts the request. No likes requests are retried automatically.

YouTube Opus/WebM streams require ffmpeg on PATH, or `PLAVNO_FFMPEG` pointing to
its executable. Playback errors distinguish conversion failures from CDN HTTP
errors, timeouts, and TLS failures without displaying signed stream URLs.
YouTube audio is downloaded in bounded HTTP ranges and piped to ffmpeg for
decoding; this avoids CDN rejection of ffmpeg's open-ended range requests.
If a YouTube CDN connection fails before audio arrives, plavno tries up to two
backup hosts advertised in the same stream URL, without probes or extra metadata
requests. HTTP errors, including 403 and 429, do not trigger this fallback.

## Diagnostics

`plavno sources` lists the streaming services and local directory source
with their connection status. `plavno diagnose vk` checks response
structure and library authentication without printing session values.
For expired sessions, repeat browser login or manual import.

Optional read-only checks using your configured VK account:

```sh
cargo test live_session_restarts_without_repeating_the_previous_wave -- --ignored --nocapture
cargo test live_session_reads_my_music_shuffle -- --ignored --nocapture
```

For HTTP/HLS transport checks with ffmpeg: `cargo test --test native_stream -- --ignored`.
