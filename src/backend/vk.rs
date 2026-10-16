use super::{Backend, Feedback, Input, PlaybackMode, Track, WaveStyle, web};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub struct Vk {
    profile: PathBuf,
    config: Option<Value>,
    http: ureq::Agent,
    cookies: web::Cookies,
    api: String,
    site: String,
    mobile: String,
    user: i64,
    hashes: HashMap<String, String>,
    streams: HashMap<String, (Value, Instant)>,
    reloaded: Option<Instant>,
    user_agent: String,
    web_token: Option<(String, Instant)>,
    api_user_verified: bool,
    style: WaveStyle,
    mix_options_id: u64,
    manual_token_rejected: bool,
    mix_active: bool,
    recent: VecDeque<String>,
    library: Option<LibraryQueue>,
}

#[derive(Default)]
struct LibraryQueue {
    remaining: VecDeque<String>,
    last: Option<String>,
    fresh_cycle: bool,
}

impl Vk {
    pub fn new(profile: PathBuf) -> Self {
        Self {
            profile,
            config: None,
            http: super::yandex_http(),
            cookies: web::Cookies::default(),
            api: "https://web.api.vk.ru/method/".into(),
            site: "https://vk.ru".into(),
            mobile: "https://m.vk.ru".into(),
            user: 0,
            hashes: HashMap::new(),
            streams: HashMap::new(),
            reloaded: None,
            user_agent: web::USER_AGENT.into(),
            web_token: None,
            api_user_verified: false,
            style: WaveStyle::default(),
            mix_options_id: 0,
            manual_token_rejected: false,
            mix_active: false,
            recent: VecDeque::new(),
            library: None,
        }
    }
    fn init(&mut self) -> Result<()> {
        if self.config.is_some() {
            return Ok(());
        }
        let config = web::profile(&self.profile)?;
        self.user_agent = web::user_agent(&config)?;
        if let Some(site) = config["site"].as_str() {
            ensure!(
                matches!(site, "https://vk.ru" | "https://vk.com"),
                "Invalid VK site"
            );
            self.site = site.into();
            self.mobile = site.replacen("https://", "https://m.", 1);
        }
        if config["cookies"].is_string() || config["cookie"].is_string() {
            self.cookies = if let Some(cookies) = config["cookies"].as_str() {
                web::Cookies::read(Path::new(cookies))?
            } else {
                web::Cookies::from_header(config["cookie"].as_str().unwrap(), &self.site)?
            };
            self.user = config["user_id"]
                .as_i64()
                .or_else(|| {
                    self.cookies
                        .header(&self.site)
                        .ok()?
                        .split("; ")
                        .find_map(|part| part.strip_prefix("remixuserid=")?.parse().ok())
                })
                .context("Configure your numeric VK user_id")?;
            ensure!(self.user > 0, "Invalid VK user_id");
            self.load_cookie_cache()?;
            self.cookies.seed_agent(&self.http)?;
        } else {
            ensure!(
                config["token"].as_str().is_some_and(|t| !t.is_empty()),
                "Configure VK browser cookies or an audio-capable token"
            );
        }
        self.config = Some(config);
        Ok(())
    }

    fn load_cookie_cache(&self) -> Result<()> {
        let path = self.profile.with_extension("cookie-cache.json");
        let Ok(cache) = std::fs::metadata(&path) else {
            return Ok(());
        };
        let cached_at = cache.modified()?;
        if std::fs::metadata(&self.profile)?.modified()? >= cached_at {
            return Ok(());
        }
        let config = web::profile(&self.profile)?;
        if let Some(path) = config["cookies"].as_str()
            && std::fs::metadata(path)?.modified()? >= cached_at
        {
            return Ok(());
        }
        let data = web::private(&path)?;
        self.http
            .cookie_jar_lock()
            .load_json(std::io::Cursor::new(data))
            .map_err(|_| anyhow::anyhow!("Invalid VK session cookie cache"))
    }

    fn save_cookie_cache(&self) {
        if self.profile.as_os_str().is_empty() {
            return;
        }
        let mut data = Vec::new();
        let result = self
            .http
            .cookie_jar_lock()
            .save_json(&mut data)
            .map_err(|_| anyhow::anyhow!("Cannot serialize VK session cookies"))
            .and_then(|()| {
                crate::config::save_private(
                    &self.profile.with_extension("cookie-cache.json"),
                    &data,
                )
            });
        if result.is_err() {
            eprintln!("plavno: VK session cookie cache could not be saved");
        }
    }

    pub fn diagnose(&mut self) -> Result<Value> {
        self.init()?;
        ensure!(
            self.config.as_ref().unwrap()["cookies"].is_string()
                || self.config.as_ref().unwrap()["cookie"].is_string(),
            "VK response diagnostics require a browser connection; run plavno login vk"
        );
        let data = self.recommendation_page()?;
        let mut budget = 512;
        let mut report = serde_json::json!({
            "source":"vk",
            "only_skeleton":data["onlySkeleton"].as_bool(),
            "legacy_playlist_present":data["sectionData"]["recoms"]["playlist"]["list"].is_array(),
            "page_fields":response_shape(&data, 1, &mut 128),
            "section_layout":response_shape(data.get("sectionData").unwrap_or(&data), 6, &mut budget)
        });
        let section = &data["sectionData"]["recoms"];
        report["section_id_present"] = section["sectionId"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
            .into();
        report["continuation_present"] = (!section_cursor(section).is_empty()).into();
        if section.is_object() && !section_items(section).any(|item| audio_hash(item).is_some()) {
            report["catalog_load"] = match self.load_catalog(section) {
                Ok(loaded) => response_shape(&loaded, 6, &mut 512),
                Err(error) => serde_json::json!({"error":error.to_string()}),
            };
        }
        report["library_access"] = match self.verify_api_user() {
            Ok(()) => serde_json::json!({"authenticated":true}),
            Err(error) => serde_json::json!({"authenticated":false,"error":error.to_string()}),
        };
        Ok(report)
    }

    fn recommendation_page(&self) -> Result<Value> {
        let html = self.request(
            &format!("{}/audio", self.site),
            &[
                ("section", "recoms".into()),
                ("owner_id", self.user.to_string()),
            ],
        )?;
        page_data(&html)
    }

    fn load_catalog(&self, section: &Value) -> Result<Value> {
        let mut fields = vec![("al", "1".into())];
        if let Some(id) = section["sectionId"].as_str().filter(|s| !s.is_empty()) {
            fields.extend([
                ("act", "load_catalog_section".into()),
                ("section_id", id.into()),
                ("start_from", section_cursor(section).into()),
            ]);
        } else {
            fields.extend([
                ("act", "section".into()),
                ("section", "recoms".into()),
                ("owner_id", self.user.to_string()),
                ("claim", "0".into()),
                ("is_layer", "0".into()),
            ]);
        }
        catalog_data(&self.request(&format!("{}/al_audio.php", self.site), &fields)?)
    }

    fn recommendation_ids(&self, history: &[String]) -> Result<Vec<(String, String)>> {
        let page = self.recommendation_page()?;
        let mut section = page["sectionData"]["recoms"].clone();
        ensure!(
            section.is_object(),
            "VK omitted its recommendation section; inspect it with: plavno diagnose vk"
        );
        let mut cursors = HashSet::new();
        for attempt in 0..4 {
            let mut seen = HashSet::new();
            let ids: Vec<_> = section_items(&section)
                .filter_map(audio_hash)
                .filter(|(id, _)| !history.contains(id) && seen.insert(id.clone()))
                .take(20)
                .collect();
            if !ids.is_empty() {
                return Ok(ids);
            }
            let cursor = (
                section["sectionId"].as_str().unwrap_or_default().to_owned(),
                section_cursor(&section).to_owned(),
            );
            if attempt == 3 || !cursors.insert(cursor) {
                break;
            }
            section = self.load_catalog(&section)?;
        }
        anyhow::bail!(
            "VK recommendation catalog returned no playable tracks; inspect its loading response with: plavno diagnose vk"
        )
    }
    fn request(&self, address: &str, fields: &[(&str, String)]) -> Result<String> {
        let request = self
            .http
            .post(address)
            .header("User-Agent", &self.user_agent)
            .header("X-Requested-With", "XMLHttpRequest")
            .header("Origin", &self.site)
            .header("Referer", format!("{}/audio", self.site));
        let mut response = request
            .send_form(fields.iter().map(|(key, value)| (*key, value.as_str())))
            .map_err(|error| match error {
                ureq::Error::StatusCode(code) => {
                    anyhow::anyhow!("VK request failed (HTTP {code}); check connection and session")
                }
                _ => anyhow::anyhow!("VK request failed; check connection and session"),
            })?;
        read_response(response.body_mut())
    }
    fn api(&mut self, method: &str, mut fields: Vec<(&str, String)>) -> Result<Value> {
        self.init()?;
        fields.push(("v", "5.289".into()));
        fields.push(("client_id", "6287487".into()));
        for attempt in 0..2 {
            let token = self.access_token()?;
            let mut fields = fields.clone();
            fields.push(("access_token", token));
            let data: Value =
                serde_json::from_str(&self.request(&format!("{}{method}", self.api), &fields)?)
                    .map_err(|_| anyhow::anyhow!("Invalid VK API response"))?;
            if data.get("error").is_some() {
                let code = data["error"]["error_code"].as_u64().unwrap_or(0);
                if matches!(code, 5 | 1116 | 1117) {
                    self.web_token = None;
                    self.api_user_verified = false;
                    self.manual_token_rejected = true;
                    if attempt == 0
                        && (self.config.as_ref().unwrap()["cookies"].is_string()
                            || self.config.as_ref().unwrap()["cookie"].is_string())
                    {
                        continue;
                    }
                    anyhow::bail!(
                        "VK token expired or session is invalid (code {code}); renew the session with plavno login vk or plavno login vk --manual"
                    );
                }
                anyhow::bail!(
                    "VK audio access denied (code {code}); use browser cookies or an audio-capable token"
                );
            }
            return data
                .get("response")
                .cloned()
                .context("Missing VK API response");
        }
        anyhow::bail!("VK authentication retries exhausted")
    }

    fn access_token(&mut self) -> Result<String> {
        if !self.manual_token_rejected
            && let Some(token) = self
                .config
                .as_ref()
                .and_then(|c| c["token"].as_str())
                .filter(|s| !s.is_empty())
        {
            return Ok(token.into());
        }
        if let Some((token, deadline)) = &self.web_token
            && Instant::now() < *deadline
        {
            return Ok(token.clone());
        }
        let address = format!(
            "{}/?act=web_token",
            self.site.replacen("https://", "https://login.", 1)
        );
        if self.http.cookie_jar_lock().iter().next().is_some() {
            let mut response = self
                .http
                .get(format!("{}/audio", self.site))
                .header("User-Agent", &self.user_agent)
                .call()
                .map_err(|_| anyhow::anyhow!("Cannot refresh the VK browser session"))?;
            std::io::copy(&mut response.body_mut().as_reader(), &mut std::io::sink())
                .map_err(|_| anyhow::anyhow!("Cannot refresh the VK browser session"))?;
        }
        let mut response = self
            .http
            .post(&address)
            .header("User-Agent", &self.user_agent)
            .header("Origin", &self.site)
            .header("Referer", format!("{}/", self.site))
            .send_form([("version", "1"), ("app_id", "6287487")])
            .map_err(|_| {
                anyhow::anyhow!("Cannot authenticate VK library access; renew your VK session")
            })?;
        let data: Value = serde_json::from_str(&read_response(response.body_mut())?)
            .map_err(|_| anyhow::anyhow!("Invalid VK authentication response"))?;
        let data = if data.is_array() {
            data.as_array()
                .unwrap()
                .iter()
                .find(|item| item["user_id"].as_i64() == Some(self.user))
                .context("VK did not authenticate the configured account")?
        } else {
            &data["data"]
        };
        if let Some(user) = data["user_id"].as_i64() {
            ensure!(user == self.user, "VK authenticated a different account");
        }
        let token = data["access_token"].as_str().filter(|s| !s.is_empty())
            .context("VK library access requires a signed-in session; renew it with plavno login vk or plavno login vk --manual")?;
        ensure!(
            self.user > 0 && data["user_id"].as_i64() == Some(self.user),
            "VK did not authenticate the configured account"
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_secs();
        let expires = data["expires"]
            .as_u64()
            .or_else(|| data["expires"].as_str()?.parse().ok());
        let ttl = match expires {
            Some(expires) if expires > 1_000_000_000 => expires.saturating_sub(now),
            Some(seconds) => seconds,
            None => 300,
        }
        .min(86_400);
        ensure!(
            ttl > 0,
            "VK issued an expired token; renew your browser session"
        );
        let deadline = Instant::now() + Duration::from_secs(ttl - (ttl / 10).min(30));
        self.web_token = Some((token.into(), deadline));
        self.save_cookie_cache();
        Ok(token.into())
    }

    fn verify_api_user(&mut self) -> Result<()> {
        if !self.api_user_verified {
            let user = self.api("users.get", vec![])?;
            let authenticated = user[0]["id"]
                .as_i64()
                .filter(|n| *n > 0)
                .context("VK did not identify your account")?;
            ensure!(
                self.user == 0 || self.user == authenticated,
                "VK audio token belongs to a different account"
            );
            self.user = authenticated;
            self.api_user_verified = true;
        }
        Ok(())
    }

    fn library_batch(&mut self) -> Result<Vec<Track>> {
        self.init()?;
        self.verify_api_user()?;
        self.trim_streams();
        if self.library.as_ref().unwrap().remaining.is_empty() {
            let mut ids = Vec::new();
            let mut seen = HashSet::new();
            let mut offset = 0;
            loop {
                let response = self.api(
                    "audio.get",
                    vec![
                        ("owner_id", self.user.to_string()),
                        ("count", "200".into()),
                        ("offset", offset.to_string()),
                    ],
                )?;
                let items = response["items"]
                    .as_array()
                    .context("VK did not return My Music")?;
                for item in items {
                    if let Some(id) = track_id(item)
                        && seen.insert(id.clone())
                    {
                        ids.push(id);
                    }
                }
                offset += items.len();
                if items.is_empty()
                    || response["count"]
                        .as_u64()
                        .is_some_and(|n| offset as u64 >= n)
                {
                    break;
                }
                ensure!(offset < 100_000, "VK My Music pagination did not finish");
            }
            ensure!(!ids.is_empty(), "VK My Music is empty; add tracks first");
            let mut random = std::io::BufReader::new(plavno_platform::Random::new()?);
            let queue = self.library.as_mut().unwrap();
            super::yandex::shuffle_favorites(&mut ids, queue.last.as_deref(), &mut random)?;
            queue.remaining = ids.into();
            queue.fresh_cycle = true;
        }
        loop {
            let ids: Vec<_> = self
                .library
                .as_ref()
                .unwrap()
                .remaining
                .iter()
                .take(32)
                .cloned()
                .collect();
            let response = self.api("audio.getById", vec![("audios", ids.join(","))])?;
            let items = response
                .as_array()
                .context("VK did not return My Music tracks")?;
            let mut tracks = HashMap::new();
            for item in items {
                if item["is_restricted"].as_bool() == Some(true)
                    || !item["url"]
                        .as_str()
                        .is_some_and(|url| url.starts_with("https://"))
                {
                    continue;
                }
                if let Some(track) = parse_track(item)
                    && ids.contains(&track.id)
                {
                    self.streams
                        .insert(track.id.clone(), (item.clone(), Instant::now()));
                    tracks.insert(track.id.clone(), track);
                }
            }
            let mut result: Vec<_> = ids.iter().filter_map(|id| tracks.remove(id)).collect();
            let queue = self.library.as_mut().unwrap();
            queue.remaining.drain(..ids.len());
            if queue.fresh_cycle
                && result.first().map(|t| &t.id) == queue.last.as_ref()
                && !result.is_empty()
            {
                if result.len() > 1 {
                    result.swap(0, 1);
                } else if !queue.remaining.is_empty() {
                    queue.remaining.push_back(result.pop().unwrap().id);
                    continue;
                }
            }
            if let Some(last) = result.last() {
                queue.last = Some(last.id.clone());
                queue.fresh_cycle = false;
                return Ok(result);
            }
            ensure!(
                !queue.remaining.is_empty(),
                "No playable VK My Music tracks are available"
            );
        }
    }

    fn trim_streams(&mut self) {
        self.streams
            .retain(|_, (_, created)| created.elapsed() < Duration::from_secs(60));
        self.hashes.retain(|id, _| self.streams.contains_key(id));
    }

    fn favorite_audio(&mut self, track: &Track) -> Result<Value> {
        self.init()?;
        let (owner, id) = track.id.split_once('_').context("Invalid VK track ID")?;
        ensure!(
            owner.parse::<i64>().is_ok_and(|n| n != 0) && id.parse::<u64>().is_ok_and(|n| n > 0),
            "Invalid VK track ID"
        );
        self.verify_api_user()?;
        let key = self
            .streams
            .get(&track.id)
            .and_then(|(item, _)| item["access_key"].as_str())
            .filter(|s| !s.is_empty());
        let audio = key.map_or_else(|| track.id.clone(), |key| format!("{}_{key}", track.id));
        let items = self.api("audio.getById", vec![("audios", audio)])?;
        items
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .find(|item| track_id(item).as_deref() == Some(&track.id))
            })
            .cloned()
            .context("VK did not return this track's favorite state")
    }
    fn reload(&mut self, ids: &[String]) -> Result<Vec<Value>> {
        let mut tracks = Vec::new();
        for ids in ids.chunks(10) {
            if let Some(last) = self.reloaded {
                std::thread::sleep(Duration::from_millis(1500).saturating_sub(last.elapsed()));
            }
            let response = self.request(
                &format!("{}/al_audio.php", self.site),
                &[
                    ("al", "1".into()),
                    ("act", "reload_audio".into()),
                    ("ids", ids.join(",")),
                ],
            )?;
            self.reloaded = Some(Instant::now());
            let data = ajax_data(&response)?;
            let items = ajax_payload(&data)?[0]
                .as_array()
                .context("VK did not return audio tracks")?;
            for item in items {
                let mut item = array_track(item).context("Invalid VK audio track")?;
                let link = item["url"].as_str().unwrap_or_default();
                if link.contains("audio_api_unavailable") {
                    item["url"] = Value::String(decode_url(link, self.user)?);
                }
                let key = track_id(&item).context("Invalid VK track ID")?;
                self.streams.insert(key, (item.clone(), Instant::now()));
                tracks.push(item);
            }
        }
        Ok(tracks)
    }
    fn web_track(&mut self, id: &str) -> Result<Value> {
        if let Some((item, created)) = self.streams.get(id)
            && created.elapsed() < Duration::from_secs(60)
        {
            return Ok(item.clone());
        }
        if !self.hashes.contains_key(id) {
            let address = format!("{}/audio{id}", self.mobile);
            let mut response = self
                .http
                .get(&address)
                .header("User-Agent", &self.user_agent)
                .call()
                .map_err(|_| anyhow::anyhow!("Cannot open VK audio page"))?;
            let html = read_response(response.body_mut())?;
            let pattern = regex_lite::Regex::new(r#"data-audio=(?:"([^"]*)"|'([^']*)')"#).unwrap();
            for capture in pattern.captures_iter(&html) {
                let encoded = capture.get(1).or_else(|| capture.get(2)).unwrap().as_str();
                let data: Value = serde_json::from_str(&html_escape::decode_html_entities(encoded))
                    .map_err(|_| anyhow::anyhow!("Invalid VK audio page metadata"))?;
                if let Some((key, hash)) = audio_hash(&data) {
                    self.hashes.insert(key, hash);
                }
            }
        }
        let hash = self
            .hashes
            .get(id)
            .context("VK did not expose this track; its web API may have changed")?
            .clone();
        self.reload(&[hash])?
            .into_iter()
            .find(|item| track_id(item).as_deref() == Some(id))
            .context("VK did not return the requested track")
    }
}

fn read_response(body: &mut ureq::Body) -> Result<String> {
    let encoding = match body.charset() {
        Some(charset) => encoding_rs::Encoding::for_label(charset.as_bytes())
            .context("Unsupported VK response encoding")?,
        None => encoding_rs::UTF_8,
    };
    let bytes = body.read_to_vec().map_err(response_read_error)?;
    let (text, _, malformed) = encoding.decode(&bytes);
    ensure!(!malformed, "Invalid VK response encoding");
    Ok(text.into_owned())
}

fn response_read_error(error: ureq::Error) -> anyhow::Error {
    let reason = match error {
        ureq::Error::Timeout(_) => "body timeout",
        ureq::Error::BodyExceedsLimit(_) => "body size limit exceeded",
        ureq::Error::Io(error) if error.kind() == std::io::ErrorKind::InvalidData => {
            "invalid text encoding or body data"
        }
        ureq::Error::Io(_) => "connection interrupted",
        _ => "invalid response body",
    };
    anyhow::anyhow!("Cannot read VK response ({reason})")
}

fn track_id(value: &Value) -> Option<String> {
    Some(format!(
        "{}_{}",
        value["owner_id"].as_i64()?,
        value["id"].as_u64()?
    ))
}

fn parse_track(item: &Value) -> Option<Track> {
    Some(Track {
        source: String::new(),
        id: track_id(item)?,
        title: item["title"].as_str()?.into(),
        artist: item["artist"].as_str().unwrap_or_default().into(),
        duration: item["duration"].as_f64(),
        cover_url: item["track_covers"]
            .as_array()
            .and_then(|covers| covers.last())
            .and_then(Value::as_str)
            .or_else(|| {
                ["photo_1200", "photo_600", "photo_300", "photo_135"]
                    .iter()
                    .find_map(|size| {
                        item["album"]["thumb"][size]
                            .as_str()
                            .or_else(|| item["thumb"][size].as_str())
                    })
            })
            .map(str::to_owned),
        genre: None,
        batch: String::new(),
    })
}

fn audio_liked(audio: &Value, user: i64) -> Result<bool> {
    match &audio["like"] {
        Value::Bool(value) => Ok(*value),
        Value::Number(value) if value.as_u64() == Some(0) => Ok(false),
        Value::Number(value) if value.as_u64() == Some(1) => Ok(true),
        Value::Null => Ok(audio["owner_id"].as_i64() == Some(user)),
        _ => anyhow::bail!("Invalid VK favorite state"),
    }
}

fn audio_hash(item: &Value) -> Option<(String, String)> {
    let (owner, id, action, hash) = if item.is_array() {
        let hashes: Vec<_> = item[13].as_str()?.split('/').collect();
        (
            item[1].as_i64()?,
            item[0].as_u64()?,
            *hashes.get(2)?,
            *hashes.get(5)?,
        )
    } else {
        (
            item["owner_id"].as_i64()?,
            item["id"].as_u64()?,
            item["actionHash"].as_str()?,
            item["urlHash"].as_str()?,
        )
    };
    if action.is_empty() || hash.is_empty() {
        return None;
    }
    Some((
        format!("{owner}_{id}"),
        format!("{owner}_{id}_{action}_{hash}"),
    ))
}

fn array_track(item: &Value) -> Option<Value> {
    Some(
        serde_json::json!({"id":item[0].as_u64()?,"owner_id":item[1].as_i64()?,"url":item[2].as_str()?,
        "title":web::text(item[3].as_str()?),"artist":web::text(item[4].as_str()?),"duration":item[5].as_u64(),
        "track_covers":item[14].as_str().unwrap_or_default().split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>(),
        "access_key":item[24].as_str()}),
    )
}

fn page_data(html: &str) -> Result<Value> {
    let tail = html
        .split_once("new AudioPage(")
        .context("VK did not expose recommendations; its web API may have changed")?
        .1;
    let tail = &tail[tail.find('{').context("Missing VK page data")?..];
    Value::deserialize(&mut serde_json::Deserializer::from_str(tail))
        .map_err(|_| anyhow::anyhow!("Invalid VK page data"))
}

fn section_cursor(section: &Value) -> &str {
    section["nextFrom"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| section["next_from"].as_str())
        .unwrap_or_default()
}

fn section_items(section: &Value) -> impl Iterator<Item = &Value> {
    section["playlist"]["list"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            section["playlists"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|playlist| {
                    playlist
                        .get("list")
                        .or_else(|| playlist.get("playlist").and_then(|p| p.get("list")))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                }),
        )
}

fn ajax_data(response: &str) -> Result<Value> {
    let text = response
        .trim()
        .strip_prefix("<!--")
        .unwrap_or(response.trim());
    serde_json::from_str(text.trim_end().strip_suffix("-->").unwrap_or(text))
        .map_err(|_| anyhow::anyhow!("Invalid VK AJAX response"))
}

fn ajax_payload(data: &Value) -> Result<&Value> {
    let payload = data.get("payload").context("Missing VK AJAX payload")?;
    let code = payload[0]
        .as_i64()
        .or_else(|| payload[0].as_str()?.parse().ok())
        .context("Missing VK AJAX response status")?;
    ensure!(
        code == 0,
        "VK audio access denied (code {code}); check session and music access"
    );
    payload.get(1).context("VK omitted AJAX data")
}

fn catalog_data(response: &str) -> Result<Value> {
    let data = ajax_data(response)?;
    if data.get("payload").is_some() {
        let section = &ajax_payload(&data)?[1];
        ensure!(section.is_object(), "VK omitted loaded catalog data");
        return Ok(section.clone());
    }
    ensure!(
        data.is_object() && (data.get("playlist").is_some() || data.get("playlists").is_some()),
        "VK omitted loaded catalog data"
    );
    Ok(data)
}

fn response_shape(value: &Value, depth: usize, budget: &mut usize) -> Value {
    let kind = match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    };
    if depth == 0 || *budget == 0 {
        return Value::String(kind.into());
    }
    *budget -= 1;
    match value {
        Value::Object(fields) => {
            let mut shape = serde_json::Map::new();
            for (name, value) in fields.iter().take(64) {
                let name = if !name.is_empty()
                    && name.len() <= 48
                    && name.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_')
                {
                    name.as_str()
                } else {
                    "<dynamic-field>"
                };
                shape.insert(name.into(), response_shape(value, depth - 1, budget));
            }
            Value::Object(shape)
        }
        Value::Array(items) => serde_json::json!({
            "type":"array", "length":items.len(),
            "samples":items.iter().take(3).map(|v| response_shape(v, depth - 1, budget)).collect::<Vec<_>>()
        }),
        _ => Value::String(kind.into()),
    }
}

const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN0PQRSTUVWXYZO123456789+/=";

fn decode64(encoded: &str) -> Vec<u8> {
    let mut index = 0;
    let mut bits = 0u32;
    let mut result = Vec::new();
    for byte in encoded.bytes() {
        if let Some(symbol) = ALPHABET.iter().position(|b| *b == byte) {
            if index % 4 != 0 {
                bits = bits.wrapping_shl(6).wrapping_add(symbol as u32);
                index += 1;
                result.push((bits >> ((-2i32 * index) & 6)) as u8);
            } else {
                bits = symbol as u32;
                index += 1;
            }
        }
    }
    result
}

fn shuffle(bytes: &mut [u8], mut seed: i64) {
    let len = bytes.len();
    if len == 0 {
        return;
    }
    let mut order = vec![0; len];
    for position in (0..len).rev() {
        seed = ((len * (position + 1)) as i64 ^ seed.wrapping_add(position as i64))
            .rem_euclid(len as i64);
        order[position] = seed as usize;
    }
    for position in 1..len {
        bytes.swap(position, order[len - 1 - position]);
    }
}

fn decode_url(link: &str, user: i64) -> Result<String> {
    let extra = link
        .split_once("?extra=")
        .context("Invalid VK audio URL")?
        .1;
    let (value, operations) = extra
        .split_once('#')
        .context("Missing VK audio URL operations")?;
    let mut value = decode64(value);
    let operations =
        String::from_utf8(decode64(operations)).context("Invalid VK audio URL operations")?;
    for operation in operations.split('\t').rev() {
        let mut parts = operation.split('\u{b}');
        let command = parts.next().unwrap_or_default();
        let argument = parts.next().unwrap_or_default();
        match command {
            "v" => value.reverse(),
            "x" => {
                let xor = argument.bytes().next().context("Missing VK XOR argument")?;
                for byte in &mut value {
                    *byte ^= xor;
                }
            }
            "r" => {
                let amount: i64 = argument.parse().context("Invalid VK rotation argument")?;
                for byte in &mut value {
                    if let Some(index) = ALPHABET.iter().position(|b| *b == *byte) {
                        *byte = ALPHABET[((index as i64 - amount)
                            .rem_euclid(ALPHABET.len() as i64 * 2)
                            as usize)
                            % ALPHABET.len()];
                    }
                }
            }
            "s" | "i" => {
                let seed: i64 = argument.parse().context("Invalid VK shuffle argument")?;
                shuffle(&mut value, if command == "i" { seed ^ user } else { seed });
            }
            _ => anyhow::bail!("Unknown VK audio URL operation"),
        }
    }
    String::from_utf8(value).context("Invalid decoded VK audio URL")
}

impl Backend for Vk {
    fn key(&self) -> &'static str {
        "vk"
    }
    fn name(&self) -> &'static str {
        "VK Music"
    }
    fn supports_favorites(&self) -> bool {
        true
    }
    fn supports_favorites_shuffle(&self) -> bool {
        true
    }
    fn supports_wave_styles(&self) -> bool {
        true
    }
    fn set_wave_style(&mut self, style: WaveStyle) -> Result<()> {
        if self.style != style {
            self.recent.clear();
        }
        self.style = style;
        Ok(())
    }
    fn set_dislike(&mut self, track: &Track, disliked: bool) -> Result<bool> {
        self.init()?;
        let (owner, id) = track.id.split_once('_').context("Invalid VK track ID")?;
        ensure!(
            owner.parse::<i64>().is_ok_and(|n| n != 0) && id.parse::<u64>().is_ok_and(|n| n > 0),
            "Invalid VK track ID"
        );
        self.verify_api_user()?;
        let result = self.api(
            if disliked {
                "audio.addDislike"
            } else {
                "audio.removeDislike"
            },
            vec![("audio_ids", track.id.clone())],
        )?;
        ensure!(
            result == serde_json::json!(1)
                || result == serde_json::json!(true)
                || result["audio_ids"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(&track.id))),
            "VK did not confirm the dislike change"
        );
        Ok(true)
    }
    fn favorite_state(&mut self, track: &Track) -> Result<Option<bool>> {
        let audio = self.favorite_audio(track)?;
        Ok(Some(audio_liked(&audio, self.user)?))
    }
    fn toggle_favorite(&mut self, track: &Track) -> Result<bool> {
        let audio = self.favorite_audio(track)?;
        let liked = !audio_liked(&audio, self.user)?;
        let mut fields = vec![
            ("owner_id", audio["owner_id"].to_string()),
            ("audio_id", audio["id"].to_string()),
        ];
        if let Some(key) = audio["access_key"].as_str().filter(|s| !s.is_empty()) {
            fields.push(("access_key", key.into()));
        }
        if liked && let Some(code) = audio["track_code"].as_str() {
            fields.push(("track_code", code.into()));
        }
        let result = self.api(if liked { "audio.add" } else { "audio.delete" }, fields)?;
        let confirmed = if liked {
            result.as_u64().is_some_and(|n| n > 0)
                || result["items"][0]["new_audio_id"]
                    .as_u64()
                    .is_some_and(|n| n > 0)
        } else {
            result.as_u64() == Some(1)
                || result["audio_ids"].as_array().is_some_and(|ids| {
                    ids.iter().any(|id| {
                        id.as_str()
                            .and_then(|id| id.split_once('_'))
                            .is_some_and(|(owner, id)| {
                                owner.parse::<i64>().ok() == Some(self.user)
                                    && id.parse::<u64>().is_ok_and(|id| id > 0)
                            })
                    })
                })
        };
        ensure!(confirmed, "VK did not confirm the favorites change");
        ensure!(
            self.favorite_state(track)? == Some(liked),
            "VK favorite state did not match the requested change"
        );
        Ok(liked)
    }
    fn start(&mut self) -> Result<Vec<Track>> {
        self.library = None;
        self.mix_active = self.style != WaveStyle::Balanced || self.api_user_verified;
        self.mix_options_id = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)?
            .as_millis() as u64)
            .max(self.mix_options_id.saturating_add(1));
        self.more(&[])
    }
    fn start_mode(&mut self, mode: PlaybackMode) -> Result<Vec<Track>> {
        match mode {
            PlaybackMode::MyVibe => self.start(),
            PlaybackMode::FavoritesShuffle => {
                self.library = Some(LibraryQueue::default());
                self.library_batch()
            }
        }
    }
    fn more(&mut self, history: &[String]) -> Result<Vec<Track>> {
        if self.library.is_some() {
            return self.library_batch();
        }
        self.init()?;
        let history: Vec<_> = history
            .iter()
            .cloned()
            .chain(self.recent.iter().cloned())
            .collect();
        self.trim_streams();
        let items = if self.mix_active {
            self.verify_api_user()?;
            let mut options = serde_json::json!({"id":self.mix_options_id});
            if self.style != WaveStyle::Balanced {
                options["vibes"] = serde_json::json!([if self.style == WaveStyle::Energetic {
                    "active"
                } else {
                    "calm"
                }]);
            }
            if self.style == WaveStyle::Work {
                options["langs"] = serde_json::json!(["instrumental"]);
            }
            let mut items = Vec::new();
            for _ in 0..4 {
                let response = self.api(
                    "audio.getStreamMixAudios",
                    vec![
                        ("mix_id", "common".into()),
                        ("options", options.to_string()),
                    ],
                )?;
                let mut seen = HashSet::new();
                items = response
                    .as_array()
                    .context("VK did not return mix tracks")?
                    .iter()
                    .filter(|item| {
                        track_id(item).is_some_and(|id| !history.contains(&id) && seen.insert(id))
                    })
                    .cloned()
                    .collect();
                if !items.is_empty() {
                    break;
                }
            }
            for item in &items {
                if let Some(id) = track_id(item) {
                    self.streams.insert(id, (item.clone(), Instant::now()));
                }
            }
            items
        } else if self.config.as_ref().unwrap()["cookies"].is_string()
            || self.config.as_ref().unwrap()["cookie"].is_string()
        {
            let ids: Vec<_> = self
                .recommendation_ids(&history)?
                .into_iter()
                .map(|(id, hash)| {
                    self.hashes.insert(id, hash.clone());
                    hash
                })
                .collect();
            self.reload(&ids)?
        } else {
            self.api("audio.getRecommendations", vec![("count", "30".into())])?["items"]
                .as_array()
                .context("Missing VK recommendations")?
                .clone()
        };
        let tracks: Vec<_> = items
            .iter()
            .filter_map(parse_track)
            .filter(|track| !history.contains(&track.id))
            .collect();
        for track in &tracks {
            self.recent.push_back(track.id.clone());
            if self.recent.len() > 200 {
                self.recent.pop_front();
            }
        }
        Ok(tracks)
    }
    fn resolve(&mut self, track: &Track) -> Result<Input> {
        self.init()?;
        let (owner, id) = track.id.split_once('_').context("Invalid VK track ID")?;
        owner.parse::<i64>().context("Invalid VK owner ID")?;
        id.parse::<u64>().context("Invalid VK audio ID")?;
        let item = if let Some((item, created)) = self.streams.get(&track.id)
            && created.elapsed() < Duration::from_secs(60)
        {
            item.clone()
        } else if self.style == WaveStyle::Balanced
            && !self.mix_active
            && self.library.is_none()
            && (self.config.as_ref().unwrap()["cookies"].is_string()
                || self.config.as_ref().unwrap()["cookie"].is_string())
        {
            self.web_track(&track.id)?
        } else {
            self.api("audio.getById", vec![("audios", track.id.clone())])?
                .as_array()
                .and_then(|items| items.first())
                .cloned()
                .context("VK did not return this track")?
        };
        let link = item["url"]
            .as_str()
            .filter(|link| link.starts_with("https://"))
            .context("VK did not provide a full audio stream")?;
        let pattern =
            regex_lite::Regex::new(r"/[0-9a-f]+(/audios)?/([0-9a-f]+)/index\.m3u8").unwrap();
        let link = pattern.replace(link, "${1}/${2}.mp3").into_owned();
        if link.contains(".m3u8") {
            return web::transcode(link, "VK Music · service audio".into());
        }
        Ok(Input::Http {
            headers: Vec::new(),
            url: link,
            aes128_key: None,
            hint: "mp3",
            quality: "VK Music · MP3".into(),
        })
    }
    fn feedback(&mut self, _: &Track, _: Feedback, _: f64) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn expired_tokens_exchange_session_cookies_once_and_refresh_before_expiry() {
        let expires = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let (base, server) = web::fixture_server_with_headers(|_| {
            vec![
            (json!({"error":{"error_code":5}}).to_string(), ""),
            ("Audio page".into(), "Set-Cookie: httoken=fresh; Path=/; Max-Age=3600\r\n"),
            (json!({"type":"okay","data":{"user_id":123,"access_token":"fixture-renewed-1","expires":expires}}).to_string(), ""),
            (json!({"response":[{"id":123}]}).to_string(), ""),
            (json!({"response":[favorite_item(false)]}).to_string(), ""),
            (json!({"response":[favorite_item(false)]}).to_string(), ""),
            ("Audio page".into(), "Set-Cookie: httoken=second; Path=/; Max-Age=3600\r\n"),
            (json!({"type":"okay","data":{"user_id":123,"access_token":"fixture-renewed-2","expires":expires}}).to_string(), ""),
            (json!({"response":[favorite_item(false)]}).to_string(), ""),
        ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.api = format!("{base}method/");
        backend.site = base.trim_end_matches('/').into();
        backend.user = 123;
        backend.config = Some(json!({"cookie":"fixture","token":"fixture-expired"}));
        web::Cookies::parse("127.0.0.1\tFALSE\t/\tFALSE\t0\tsession\tfixture-session\n")
            .unwrap()
            .seed_agent(&backend.http)
            .unwrap();
        let track = favorite_track();
        assert_eq!(backend.favorite_state(&track).unwrap(), Some(false));
        let deadline = backend.web_token.as_ref().unwrap().1;
        assert!(deadline > Instant::now() + Duration::from_secs(3500));
        assert!(deadline < Instant::now() + Duration::from_secs(3600));
        assert_eq!(backend.favorite_state(&track).unwrap(), Some(false));
        backend.web_token.as_mut().unwrap().1 = Instant::now();
        assert_eq!(backend.favorite_state(&track).unwrap(), Some(false));
        let requests = server.join().unwrap();
        assert!(requests[0].contains("access_token=fixture-expired"));
        assert!(requests[1].starts_with("GET /audio"));
        assert!(
            requests[2].contains("httoken=fresh")
                && requests[2].contains("session=fixture-session")
        );
        for request in &requests[3..6] {
            assert!(request.contains("access_token=fixture-renewed-1"));
        }
        assert!(requests[7].contains("httoken=second"));
        assert!(requests[8].contains("access_token=fixture-renewed-2"));
    }

    #[test]
    fn repeated_auth_rejection_stops_after_one_exchange_without_a_mutation() {
        for code in [5, 1116, 1117] {
            let (base, server) = web::fixture_server(|_| {
                vec![
                json!({"error":{"error_code":code}}).to_string(),
                json!({"type":"okay","data":{"user_id":123,"access_token":"fixture-renewed","expires":3600}}).to_string(),
                json!({"error":{"error_code":code}}).to_string(),
            ]
            });
            let mut backend = Vk::new(PathBuf::new());
            backend.api = format!("{base}method/");
            backend.site = base.trim_end_matches('/').into();
            backend.user = 123;
            backend.config = Some(json!({"cookie":"fixture","token":"fixture-expired"}));
            let error = backend
                .toggle_favorite(&favorite_track())
                .unwrap_err()
                .to_string();
            assert!(error.contains("session is invalid") && !error.contains("fixture-renewed"));
            let requests = server.join().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(
                requests
                    .iter()
                    .all(|r| !r.contains("/audio.add") && !r.contains("/audio.delete"))
            );
        }
    }

    #[test]
    fn cookie_cache_keeps_rotations_and_yields_to_a_new_manual_profile() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "plavno-vk-cookie-cache-{}-{suffix}",
            std::process::id()
        ));
        let profile = root.join("vk.json");
        crate::config::save_private(
            &profile,
            br#"{"cookie":"session=original; remixuserid=123","site":"https://vk.ru"}"#,
        )
        .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&profile)
            .unwrap()
            .set_times(
                std::fs::FileTimes::new()
                    .set_modified(std::time::UNIX_EPOCH + Duration::from_secs(1000)),
            )
            .unwrap();
        let mut first = Vk::new(profile.clone());
        first.init().unwrap();
        let uri: ureq::http::Uri = "https://vk.ru/".parse().unwrap();
        first
            .http
            .cookie_jar_lock()
            .insert(
                ureq::Cookie::parse("session=updated; Domain=vk.ru; Path=/; Max-Age=3600", &uri)
                    .unwrap(),
                &uri,
            )
            .unwrap();
        first.save_cookie_cache();
        let mut restored = Vk::new(profile.clone());
        restored.init().unwrap();
        assert_eq!(
            restored
                .http
                .cookie_jar_lock()
                .get("vk.ru", "/", "session")
                .unwrap()
                .value(),
            "updated"
        );
        crate::config::save_private(
            &profile,
            br#"{"cookie":"session=fresh; remixuserid=123","site":"https://vk.ru"}"#,
        )
        .unwrap();
        let mut fresh = Vk::new(profile);
        fresh.init().unwrap();
        assert_eq!(
            fresh
                .http
                .cookie_jar_lock()
                .get("vk.ru", "/", "session")
                .unwrap()
                .value(),
            "fresh"
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn balanced_mix_restarts_keep_recent_tracks_and_refresh_expired_streams() {
        let audio = |id| json!({"id":id,"owner_id":-2,"title":"Track","url":"https://audio.example/track.mp3"});
        let (base, server) = web::fixture_server(|_| {
            vec![
                json!({"response":[audio(9),audio(10)]}).to_string(),
                json!({"response":[audio(9),audio(9)]}).to_string(),
                json!({"response":[audio(9),audio(11),audio(11)]}).to_string(),
                json!({"response":[audio(11)]}).to_string(),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.api = base;
        backend.config = Some(json!({"token":"fixture-token","cookie":"session=fixture"}));
        backend.api_user_verified = true;
        backend.user = 123;
        assert_eq!(backend.start().unwrap().len(), 2);
        let first_id = backend.mix_options_id;
        let tracks = backend.start().unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "-2_11");
        assert!(backend.mix_options_id > first_id);
        backend.streams.get_mut("-2_11").unwrap().1 = Instant::now() - Duration::from_secs(61);
        assert!(matches!(
            backend.resolve(&tracks[0]).unwrap(),
            Input::Http { .. }
        ));
        let requests = server.join().unwrap();
        for request in &requests[..3] {
            assert!(request.starts_with("POST /audio.getStreamMixAudios"));
            let fields: HashMap<_, _> =
                url::form_urlencoded::parse(request.split_once("\r\n\r\n").unwrap().1.as_bytes())
                    .into_owned()
                    .collect();
            let options: Value = serde_json::from_str(&fields["options"]).unwrap();
            assert!(options["id"].is_u64());
            assert!(options.get("vibes").is_none());
        }
        assert!(requests[3].starts_with("POST /audio.getById"));
    }

    #[test]
    fn my_music_pages_the_library_skips_unavailable_tracks_and_refreshes_each_pass() {
        let audio = |id| json!({"id":id,"owner_id":123,"title":"Library track","url":"https://audio.example/track.mp3"});
        let (base, server) = web::fixture_server(|_| {
            vec![
            json!({"response":[{"id":123}]}).to_string(),
            json!({"response":{"count":3,"items":[audio(1),audio(2)]}}).to_string(),
            json!({"response":{"count":3,"items":[audio(3)]}}).to_string(),
            json!({"response":[audio(2),audio(1),{"id":3,"owner_id":123,"title":"Unavailable"},audio(999)]}).to_string(),
            json!({"response":{"count":1,"items":[audio(4)]}}).to_string(),
            json!({"response":[audio(4)]}).to_string(),
        ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.api = base;
        backend.config = Some(json!({"token":"fixture-token"}));
        let tracks = backend.start_mode(PlaybackMode::FavoritesShuffle).unwrap();
        assert_eq!(
            tracks.iter().map(|t| t.id.as_str()).collect::<HashSet<_>>(),
            HashSet::from(["123_1", "123_2"])
        );
        assert_eq!(backend.streams.len(), 2);
        assert!(backend.recent.is_empty());
        for track in &tracks {
            assert!(matches!(
                backend.resolve(track).unwrap(),
                Input::Http { .. }
            ));
        }
        assert_eq!(backend.more(&["123_4".into()]).unwrap()[0].id, "123_4");
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /audio.get"));
        assert!(requests[1].contains("owner_id=123") && requests[1].contains("offset=0"));
        assert!(requests[2].contains("offset=2"));
        assert!(requests[3].starts_with("POST /audio.getById"));
    }

    #[test]
    fn my_music_keeps_pending_ids_after_metadata_errors_and_reports_empty_libraries() {
        let audio = json!({"id":1,"owner_id":123,"title":"Library track","url":"https://audio.example/track.mp3"});
        let (base, server) = web::fixture_server(|_| {
            vec![
                json!({"response":[{"id":123}]}).to_string(),
                json!({"response":{"count":1,"items":[audio.clone()]}}).to_string(),
                json!({"error":{"error_code":6,"error_msg":"private error"}}).to_string(),
                json!({"response":[audio]}).to_string(),
                json!({"response":{"count":0,"items":[]}}).to_string(),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.api = base;
        backend.config = Some(json!({"token":"fixture-token"}));
        assert!(backend.start_mode(PlaybackMode::FavoritesShuffle).is_err());
        assert_eq!(backend.library.as_ref().unwrap().remaining.len(), 1);
        assert_eq!(backend.more(&[]).unwrap()[0].id, "123_1");
        assert!(
            backend
                .more(&[])
                .unwrap_err()
                .to_string()
                .contains("My Music is empty")
        );
        assert_eq!(server.join().unwrap().len(), 5);
    }

    #[test]
    fn tuned_mixes_use_numeric_options_ids_cache_streams_and_filter_history() {
        let audio = json!({"id":9,"owner_id":-2,"title":"Track","artist":"Artist","url":"https://audio.example/track.mp3","album":{"thumb":{"photo_600":"https://cover.example/600.jpg"}}});
        let (base, server) = web::fixture_server(|_| {
            vec![
                json!({"response":[{"id":123}]}).to_string(),
                json!({"response":[audio.clone()]}).to_string(),
                json!({"response":[audio.clone()]}).to_string(),
                json!({"response":[audio.clone()]}).to_string(),
                json!({"response":[audio.clone()]}).to_string(),
                json!({"response":[audio.clone()]}).to_string(),
                json!({"response":[audio.clone()]}).to_string(),
                json!({"response":[audio]}).to_string(),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.api = base;
        backend.config = Some(json!({"token":"fixture-token"}));
        for style in [WaveStyle::Calm, WaveStyle::Energetic, WaveStyle::Work] {
            let tracks = backend
                .start_with_style("vk", super::super::PlaybackMode::MyVibe, style)
                .unwrap();
            assert_eq!(tracks.len(), 1);
            assert_eq!(
                tracks[0].cover_url.as_deref(),
                Some("https://cover.example/600.jpg")
            );
            assert!(matches!(
                backend.resolve(&tracks[0]).unwrap(),
                Input::Http { .. }
            ));
        }
        assert!(backend.more(&["-2_9".into()]).unwrap().is_empty());
        let requests = server.join().unwrap();
        for (request, vibe) in requests[1..]
            .iter()
            .zip(["calm", "active", "calm", "calm", "calm", "calm", "calm"])
        {
            assert!(request.starts_with("POST /audio.getStreamMixAudios"));
            let body = request.split_once("\r\n\r\n").unwrap().1;
            let fields: HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
                .into_owned()
                .collect();
            assert_eq!(fields["client_id"], "6287487");
            assert_eq!(fields["mix_id"], "common");
            let options: Value = serde_json::from_str(&fields["options"]).unwrap();
            assert!(options["id"].as_u64().unwrap() >= 1_000_000_000_000);
            assert_eq!(options["vibes"], json!([vibe]));
        }
        let body = requests[3].split_once("\r\n\r\n").unwrap().1;
        let fields: HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
            .into_owned()
            .collect();
        assert_eq!(
            serde_json::from_str::<Value>(&fields["options"]).unwrap()["langs"],
            json!(["instrumental"])
        );
    }

    #[test]
    #[ignore = "Read-only My Music shuffle check using the configured VK account"]
    fn live_session_reads_my_music_shuffle() -> Result<()> {
        let mut backend = Vk::new(crate::config::profile_path("vk")?);
        let first = backend.start_mode(PlaybackMode::FavoritesShuffle)?;
        ensure!(!first.is_empty(), "VK returned no My Music tracks");
        backend.resolve(&first[0])?;
        let next = backend.more(&[])?;
        println!(
            "VK My Music shuffle: {} first, {} next; stream available",
            first.len(),
            next.len()
        );
        Ok(())
    }

    #[test]
    #[ignore = "Read-only wave style and stream checks using configured VK account"]
    fn live_session_reads_tuned_mixes() -> Result<()> {
        let mut backend = Vk::new(crate::config::profile_path("vk")?);
        for style in [WaveStyle::Calm, WaveStyle::Energetic, WaveStyle::Work] {
            let tracks =
                backend.start_with_style("vk", super::super::PlaybackMode::MyVibe, style)?;
            let track = tracks.first().context("VK returned no tuned mix tracks")?;
            backend.resolve(track)?;
            backend.favorite_state(track)?;
            println!(
                "VK {} mix: {} tracks; stream and favorites available",
                style.label(),
                tracks.len()
            );
        }
        Ok(())
    }

    #[test]
    #[ignore = "Read-only VK wave restart check using the configured account"]
    fn live_session_restarts_without_repeating_the_previous_wave() -> Result<()> {
        let style = crate::config::wave_styles()?
            .get("vk")
            .copied()
            .unwrap_or_default();
        let mut backend = Vk::new(crate::config::profile_path("vk")?);
        backend.set_wave_style(style)?;
        let first = backend.start()?;
        if let Some(track) = first.first() {
            backend.favorite_state(track)?;
        }
        let mut previous: HashSet<_> = first.iter().map(|track| track.id.clone()).collect();
        for restart in 1..=3 {
            let next = backend.start()?;
            let fresh = next
                .iter()
                .filter(|track| !previous.contains(&track.id))
                .count();
            println!(
                "VK {} restart {restart}: {} recent, {} next, {fresh} new tracks",
                style.label(),
                previous.len(),
                next.len()
            );
            ensure!(fresh > 0, "VK restart repeated the previous wave");
            ensure!(fresh == next.len(), "VK restart included recent tracks");
            previous.extend(next.iter().map(|track| track.id.clone()));
        }
        Ok(())
    }

    #[test]
    #[ignore = "Adds then removes one previously unliked track using configured VK account"]
    fn live_session_round_trips_a_favorite() -> Result<()> {
        let mut backend = Vk::new(crate::config::profile_path("vk")?);
        let tracks =
            backend.start_with_style("vk", super::super::PlaybackMode::MyVibe, WaveStyle::Work)?;
        for track in tracks {
            if backend.favorite_state(&track)? != Some(false) {
                continue;
            }
            let added = backend.toggle_favorite(&track);
            let removed = if backend.favorite_state(&track)? == Some(true) {
                backend.toggle_favorite(&track)
            } else {
                Ok(false)
            };
            ensure!(
                backend.favorite_state(&track)? == Some(false),
                "VK probe did not restore the original favorite state"
            );
            ensure!(
                added? && !removed?,
                "VK did not confirm the favorite round trip"
            );
            println!("VK favorite add/remove confirmed; original library state restored");
            return Ok(());
        }
        anyhow::bail!("No previously unliked track available for the VK round-trip check")
    }

    fn favorite_track() -> Track {
        Track {
            source: "vk".into(),
            id: "-2_9".into(),
            title: "Track".into(),
            artist: "Artist".into(),
            duration: Some(120.0),
            cover_url: None,
            genre: None,
            batch: String::new(),
        }
    }

    fn favorite_item(liked: bool) -> Value {
        json!({"id":9,"owner_id":-2,"like":liked,"access_key":"fixture-key","track_code":"fixture-code"})
    }

    #[test]
    fn dislikes_use_audio_ids_and_reject_unconfirmed_results() {
        let (base, server) = web::fixture_server(|_| {
            vec![
                json!({"response":[{"id":123}]}).to_string(),
                json!({"response":1}).to_string(),
                json!({"response":{"audio_ids":["-2_9"]}}).to_string(),
                json!({"response":0}).to_string(),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.api = format!("{base}method/");
        backend.config = Some(json!({"token":"fixture-token"}));
        backend.user = 123;
        assert!(backend.set_dislike(&favorite_track(), true).unwrap());
        assert!(backend.set_dislike(&favorite_track(), false).unwrap());
        assert!(
            backend
                .set_dislike(&favorite_track(), true)
                .unwrap_err()
                .to_string()
                .contains("confirm")
        );
        let requests = server.join().unwrap();
        assert!(requests[1].starts_with("POST /method/audio.addDislike"));
        assert!(requests[2].starts_with("POST /method/audio.removeDislike"));
        for request in &requests[1..] {
            assert!(request.contains("audio_ids=-2_9"));
        }
    }

    #[test]
    fn native_favorites_use_current_state_and_confirm_add_and_remove() {
        let (base, server) = web::fixture_server(|_| {
            vec![
                json!({"type":"okay","data":{"access_token":"fixture-web-token","user_id":123}})
                    .to_string(),
                json!({"response":[{"id":123}]}).to_string(),
                json!({"response":[favorite_item(false)]}).to_string(),
                json!({"response":{"items":[{"new_audio_id":77}]}}).to_string(),
                json!({"response":[favorite_item(true)]}).to_string(),
                json!({"response":[favorite_item(true)]}).to_string(),
                json!({"response":{"audio_ids":["123_77"]}}).to_string(),
                json!({"response":[favorite_item(false)]}).to_string(),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.site = base.trim_end_matches('/').into();
        backend.api = format!("{base}method/");
        backend.user = 123;
        backend.config = Some(json!({"cookie":"fixture"}));
        let track = favorite_track();
        let added = backend.toggle_favorite(&track);
        let removed = backend.toggle_favorite(&track);
        let requests = server.join().unwrap();
        assert!(added.unwrap());
        assert!(!removed.unwrap());
        assert_eq!(requests.len(), 8);
        assert!(requests[0].contains("act=web_token") && requests[0].contains("app_id=6287487"));
        for index in [2, 4, 5, 7] {
            assert!(requests[index].starts_with("POST /method/audio.getById"));
        }
        assert!(requests[3].starts_with("POST /method/audio.add"));
        assert!(requests[3].contains("owner_id=-2") && requests[3].contains("audio_id=9"));
        assert!(
            requests[3].contains("access_key=fixture-key")
                && requests[3].contains("track_code=fixture-code")
        );
        assert!(requests[6].starts_with("POST /method/audio.delete"));
        assert!(requests[6].contains("owner_id=-2") && !requests[6].contains("group_id"));
        assert!(backend.supports_favorites() && backend.supports_favorites_shuffle());
    }

    #[test]
    fn favorites_reject_wrong_accounts_unconfirmed_changes_and_private_api_errors() {
        for (reply, after, expected) in [
            (json!({"response":0}), None, "did not confirm"),
            (
                json!({"response":{"items":[{"new_audio_id":77}]}}),
                Some(favorite_item(false)),
                "did not match",
            ),
            (
                json!({"error":{"error_code":15,"error_msg":"private-fixture-token"}}),
                None,
                "code 15",
            ),
        ] {
            let (base, server) = web::fixture_server(|_| {
                let mut replies = vec![
                    json!({"response":[{"id":123}]}),
                    json!({"response":[favorite_item(false)]}),
                    reply,
                ];
                if let Some(after) = after {
                    replies.push(json!({"response":[after]}));
                }
                replies.into_iter().map(|r| r.to_string()).collect()
            });
            let mut backend = Vk::new(PathBuf::new());
            backend.api = base;
            backend.user = 123;
            backend.config = Some(json!({"token":"fixture-token"}));
            let error = backend
                .toggle_favorite(&favorite_track())
                .unwrap_err()
                .to_string();
            server.join().unwrap();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("private-fixture-token"));
        }
        let (base, server) =
            web::fixture_server(|_| vec![json!({"response":[{"id":456}]}).to_string()]);
        let mut backend = Vk::new(PathBuf::new());
        backend.api = base;
        backend.user = 123;
        backend.config = Some(json!({"cookie":"fixture", "token":"fixture-token"}));
        let error = backend
            .toggle_favorite(&favorite_track())
            .unwrap_err()
            .to_string();
        assert!(error.contains("different account"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    fn expired_web_sessions_do_not_send_a_favorite_mutation() {
        let (base, server) = web::fixture_server(|_| {
            vec![json!({"type":"error","error_info":"private-fixture-token"}).to_string()]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.site = base.trim_end_matches('/').into();
        backend.user = 123;
        backend.config = Some(json!({"cookies":"fixture"}));
        let error = backend
            .toggle_favorite(&favorite_track())
            .unwrap_err()
            .to_string();
        assert!(error.contains("signed-in session") && !error.contains("private-fixture-token"));
        assert_eq!(server.join().unwrap().len(), 1);
    }

    #[test]
    #[ignore = "Read-only favorites check using the configured VK session"]
    fn live_session_reads_favorite_state() -> Result<()> {
        let mut backend = Vk::new(crate::config::profile_path("vk")?);
        let tracks = backend.start()?;
        let track = tracks.first().context("VK returned no tracks")?;
        let state = backend.favorite_state(track)?;
        ensure!(state.is_some(), "VK omitted favorite state");
        println!("VK favorite state read successfully");
        Ok(())
    }

    #[test]
    fn desktop_reload_uses_the_signed_in_site_and_ajax_payload() {
        let item = json!([
            9,
            -2,
            "https://cdn.example/audio.mp3",
            "Track",
            "Artist",
            120,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            "one/two/action/four/five/url",
            ""
        ]);
        let (base, server) = web::fixture_server(|_| {
            vec![format!("<!--{}", json!({"payload":[0,[[item],false,[]]]}))]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.site = base.trim_end_matches('/').into();
        backend.mobile = backend.site.clone();
        let result = backend.reload(&["-2_9_action_url".into()]);
        let requests = server.join().unwrap();
        assert_eq!(result.unwrap().len(), 1);
        assert!(requests[0].starts_with("POST /al_audio.php"));
        assert!(requests[0].contains("al=1") && requests[0].contains("act=reload_audio"));
    }

    #[test]
    fn windows_1251_catalog_response_preserves_track_text() {
        use std::{
            io::{BufRead, BufReader, Write},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = format!("http://{}/al_audio.php", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = b"<!--{\"payload\":[0,[null,{\"title\":\"".to_vec();
            body.extend_from_slice(&[0xcc, 0xf3, 0xe7, 0xfb, 0xea, 0xe0]);
            body.extend_from_slice(b"\"}]]}");
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=windows-1251\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(&body).unwrap();
        });
        let backend = Vk::new(PathBuf::new());
        let response = backend.request(&address, &[]);
        server.join().unwrap();
        assert_eq!(
            catalog_data(&response.unwrap()).unwrap()["title"],
            "\u{41c}\u{443}\u{437}\u{44b}\u{43a}\u{430}"
        );
    }

    #[test]
    #[ignore = "Read-only playback check using the configured VK session"]
    fn live_session_decodes_three_tracks_and_seeks_in_ram() -> Result<()> {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Cancel(Arc<AtomicBool>);
        impl Drop for Cancel {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let mut backend = Vk::new(crate::config::profile_path("vk")?);
        let tracks = backend.start()?;
        ensure!(
            tracks.len() >= 3,
            "Live VK queue has fewer than three tracks"
        );
        println!("VK recommendation tracks: {}", tracks.len());
        for (index, track) in tracks.iter().take(3).enumerate() {
            let mut input = backend.resolve(track)?;
            if let Input::Pipe { program, .. } = &mut input {
                *program =
                    PathBuf::from(std::env::var_os("PLAVNO_TEST_PLAYER").context(
                        "Set PLAVNO_TEST_PLAYER to a built plavno binary for HLS playback",
                    )?);
            }
            println!(
                "Track {} input: {}",
                index + 1,
                match &input {
                    Input::Http { .. } => "HTTP",
                    Input::Pipe { .. } => "ffmpeg",
                    _ => "other",
                }
            );
            let buffer = crate::audio::cache_input(input)
                .map_err(|_| anyhow::anyhow!("Live VK audio buffer could not be opened"))?;
            let cancelled = Cancel(Arc::new(AtomicBool::new(false)));
            let mut prepared = crate::audio::prepare(buffer.clone(), cancelled.0.clone())
                .map_err(|_| anyhow::anyhow!("Live VK audio could not be decoded"))?;
            let samples: Vec<_> = prepared
                .source
                .by_ref()
                .take(prepared.samples_per_second as usize)
                .collect();
            ensure!(
                !samples.is_empty()
                    && samples.iter().all(|s| s.is_finite())
                    && samples.iter().any(|s| s.abs() > 0.00001),
                "Live VK PCM is empty or silent"
            );
            println!(
                "Track {}: PCM samples {}, cached bytes {}",
                index + 1,
                samples.len(),
                buffer.buffered_bytes()
            );
            if index == 0 {
                let seek_cancelled = Cancel(Arc::new(AtomicBool::new(false)));
                let mut seeked =
                    crate::audio::prepare_at(buffer.clone(), seek_cancelled.0.clone(), 10.0)
                        .map_err(|_| anyhow::anyhow!("Live VK seek through RAM buffer failed"))?;
                ensure!(
                    seeked.source.by_ref().take(4096).count() == 4096,
                    "Live VK seek returned no PCM"
                );
                println!("Seek to 10 seconds through the same buffer: OK");
            }
            buffer.cancel();
        }
        Ok(())
    }

    #[test]
    fn manual_profile_accepts_a_cookie_header_and_detects_user_id() {
        let root = std::env::temp_dir().join(format!("plavno-vk-manual-{}", std::process::id()));
        let path = root.join("vk.json");
        crate::config::save_private(&path, br#"{"cookie":"remixsid6=fixture; remixuserid=123","site":"https://vk.com","user_agent":"Fixture"}"#).unwrap();
        let mut backend = Vk::new(path);
        let result = backend.init();
        std::fs::remove_dir_all(root).unwrap();
        result.unwrap();
        assert_eq!(backend.user, 123);
        assert_eq!(backend.mobile, "https://m.vk.com");
        assert_eq!(backend.user_agent, "Fixture");
        assert!(
            backend
                .cookies
                .header(&backend.mobile)
                .unwrap()
                .contains("remixsid6=fixture")
        );
        assert_eq!(
            backend
                .cookies
                .header("https://api.vk.com/method/")
                .unwrap(),
            "remixsid6=fixture; remixuserid=123"
        );
        assert!(
            backend
                .cookies
                .header("https://vk.ru/audio")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn empty_recommendation_shell_loads_its_catalog_before_reloading_tracks() {
        let item = json!([
            9,
            -2,
            "https://cdn.example/audio.mp3",
            "Track",
            "Artist",
            120,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            "one/two/action/four/five/url",
            ""
        ]);
        let (base, server) = web::fixture_server(|_| {
            vec![
                format!(
                    "<script>new AudioPage(0,{},false)</script>",
                    json!({
                        "onlySkeleton":true,"sectionData":{"recoms":{
                            "blockIds":[],"hints":[],"playlist":null,"playlists":[],
                            "sectionId":"fixture-section","nextFrom":"fixture-cursor"
                        }}
                    })
                ),
                format!(
                    "<!--{}",
                    json!({"payload":[0,[null,{
                        "sectionId":"fixture-section","nextFrom":"",
                        "playlist":{"list":[item.clone()]}
                    }]]})
                ),
                json!({"payload":[0,[[item],false,[]]]}).to_string(),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.site = base.trim_end_matches('/').into();
        backend.mobile = backend.site.clone();
        backend.user = 123;
        backend.config = Some(json!({"cookies":"fixture"}));
        let tracks = backend.start().unwrap();
        assert_eq!(tracks[0].id, "-2_9");
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 3);
        assert!(requests[1].starts_with("POST /al_audio.php"));
        assert!(requests[1].contains("act=load_catalog_section"));
        assert!(requests[1].contains("section_id=fixture-section"));
        assert!(requests[1].contains("start_from=fixture-cursor"));
        assert!(requests[2].contains("act=reload_audio"));
    }

    #[test]
    fn uninitialized_catalog_requests_recoms_and_filters_history_and_duplicates() {
        let item = |id| {
            json!([
                id,
                -2,
                "https://cdn.example/audio.mp3",
                "Track",
                "Artist",
                120,
                0,
                0,
                0,
                0,
                0,
                0,
                0,
                "one/two/action/four/five/url",
                ""
            ])
        };
        let (base, server) = web::fixture_server(|_| {
            vec![
                format!(
                    "new AudioPage(0,{},false)",
                    json!({"sectionData":{"recoms":{
                        "playlist":null,"playlists":[],"sectionId":"","next_from":""
                    }}})
                ),
                format!(
                    "<!--{}",
                    json!({"payload":["0",[null,{
                        "playlists":[{"list":[item(8),item(9),item(9)]}]
                    }]]})
                ),
                json!({"payload":[0,[[item(9)],false,[]]]}).to_string(),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.site = base.trim_end_matches('/').into();
        backend.mobile = backend.site.clone();
        backend.user = 123;
        backend.config = Some(json!({"cookies":"fixture"}));
        let tracks = backend.more(&["-2_8".into()]).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].id, "-2_9");
        let requests = server.join().unwrap();
        assert!(requests[1].contains("act=section"));
        assert!(requests[1].contains("section=recoms"));
        assert!(requests[1].contains("owner_id=123"));
        assert!(requests[2].contains("ids=-2_9_action_url"));
        assert!(
            requests
                .iter()
                .all(|r| !r.contains("chart") && !r.contains("new_songs"))
        );
    }

    #[test]
    fn repeated_empty_catalog_is_bounded_and_access_errors_keep_values_private() {
        let section = json!({"sectionId":"fixture-section","nextFrom":"", "next_from":"fixture-cursor", "playlist":null,"playlists":[]});
        let (base, server) = web::fixture_server(|_| {
            vec![
                format!(
                    "new AudioPage(0,{},false)",
                    json!({"sectionData":{"recoms":section.clone()}})
                ),
                format!("<!--{}", json!({"payload":[0,[null,section]]})),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.site = base.trim_end_matches('/').into();
        backend.user = 123;
        backend.config = Some(json!({"cookies":"fixture"}));
        assert!(
            backend
                .start()
                .unwrap_err()
                .to_string()
                .contains("no playable tracks")
        );
        let requests = server.join().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("start_from=fixture-cursor"));
        let error = catalog_data(r#"<!--{"payload":[3,["private-fixture-token"]]}"#)
            .unwrap_err()
            .to_string();
        assert!(error.contains("code 3"));
        assert!(!error.contains("private-fixture"));
        assert!(
            catalog_data("private-fixture-token")
                .unwrap_err()
                .to_string()
                .contains("Invalid VK AJAX response")
        );
    }

    #[test]
    fn response_diagnostics_show_catalog_structure_without_private_values() {
        let page = json!({"user_id":123456,"access_token":"private-fixture-token",
            "sectionData":{"recoms":{"blocks":[{"playlist":{"list":[
                {"id":987654,"url":"https://cdn.example/audio?secret=private-fixture-token","title":"Private fixture title"}
            ]},"sectionId":"private-fixture-section"}]}}});
        let (base, server) = web::fixture_server(|_| {
            vec![format!("<script>new AudioPage(0,{page},false)</script>")]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.site = base.trim_end_matches('/').into();
        backend.config = Some(json!({"cookies":"fixture"}));
        let report = backend.diagnose().unwrap();
        assert_eq!(report["legacy_playlist_present"], false);
        assert_eq!(
            report["section_layout"]["recoms"]["blocks"]["type"],
            "array"
        );
        let encoded = report.to_string();
        for private in [
            "private-fixture",
            "cdn.example",
            "Private fixture title",
            "123456",
            "987654",
        ] {
            assert!(
                !encoded.contains(private),
                "Diagnostic exposed a response value"
            );
        }
        assert!(server.join().unwrap()[0].contains("section=recoms"));
    }

    #[test]
    fn response_layout_has_bounded_depth_and_redacts_dynamic_keys() {
        let value = json!({"https://cdn.example/?secret=fixture":{"token":"fixture"},
            "nested":{"items":[{"token":"fixture"},{"id":123}]}});
        let mut budget = 16;
        let shape = response_shape(&value, 4, &mut budget);
        let output = shape.to_string();
        assert!(shape.get("<dynamic-field>").is_some());
        assert!(!output.contains("fixture"));
        assert!(!output.contains("cdn.example"));
        assert!(!output.contains("123"));
        assert_eq!(response_shape(&value, 0, &mut 0), json!("object"));
    }

    #[test]
    fn native_web_recommendations_reload_hashes_and_resolve_without_an_interpreter() {
        let encoded = "https://vk.example/audio_api_unavailable?extra=mZiXy2jHpxLLAZ8ZCgOUB2LKDweVzwXWBwf4zs5UzgmVlZPZChrOAa#DG";
        let item = json!([
            9,
            -2,
            encoded,
            "A &amp; B",
            "<b>Artist</b>",
            120,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            "one/two/action/four/five/url",
            "https://cdn.example/cover.jpg"
        ]);
        let (base, server) = web::fixture_server(|_| {
            vec![
                format!(
                    "<script>new AudioPage(0,{},false);</script>",
                    json!({"sectionData":{"recoms":{"playlist":{"list":[item.clone()]}}}})
                ),
                json!({"payload":[0,[[item.clone()],false,[]]]}).to_string(),
            ]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.site = base.trim_end_matches('/').into();
        backend.mobile = backend.site.clone();
        backend.user = 123;
        backend.config = Some(json!({"cookies":"fixture"}));
        let tracks = backend.start().unwrap();
        assert_eq!(tracks[0].title, "A & B");
        assert_eq!(tracks[0].artist, "Artist");
        match backend.resolve(&tracks[0]).unwrap() {
            Input::Http { url, .. } => assert_eq!(url, "https://cdn.example/audio.mp3?key=abc123"),
            _ => panic!("VK full MP3 did not use the native buffer"),
        }
        let requests = server.join().unwrap();
        assert!(requests[0].contains("section=recoms"));
        assert!(requests[1].contains("act=reload_audio"));
        assert!(requests[1].contains("ids=-2_9_action_url"));
    }

    #[test]
    fn native_token_api_keeps_entitlement_errors_private() {
        let (base, server) = web::fixture_server(|_| {
            vec![json!({"error":{"error_code":5,"error_msg":"private-token-details"}}).to_string()]
        });
        let mut backend = Vk::new(PathBuf::new());
        backend.api = base;
        backend.config = Some(json!({"token":"test-token"}));
        let error = backend.start().unwrap_err().to_string();
        assert!(error.contains("code 5"));
        assert!(!error.contains("private-token-details"));
        assert!(server.join().unwrap()[0].contains("audio.getRecommendations"));
    }

    #[test]
    fn audio_url_operations_match_independent_reference_vectors() {
        let vectors = [
            "https://vk.example/audio_api_unavailable?extra=mZiXy2jHpxLLAZ8ZCgOUB2LKDweVzwXWBwf4zs5UzgmVlZPZChrOAa#DG",
            "https://vk.example/audio_api_unavailable?extra=eG4ocGLavvuzhHruhWiBfW0wh1uBdX4tfvqxcKLfer8drXSyguTisq#EaT6",
            "https://vk.example/audio_api_unavailable?extra=BxL5DxG6zgr0AxmUAKnMCNvXAMrMEMLUDc5YDtG/CgPezwzNAdy3oa#CGS1",
            "https://vk.example/audio_api_unavailable?extra=mxbSoI50DhrLlI8VBNLJzxnTzdnJm2KYB2qVCgvHp2j1Bt14A2fWyq#CWS1",
            "https://vk.example/audio_api_unavailable?extra=yxrLEde/mJnWAgXZEwSVDgfUmY5JyY9ToNvIlMLKl3a9zwvWBw9Kyq#AqSXnW",
        ];
        for vector in vectors {
            assert_eq!(
                decode_url(vector, 123).unwrap(),
                "https://cdn.example/audio.mp3?key=abc123"
            );
        }
        assert!(decode_url("invalid", 123).is_err());
    }

    #[test]
    fn recommendations_hashes_and_html_metadata_are_decoded() {
        let item = json!([
            9,
            -2,
            "https://cdn.example/audio.mp3",
            "A &amp; B",
            "<b>Artist</b>",
            120,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            "one/two/action/four/five/url",
            "https://cdn.example/small.jpg,https://cdn.example/large.jpg"
        ]);
        let data = page_data(&format!(
            "<script>new AudioPage(0,{}, false);</script>",
            json!({"sectionData":{"recoms":{"playlist":{"list":[item.clone()]}}}})
        ))
        .unwrap();
        assert_eq!(
            audio_hash(&data["sectionData"]["recoms"]["playlist"]["list"][0]),
            Some(("-2_9".into(), "-2_9_action_url".into()))
        );
        let track = array_track(&item).unwrap();
        assert_eq!(track["title"], "A & B");
        assert_eq!(track["artist"], "Artist");
        assert_eq!(track_id(&track).as_deref(), Some("-2_9"));
        assert!(page_data("chart instead of recommendations").is_err());
    }
}
