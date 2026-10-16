use super::*;
use crate::backend::PlaybackMode;
use sha1::Sha1;

const ORIGIN: &str = "https://music.youtube.com";

fn configured(profile: &std::path::Path) -> bool {
    web::profile(profile)
        .ok()
        .is_some_and(|p| p["cookies"].is_string() || p["headers"].is_string())
}

fn authorization(cookies: &str, now: u64) -> Result<String> {
    let cookie = |name: &str| {
        cookies.split(';').find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name && !value.is_empty()).then_some(value)
        })
    };
    let sid = cookie("SAPISID")
        .or_else(|| cookie("__Secure-3PAPISID"))
        .context("YouTube likes require signed-in cookies; refresh the cookie export")?;
    Ok(format!(
        "SAPISIDHASH {now}_{:x}",
        Sha1::digest(format!("{now} {sid} {ORIGIN}"))
    ))
}

// Only inspect the selected track's renderer, never the adjacent recommendations.
fn selected_rating(value: &Value, id: &str) -> Option<bool> {
    match value {
        Value::Object(object) => {
            if let Some(track) = object.get("playlistPanelVideoRenderer")
                && track["videoId"].as_str() == Some(id)
            {
                return rating(track);
            }
            object.values().find_map(|v| selected_rating(v, id))
        }
        Value::Array(items) => items.iter().find_map(|v| selected_rating(v, id)),
        _ => None,
    }
}

fn rating(value: &Value) -> Option<bool> {
    if let Some(status) = value
        .pointer("/likeButtonRenderer/likeStatus")
        .and_then(Value::as_str)
    {
        return match status {
            "LIKE" => Some(true),
            "INDIFFERENT" | "DISLIKE" => Some(false),
            _ => None,
        };
    }
    // The default endpoint is the action available now, hence the inverse state.
    if let Some(status) = value
        .pointer("/musicToggleMenuServiceItemRenderer/defaultServiceEndpoint/likeEndpoint/status")
        .and_then(Value::as_str)
    {
        return match status {
            "INDIFFERENT" => Some(true),
            "LIKE" => Some(false),
            _ => None,
        };
    }
    match value {
        Value::Object(object) => object.values().find_map(rating),
        Value::Array(items) => items.iter().find_map(rating),
        _ => None,
    }
}

fn find<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(object) => object
            .get(key)
            .or_else(|| object.values().find_map(|v| find(v, key))),
        Value::Array(items) => items.iter().find_map(|v| find(v, key)),
        _ => None,
    }
}

fn text(value: &Value) -> String {
    value["runs"]
        .as_array()
        .map(|runs| runs.iter().filter_map(|r| r["text"].as_str()).collect())
        .unwrap_or_default()
}

fn music_track(item: &Value) -> Option<Track> {
    let item = item.get("musicResponsiveListItemRenderer")?;
    if item["musicItemRendererDisplayPolicy"].as_str()
        == Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT")
    {
        return None;
    }
    let endpoint = find(item, "watchEndpoint")?;
    let id = endpoint["videoId"].as_str().filter(|id| valid_id(id))?;
    if let Some(kind) = find(endpoint, "musicVideoType").and_then(Value::as_str)
        && !matches!(
            kind,
            "MUSIC_VIDEO_TYPE_ATV" | "MUSIC_VIDEO_TYPE_OMV" | "MUSIC_VIDEO_TYPE_UGC"
        )
    {
        return None;
    }
    let columns = item["flexColumns"].as_array()?;
    let title = text(&columns.first()?["musicResponsiveListItemFlexColumnRenderer"]["text"]);
    if title.is_empty() {
        return None;
    }
    let artist = columns
        .get(1)
        .and_then(|c| {
            c.pointer("/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")
                .and_then(Value::as_str)
        })
        .map(str::to_owned)
        .unwrap_or_default();
    let duration = item
        .pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text")
        .map(text)
        .and_then(|s| {
            s.split(':').try_fold(0u64, |sum, part| {
                sum.checked_mul(60)?.checked_add(part.parse().ok()?)
            })
        })
        .map(|s| s as f64);
    let cover_url = find(item, "thumbnails")
        .and_then(Value::as_array)
        .and_then(|t| t.last())
        .and_then(|t| t["url"].as_str())
        .map(str::to_owned);
    Some(Track {
        source: "ytmusic".into(),
        id: id.into(),
        title,
        artist,
        duration,
        cover_url,
        genre: None,
        batch: String::new(),
    })
}

impl YouTube {
    pub(crate) fn has_favorites_profile(profile: &std::path::Path) -> bool {
        configured(profile)
    }

    pub(super) fn check_cooldown(&mut self) -> Result<()> {
        let account = self.session()?.account.clone();
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        if let Ok(state) = web::profile(&self.profile.with_file_name("ytmusic-backoff.json"))
            && state["account"].as_str() == Some(&account)
            && let Some(until) = state["until"].as_u64().filter(|until| *until > now)
        {
            return Err(
                RateLimited::for_service("YouTube", Duration::from_secs(until - now)).into(),
            );
        }
        Ok(())
    }

    pub(super) fn limit(&mut self, wait: u64) -> Result<anyhow::Error> {
        let wait = wait.max(1);
        let account = self.session()?.account.clone();
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        crate::config::save_private(
            &self.profile.with_file_name("ytmusic-backoff.json"),
            &serde_json::to_vec(&json!({"account":account,"until":now.saturating_add(wait)}))?,
        )?;
        Ok(RateLimited::for_service("YouTube", Duration::from_secs(wait)).into())
    }

    fn api(&mut self, endpoint: &str, mut body: Value) -> Result<Value> {
        self.check_cooldown()?;
        let path = self
            .session()?
            .cookies
            .as_ref()
            .context("YouTube likes require saved cookies; use plavno login ytmusic")?;
        let cookie =
            web::Cookies::read(path)?.header(&format!("{ORIGIN}/youtubei/v1/{endpoint}"))?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let auth = authorization(&cookie, now)?;
        let auth_user = self.session()?.auth_user.clone();
        let date = time::OffsetDateTime::now_utc();
        body["context"] = json!({"client":{"clientName":"WEB_REMIX", "clientVersion":format!("1.{:04}{:02}{:02}.01.00",date.year(),u8::from(date.month()),date.day()),"hl":"en"},"user":{}});
        let base = format!("{ORIGIN}/youtubei/v1/");
        #[cfg(test)]
        let base = self.api_base.clone().unwrap_or(base);
        // yt-dlp's private jar remains the source of cookies, including rotations.
        self.api.cookie_jar_lock().clear();
        let mut response = self
            .api
            .post(format!("{base}{endpoint}?prettyPrint=false"))
            .header("Origin", ORIGIN)
            .header("X-Origin", ORIGIN)
            .header("Authorization", auth)
            .header("Cookie", cookie)
            .header("X-Goog-AuthUser", auth_user)
            .header("X-Youtube-Client-Name", "67")
            .header("User-Agent", web::USER_AGENT)
            .send_json(body)
            .map_err(|_| anyhow::anyhow!("YouTube likes request failed; check your network"))?;
        let status = response.status().as_u16();
        if status == 429 {
            let wait = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| {
                    v.trim().parse::<u64>().ok().or_else(|| {
                        time::OffsetDateTime::parse(
                            v.trim(),
                            &time::format_description::well_known::Rfc2822,
                        )
                        .ok()
                        .map(|date| (date.unix_timestamp() - now as i64).max(1) as u64)
                    })
                })
                .unwrap_or(60);
            return Err(self.limit(wait)?);
        }
        ensure!(
            !matches!(status, 401 | 403),
            "YouTube likes require sign-in; refresh saved cookies"
        );
        ensure!(
            (200..300).contains(&status),
            "YouTube likes returned HTTP {status}"
        );
        let value: Value = response
            .body_mut()
            .with_config()
            .limit(8 * 1024 * 1024)
            .read_json()
            .map_err(|_| anyhow::anyhow!("Invalid YouTube likes response"))?;
        ensure!(
            value.is_object() && value.get("error").is_none(),
            "YouTube rejected the likes request; refresh saved cookies"
        );
        ensure!(
            find(&value, "loggedOut").and_then(Value::as_bool) != Some(true),
            "YouTube likes require sign-in; refresh saved cookies"
        );
        Ok(value)
    }

    fn remember_rating(&mut self, id: &str, liked: bool) {
        let session = self.session.as_mut().unwrap();
        session.liked.retain(|(key, _)| key != id);
        session.liked.push_back((id.into(), liked));
        while session.liked.len() > 100 {
            session.liked.pop_front();
        }
        if !liked && let Some(queue) = &mut session.favorites {
            queue.retain(|t| t.id != id);
        }
    }

    pub(super) fn read_rating(&mut self, track: &Track) -> Result<Option<bool>> {
        ensure!(valid_id(&track.id), "Invalid YouTube video ID");
        if let Some((_, liked)) = self.session()?.liked.iter().find(|(id, _)| id == &track.id) {
            return Ok(Some(*liked));
        }
        let value = self.api(
            "next",
            json!({"videoId":track.id,"isAudioOnly":true,"enablePersistentPlaylistPanel":true}),
        )?;
        let liked = selected_rating(&value, &track.id)
            .context("YouTube did not return this track's like state")?;
        self.remember_rating(&track.id, liked);
        Ok(Some(liked))
    }

    pub(super) fn rate(&mut self, track: &Track) -> Result<bool> {
        let liked = !self
            .read_rating(track)?
            .context("YouTube did not return a like state")?;
        self.api(
            if liked {
                "like/like"
            } else {
                "like/removelike"
            },
            json!({"target":{"videoId":track.id}}),
        )?;
        self.remember_rating(&track.id, liked);
        Ok(liked)
    }

    pub(super) fn favorite_batch(&mut self, _history: &[String]) -> Result<Vec<Track>> {
        if self
            .session()?
            .favorites
            .as_ref()
            .is_none_or(VecDeque::is_empty)
        {
            let mut body = json!({"browseId":"VLLM"});
            let mut tracks = Vec::new();
            let mut seen = HashSet::new();
            let mut tokens = HashSet::new();
            loop {
                let value = self.api("browse", body)?;
                let shelf = find(&value, "musicPlaylistShelfRenderer")
                    .or_else(|| find(&value, "musicPlaylistShelfContinuation"))
                    .or_else(|| find(&value, "appendContinuationItemsAction"))
                    .context("YouTube did not return Liked Music")?;
                let items = shelf["contents"]
                    .as_array()
                    .or_else(|| shelf["continuationItems"].as_array())
                    .context("YouTube did not return Liked Music tracks")?;
                for track in items.iter().filter_map(music_track) {
                    if seen.insert(track.id.clone()) {
                        tracks.push(track);
                    }
                }
                ensure!(
                    tracks.len() <= 100_000,
                    "YouTube Liked Music exceeds the shuffle limit"
                );
                let token = find(shelf, "nextContinuationData")
                    .or_else(|| find(shelf, "nextRadioContinuationData"))
                    .and_then(|v| v["continuation"].as_str())
                    .or_else(|| {
                        find(shelf, "continuationCommand").and_then(|v| v["token"].as_str())
                    });
                let Some(token) = token else {
                    break;
                };
                ensure!(
                    tokens.insert(token.to_owned()) && tokens.len() <= 2000,
                    "YouTube Liked Music pagination did not finish"
                );
                body = json!({"continuation":token});
            }
            ensure!(!tracks.is_empty(), "YouTube Music Liked Music is empty");
            let mut ids: Vec<String> = tracks.iter().map(|t| t.id.clone()).collect();
            super::super::yandex::shuffle_favorites(
                &mut ids,
                self.session()?.favorites_last.as_deref(),
                &mut plavno_platform::Random::new()?,
            )?;
            let mut pool: std::collections::HashMap<_, _> =
                tracks.into_iter().map(|t| (t.id.clone(), t)).collect();
            self.session.as_mut().unwrap().favorites = Some(
                ids.into_iter()
                    .map(|id| pool.remove(&id).unwrap())
                    .collect(),
            );
        }
        let session = self.session.as_mut().unwrap();
        let queue = session.favorites.as_mut().unwrap();
        let batch: Vec<_> = queue.drain(..queue.len().min(25)).collect();
        session.favorites_last = batch.last().map(|t| t.id.clone());
        for track in &batch {
            self.remember_rating(&track.id, true);
        }
        Ok(batch)
    }

    pub(super) fn start_favorites(&mut self, mode: PlaybackMode) -> Result<Vec<Track>> {
        if mode == PlaybackMode::MyVibe {
            return self.start();
        }
        self.session = None;
        self.session()?
            .cookies
            .as_ref()
            .context("YouTube Liked Music requires saved cookies")?;
        self.favorite_batch(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::Fixture;
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        net::TcpListener,
    };

    fn account(fixture: &Fixture) -> YouTube {
        let cookies = fixture.0.join("cookies.txt");
        crate::config::save_private(
            &cookies,
            b".youtube.com\tTRUE\t/\tTRUE\t0\tSAPISID\tfixture-secret\n",
        )
        .unwrap();
        crate::config::save_private(
            &fixture.0.join("ytmusic.json"),
            &serde_json::to_vec(&json!({"cookies":cookies,"auth_user":"1"})).unwrap(),
        )
        .unwrap();
        fixture.backend()
    }

    fn server(responses: Vec<(u16, Value)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if let Some(n) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = n.trim().parse().unwrap();
                    }
                    request.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body_bytes = vec![0; length];
                reader.read_exact(&mut body_bytes).unwrap();
                request.push_str(&String::from_utf8(body_bytes).unwrap());
                requests.push(request);
                let body = body.to_string();
                write!(socket,"HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nContent-Type: application/json\r\nRetry-After: 120\r\nConnection: close\r\n\r\n{body}",body.len()).unwrap();
            }
            requests
        });
        (base, worker)
    }

    fn track(id: &str) -> Track {
        Track {
            source: "ytmusic".into(),
            id: id.into(),
            title: "A song".into(),
            artist: "Artist".into(),
            duration: None,
            cover_url: None,
            genre: None,
            batch: String::new(),
        }
    }

    fn state(id: &str, action: &str) -> Value {
        json!({"playlistPanelVideoRenderer":{"videoId":id,"menu":{"items":[{"musicToggleMenuServiceItemRenderer":{"defaultServiceEndpoint":{"likeEndpoint":{"status":action}}}}]}}})
    }

    fn song(id: &str) -> Value {
        json!({"musicResponsiveListItemRenderer":{
            "overlay":{"watchEndpoint":{"videoId":id,"watchEndpointMusicSupportedConfigs":{"watchEndpointMusicConfig":{"musicVideoType":"MUSIC_VIDEO_TYPE_ATV"}}}},
            "flexColumns":[{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Song"}]}}},{"musicResponsiveListItemFlexColumnRenderer":{"text":{"runs":[{"text":"Artist"}]}}}],
            "fixedColumns":[{"musicResponsiveListItemFixedColumnRenderer":{"text":{"runs":[{"text":"3:12"}]}}}],
            "thumbnail":{"thumbnails":[{"url":"https://example.invalid/cover.jpg"}]}
        }})
    }

    #[test]
    fn selected_track_state_is_cached_and_only_confirmed_mutations_change_it() {
        let fixture = Fixture::new();
        let mut youtube = account(&fixture);
        let cookies = fs::read(fixture.0.join("cookies.txt")).unwrap();
        let (base, worker) = server(vec![
            (
                200,
                json!({"contents":[state("abcdefghijk","INDIFFERENT"),state("5LGUgChapj4","LIKE")]}),
            ),
            (
                403,
                json!({"error":{"message":"fixture-secret https://private.invalid/?secret"}}),
            ),
            (200, json!({})),
            (200, json!({})),
        ]);
        youtube.api_base = Some(base);
        let track = track("5LGUgChapj4");
        assert!(youtube.supports_favorites() && youtube.supports_favorites_shuffle());
        assert_eq!(youtube.favorite_state(&track).unwrap(), Some(false));
        assert_eq!(youtube.favorite_state(&track).unwrap(), Some(false));
        let error = youtube.toggle_favorite(&track).unwrap_err().to_string();
        assert!(error.contains("refresh saved cookies") && !error.contains("fixture-secret"));
        assert_eq!(youtube.favorite_state(&track).unwrap(), Some(false));
        assert!(youtube.toggle_favorite(&track).unwrap());
        assert_eq!(youtube.favorite_state(&track).unwrap(), Some(true));
        assert!(!youtube.toggle_favorite(&track).unwrap());
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].starts_with("POST /next?"));
        assert!(requests[1].starts_with("POST /like/like?"));
        assert!(requests[2].starts_with("POST /like/like?"));
        assert!(requests[3].starts_with("POST /like/removelike?"));
        for request in requests {
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("origin: https://music.youtube.com"));
            assert!(lower.contains("x-goog-authuser: 1"));
            assert!(lower.contains("authorization: sapisidhash "));
            let body: Value =
                serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
            assert_eq!(body["context"]["client"]["clientName"], "WEB_REMIX");
        }
        assert_eq!(fs::read(fixture.0.join("cookies.txt")).unwrap(), cookies);
        assert!(
            fixture.calls().is_empty(),
            "Likes must not invoke yt-dlp or resolve audio"
        );
    }

    #[test]
    fn likes_cooldown_blocks_writes_and_metadata_after_restart() {
        let fixture = Fixture::new();
        let mut youtube = account(&fixture);
        let (base, worker) = server(vec![(429, json!({}))]);
        youtube.api_base = Some(base);
        let track = track("5LGUgChapj4");
        assert!(
            youtube
                .favorite_state(&track)
                .unwrap_err()
                .downcast_ref::<RateLimited>()
                .is_some()
        );
        drop(youtube);
        let mut youtube = fixture.backend();
        assert!(
            youtube
                .toggle_favorite(&track)
                .unwrap_err()
                .downcast_ref::<RateLimited>()
                .is_some()
        );
        assert!(
            youtube
                .resolve(&track)
                .err()
                .unwrap()
                .downcast_ref::<RateLimited>()
                .is_some()
        );
        assert!(fixture.calls().is_empty());
        assert_eq!(worker.join().unwrap().len(), 1);
        let state = web::profile(&fixture.0.join("ytmusic-backoff.json")).unwrap();
        assert!(
            state["until"].as_u64().unwrap()
                >= SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 115
        );
    }

    #[test]
    fn liked_music_paginates_deduplicates_and_reuses_the_shuffle_queue() {
        let fixture = Fixture::new();
        let mut youtube = account(&fixture);
        let first: Vec<_> = (0..20).map(|i| song(&format!("music{i:06}"))).collect();
        let mut second: Vec<_> = (15..30).map(|i| song(&format!("music{i:06}"))).collect();
        let mut podcast = song("podcast0000");
        podcast["musicResponsiveListItemRenderer"]["overlay"]["watchEndpoint"]["watchEndpointMusicSupportedConfigs"]
            ["watchEndpointMusicConfig"]["musicVideoType"] =
            json!("MUSIC_VIDEO_TYPE_PODCAST_EPISODE");
        second.push(podcast);
        let (base, worker) = server(vec![
            (
                200,
                json!({"musicPlaylistShelfRenderer":{"contents":first,"continuations":[{"nextContinuationData":{"continuation":"page2"}}]},"suggestions":[song("abcdefghijk")]}),
            ),
            (
                200,
                json!({"onResponseReceivedActions":[{"appendContinuationItemsAction":{"continuationItems":second}}]}),
            ),
        ]);
        youtube.api_base = Some(base);
        let mut batch = youtube.start_mode(PlaybackMode::FavoritesShuffle).unwrap();
        assert_eq!(batch.len(), 25);
        assert_eq!(youtube.favorite_state(&batch[0]).unwrap(), Some(true));
        let next = youtube.more(&[]).unwrap();
        assert_eq!(next.len(), 5);
        batch.extend(next);
        let ids: HashSet<_> = batch.iter().map(|t| &t.id).collect();
        assert_eq!(ids.len(), 30);
        assert!(
            batch.iter().all(|t| t.duration == Some(192.0)
                && t.artist == "Artist"
                && t.cover_url.is_some())
        );
        assert!(ids.iter().all(|id| id.starts_with("music")));
        let requests = worker.join().unwrap();
        assert_eq!(requests.len(), 2);
        let body = |request: &str| {
            serde_json::from_str::<Value>(request.split_once("\r\n\r\n").unwrap().1).unwrap()
        };
        assert_eq!(body(&requests[0])["browseId"], "VLLM");
        assert_eq!(body(&requests[1])["continuation"], "page2");
        assert!(fixture.calls().is_empty());
    }

    #[test]
    fn unauthenticated_or_expired_cookies_cannot_send_likes() {
        let fixture = Fixture::new();
        let mut youtube = fixture.backend();
        assert!(!youtube.supports_favorites());
        assert_eq!(youtube.favorite_state(&track("5LGUgChapj4")).unwrap(), None);
        assert!(youtube.toggle_favorite(&track("5LGUgChapj4")).is_err());
        assert!(youtube.start_mode(PlaybackMode::FavoritesShuffle).is_err());
        assert!(authorization("SID=other", 123).is_err());
        assert_eq!(
            authorization("__Secure-3PAPISID=fake", 123).unwrap(),
            "SAPISIDHASH 123_ffb9d6b00c733d76f0022e169d8b251a33e630d6"
        );
        let cookies =
            web::Cookies::parse(".youtube.com\tTRUE\t/\tTRUE\t1\tSAPISID\texpired\n").unwrap();
        assert!(authorization(&cookies.header(ORIGIN).unwrap(), 123).is_err());
        assert_eq!(
            selected_rating(&state("abcdefghijk", "LIKE"), "5LGUgChapj4"),
            None
        );
    }

    #[test]
    fn legacy_cookie_headers_authorize_both_youtube_hosts_and_keep_the_account_selection() {
        let fixture = Fixture::new();
        let headers = fixture.0.join("headers.json");
        let saved =
            serde_json::to_vec(&json!({"Cookie":"SAPISID=legacy-fixture", "X-Goog-AuthUser":"2"}))
                .unwrap();
        crate::config::save_private(&headers, &saved).unwrap();
        crate::config::save_private(
            &fixture.0.join("ytmusic.json"),
            &serde_json::to_vec(&json!({"headers":headers})).unwrap(),
        )
        .unwrap();
        let mut youtube = fixture.backend();
        let session = youtube.session().unwrap();
        assert_eq!(session.auth_user, "2");
        let cookies = web::Cookies::read(session.cookies.as_ref().unwrap()).unwrap();
        for site in [ORIGIN, "https://www.youtube.com/"] {
            assert_eq!(cookies.header(site).unwrap(), "SAPISID=legacy-fixture");
        }
        assert_eq!(cookies.header("https://example.invalid/").unwrap(), "");
        assert_eq!(fs::read(&headers).unwrap(), saved);
    }

    #[test]
    fn shuffle_cycles_refresh_the_library_and_do_not_repeat_the_boundary_track() {
        let fixture = Fixture::new();
        let mut youtube = account(&fixture);
        let page = json!({"musicPlaylistShelfRenderer":{"contents":[song("music000000"),song("music000001")]}});
        let (base, worker) = server(vec![(200, page.clone()), (200, page), (200, json!({}))]);
        youtube.api_base = Some(base);
        let first = youtube.start_mode(PlaybackMode::FavoritesShuffle).unwrap();
        let second = youtube
            .more(&first.iter().map(|t| t.id.clone()).collect::<Vec<_>>())
            .unwrap();
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        assert_ne!(first.last().unwrap().id, second[0].id);
        assert!(!youtube.toggle_favorite(&second[0]).unwrap());
        assert_eq!(youtube.favorite_state(&second[0]).unwrap(), Some(false));
        for n in 0..110 {
            youtube.remember_rating(&format!("music{n:06}"), true);
        }
        assert_eq!(youtube.session().unwrap().liked.len(), 100);
        assert_eq!(worker.join().unwrap().len(), 3);
    }

    #[test]
    fn api_errors_and_logged_out_responses_do_not_confirm_likes() {
        let fixture = Fixture::new();
        let mut youtube = account(&fixture);
        let (base, worker) = server(vec![
            (200, state("5LGUgChapj4", "LIKE")),
            (200, json!({"error":{"message":"private-secret"}})),
            (
                200,
                json!({"responseContext":{"mainAppWebResponseContext":{"loggedOut":true}}}),
            ),
        ]);
        youtube.api_base = Some(base);
        let track = track("5LGUgChapj4");
        assert_eq!(youtube.favorite_state(&track).unwrap(), Some(false));
        for _ in 0..2 {
            let error = youtube.toggle_favorite(&track).unwrap_err().to_string();
            assert!(!error.contains("private-secret"));
            assert_eq!(youtube.favorite_state(&track).unwrap(), Some(false));
        }
        assert_eq!(worker.join().unwrap().len(), 3);
    }
}
