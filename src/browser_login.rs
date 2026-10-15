use crate::{backend::web, config};
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{fs::PermissionsExt, process::CommandExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

const RPC_TIMEOUT: Duration = Duration::from_secs(15);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const LOGIN_TIMEOUT: Duration = Duration::from_secs(600);
const VK_SESSION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MESSAGE: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Source {
    SoundCloud,
    Vk,
}

impl Source {
    fn permits(self, domain: &str) -> bool {
        let domain = domain.trim_start_matches('.');
        match self {
            Self::SoundCloud => domain == "soundcloud.com" || domain.ends_with(".soundcloud.com"),
            Self::Vk => ["vk.ru", "vk.com"]
                .iter()
                .any(|d| domain == *d || domain.ends_with(&format!(".{d}"))),
        }
    }

    fn urls(self) -> Value {
        match self {
            Self::SoundCloud => {
                json!(["https://soundcloud.com/", "https://api-v2.soundcloud.com/"])
            }
            Self::Vk => json!([
                "https://vk.ru/",
                "https://vk.ru/audio",
                "https://m.vk.ru/audio",
                "https://login.vk.ru/",
                "https://web.api.vk.ru/",
                "https://vk.com/",
                "https://vk.com/audio",
                "https://m.vk.com/audio",
                "https://login.vk.com/",
                "https://web.api.vk.com/"
            ]),
        }
    }
}

struct TemporaryProfile(PathBuf);

impl TemporaryProfile {
    fn new() -> Result<Self> {
        let mut random = [0; 16];
        fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
        let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
        let path = env::temp_dir().join(format!("plavno-browser-{suffix}"));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        Ok(Self(path))
    }
}

impl Drop for TemporaryProfile {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

use std::os::unix::fs::DirBuilderExt;

fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    #[cfg(target_os = "linux")]
    let result = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
    #[cfg(not(target_os = "linux"))]
    let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    ensure!(result == 0, "Cannot create browser connection");
    let pair = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    #[cfg(not(target_os = "linux"))]
    for fd in [&pair.0, &pair.1] {
        ensure!(
            unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == 0,
            "Cannot protect browser connection descriptors"
        );
    }
    Ok(pair)
}

fn browser_program() -> Result<PathBuf> {
    if let Some(program) = env::var_os("PLAVNO_BROWSER").filter(|p| !p.is_empty()) {
        return Ok(program.into());
    }
    for name in [
        "google-chrome-stable",
        "google-chrome",
        "chromium",
        "chromium-browser",
    ] {
        for directory in env::split_paths(&env::var_os("PATH").unwrap_or_default()) {
            let path = directory.join(name);
            if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            {
                return Ok(path);
            }
        }
    }
    #[cfg(target_os = "macos")]
    for base in [
        Some(PathBuf::from("/Applications")),
        env::var_os("HOME").map(|home| PathBuf::from(home).join("Applications")),
    ]
    .into_iter()
    .flatten()
    {
        for executable in [
            "Google Chrome.app/Contents/MacOS/Google Chrome",
            "Chromium.app/Contents/MacOS/Chromium",
            "Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
        ] {
            let path = base.join(executable);
            if fs::metadata(&path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            {
                return Ok(path);
            }
        }
    }
    bail!("Install Chrome/Chromium or set PLAVNO_BROWSER to its executable")
}

#[derive(Default)]
struct Captured {
    token: Option<String>,
    client_id: Option<String>,
    vk_token: Option<String>,
}

impl Captured {
    fn observe(&mut self, event: &Value) {
        if event["method"] != "Network.requestWillBeSent" {
            return;
        }
        let request = &event["params"]["request"];
        let Some(address) = request["url"]
            .as_str()
            .and_then(|u| url::Url::parse(u).ok())
        else {
            return;
        };
        if address.scheme() != "https" {
            return;
        }
        if matches!(
            address.host_str(),
            Some("web.api.vk.ru" | "web.api.vk.com" | "api.vk.ru" | "api.vk.com")
        ) {
            let form = request["postData"].as_str().unwrap_or_default();
            let token = address
                .query_pairs()
                .chain(url::form_urlencoded::parse(form.as_bytes()))
                .find_map(|(key, value)| (key == "access_token").then(|| value.into_owned()))
                .or_else(|| {
                    request["headers"]
                        .as_object()?
                        .iter()
                        .find_map(|(key, value)| {
                            key.eq_ignore_ascii_case("authorization")
                                .then(|| value.as_str()?.strip_prefix("Bearer ").map(str::to_owned))
                                .flatten()
                        })
                });
            if let Some(token) =
                token.filter(|t| t.starts_with("vk1.") && !t.chars().any(char::is_control))
            {
                self.vk_token = Some(token);
            }
            return;
        }
        if address.host_str() != Some("api-v2.soundcloud.com") {
            return;
        }
        for (key, value) in address.query_pairs() {
            if key == "client_id"
                && value.len() == 32
                && value.bytes().all(|c| c.is_ascii_alphanumeric())
            {
                self.client_id = Some(value.into_owned());
            }
        }
        if let Some(headers) = request["headers"].as_object() {
            for (key, value) in headers {
                if key.eq_ignore_ascii_case("authorization")
                    && let Some(token) = value.as_str().and_then(|v| v.strip_prefix("OAuth "))
                    && !token.is_empty()
                    && !token.chars().any(char::is_control)
                {
                    self.token = Some(token.into());
                }
            }
        }
    }
}

struct Browser {
    child: Child,
    input: fs::File,
    output: fs::File,
    pending: Vec<u8>,
    sequence: u64,
    captured: Captured,
    cancelled: Arc<AtomicBool>,
    _profile: TemporaryProfile,
}

impl Browser {
    fn launch(program: &Path, headless: bool, cancelled: Arc<AtomicBool>) -> Result<Self> {
        let profile = TemporaryProfile::new()?;
        let (read_input, write_input) = pipe()?;
        let (read_output, write_output) = pipe()?;
        let read_fd = read_input.as_raw_fd();
        let write_fd = write_output.as_raw_fd();
        let mut command = Command::new(program);
        command
            .args([
                "--remote-debugging-pipe",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-sync",
            ])
            .arg(format!("--user-data-dir={}", profile.0.display()))
            .arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        if headless {
            command.arg("--headless=new");
        } else if env::var_os("DISPLAY").is_none() && env::var_os("WAYLAND_DISPLAY").is_some() {
            command.arg("--ozone-platform=wayland");
        }
        unsafe {
            command.pre_exec(move || {
                let read_copy = libc::fcntl(read_fd, libc::F_DUPFD_CLOEXEC, 5);
                if read_copy < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let write_copy = libc::fcntl(write_fd, libc::F_DUPFD_CLOEXEC, 5);
                if write_copy < 0 {
                    libc::close(read_copy);
                    return Err(std::io::Error::last_os_error());
                }
                let result = if libc::dup2(read_copy, 3) < 0 || libc::dup2(write_copy, 4) < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                };
                libc::close(read_copy);
                libc::close(write_copy);
                result
            });
        }
        let child = command
            .spawn()
            .context("Cannot start Chrome/Chromium; check PLAVNO_BROWSER")?;
        drop(read_input);
        drop(write_output);
        Ok(Self {
            child,
            input: write_input.into(),
            output: read_output.into(),
            pending: Vec::new(),
            sequence: 0,
            captured: Captured::default(),
            cancelled,
            _profile: profile,
        })
    }

    fn send(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<u64> {
        self.sequence += 1;
        let mut message = json!({"id":self.sequence,"method":method,"params":params});
        if let Some(session) = session {
            message["sessionId"] = session.into()
        }
        serde_json::to_writer(&mut self.input, &message)
            .map_err(|_| anyhow::anyhow!("Browser connection closed"))?;
        self.input
            .write_all(&[0])
            .map_err(|_| anyhow::anyhow!("Browser connection closed"))?;
        Ok(self.sequence)
    }

    fn receive(&mut self, deadline: Instant) -> Result<Option<Value>> {
        loop {
            ensure!(
                !self.cancelled.load(Ordering::Relaxed),
                "Browser login cancelled"
            );
            if let Some(end) = self.pending.iter().position(|b| *b == 0) {
                let message = serde_json::from_slice(&self.pending[..end])
                    .map_err(|_| anyhow::anyhow!("Invalid browser response"))?;
                self.pending.drain(..=end);
                self.captured.observe(&message);
                return Ok(Some(message));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            ensure!(
                self.child.try_wait()?.is_none(),
                "Browser closed before login completed"
            );
            let mut poll = libc::pollfd {
                fd: self.output.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let wait = deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(100) as i32;
            let ready = unsafe { libc::poll(&mut poll, 1, wait) };
            if ready < 0 {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                bail!("Browser connection failed");
            }
            if ready > 0 {
                let mut bytes = [0; 16384];
                let size = self
                    .output
                    .read(&mut bytes)
                    .context("Cannot read browser response")?;
                ensure!(size > 0, "Browser connection closed");
                ensure!(
                    self.pending.len() + size <= MAX_MESSAGE,
                    "Browser response exceeded the size limit"
                );
                self.pending.extend_from_slice(&bytes[..size]);
            }
        }
    }

    fn call(&mut self, method: &str, params: Value, session: Option<&str>) -> Result<Value> {
        let id = self.send(method, params, session)?;
        let deadline = Instant::now()
            + if id == 1 {
                STARTUP_TIMEOUT
            } else {
                RPC_TIMEOUT
            };
        while let Some(message) = self.receive(deadline)? {
            if message["id"].as_u64() == Some(id) {
                ensure!(
                    message.get("error").is_none(),
                    "Browser command failed: {method}"
                );
                return message
                    .get("result")
                    .cloned()
                    .context("Missing browser result");
            }
        }
        bail!(
            "Browser did not respond to {method}; check the graphical session and Chrome/Chromium installation"
        )
    }

    fn page(&mut self, url: &str) -> Result<String> {
        let target = self.call("Target.createTarget", json!({"url":"about:blank"}), None)?;
        let attached = self.call(
            "Target.attachToTarget",
            json!({"targetId":target["targetId"],"flatten":true}),
            None,
        )?;
        let session = attached["sessionId"]
            .as_str()
            .context("Missing browser session")?
            .to_owned();
        self.call("Network.enable", json!({}), Some(&session))?;
        self.call("Page.navigate", json!({"url":url}), Some(&session))?;
        Ok(session)
    }

    fn page_info(&mut self, session: &str) -> Result<Value> {
        let result = self.call("Runtime.evaluate", json!({"expression":"({url:location.href,userId:Number(window.vk?.id || window.vk?.user_id || 0)})","returnByValue":true}), Some(session))?;
        Ok(result["result"]["value"].clone())
    }

    fn cookies(&mut self, source: Source, session: &str) -> Result<String> {
        let response = self.call(
            "Network.getCookies",
            json!({"urls":source.urls()}),
            Some(session),
        )?;
        cookie_file(source, &response["cookies"])
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.send("Browser.close", json!({}), None);
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}

struct Interrupts(Vec<(signal_hook::SigId, i32, libc::sigaction)>);
impl Drop for Interrupts {
    fn drop(&mut self) {
        for (id, signal, previous) in self.0.drain(..) {
            signal_hook::low_level::unregister(id);
            unsafe {
                libc::sigaction(signal, &previous, std::ptr::null_mut());
            }
        }
    }
}

fn cookie_file(source: Source, values: &Value) -> Result<String> {
    let mut output = String::from("# Netscape HTTP Cookie File\n# Saved by plavno browser login\n");
    for cookie in values.as_array().context("Missing browser cookies")? {
        let Some(domain) = cookie["domain"].as_str().filter(|d| source.permits(d)) else {
            continue;
        };
        let name = cookie["name"].as_str().context("Missing cookie name")?;
        let value = cookie["value"].as_str().context("Missing cookie value")?;
        let path = cookie["path"].as_str().unwrap_or("/");
        if [domain, path, name, value]
            .iter()
            .any(|s| s.contains(['\t', '\r', '\n', '\0']))
            || name.contains(';')
            || value.contains(';')
        {
            continue;
        }
        let prefix = if cookie["httpOnly"].as_bool() == Some(true) {
            "#HttpOnly_"
        } else {
            ""
        };
        let subdomains = if domain.starts_with('.') {
            "TRUE"
        } else {
            "FALSE"
        };
        let secure = if cookie["secure"].as_bool() == Some(true) {
            "TRUE"
        } else {
            "FALSE"
        };
        let expiration = cookie["expires"]
            .as_f64()
            .filter(|d| d.is_finite() && *d > 0.0)
            .map_or(0, |d| d as u64);
        output.push_str(&format!(
            "{prefix}{domain}\t{subdomains}\t{path}\t{secure}\t{expiration}\t{name}\t{value}\n"
        ));
    }
    Ok(output)
}

fn station_url(value: &str) -> Option<String> {
    let mut url = url::Url::parse(value.trim()).ok()?;
    if url.scheme() != "https"
        || !matches!(
            url.host_str(),
            Some("soundcloud.com" | "www.soundcloud.com")
        )
    {
        return None;
    }
    let parts: Vec<_> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
    if parts.len() != 2
        || matches!(
            parts[0],
            "you" | "discover" | "settings" | "stations" | "search"
        )
        || matches!(parts[1], "likes" | "sets" | "tracks" | "albums" | "reposts")
    {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(8)))
        .max_redirects(0)
        .build()
        .into()
}

fn soundcloud_account(
    http: &ureq::Agent,
    captured: &Captured,
    cookies: &web::Cookies,
    user_agent: &str,
) -> Option<u64> {
    let token = captured.token.as_deref()?;
    let client = captured.client_id.as_deref()?;
    let mut response = http
        .get("https://api-v2.soundcloud.com/me")
        .query("client_id", client)
        .header("Authorization", format!("OAuth {token}"))
        .header("User-Agent", user_agent)
        .header(
            "Cookie",
            cookies.header("https://api-v2.soundcloud.com/me").ok()?,
        )
        .call()
        .ok()?;
    let value: Value = response.body_mut().read_json().ok()?;
    value["id"].as_u64().filter(|id| *id > 0)
}

fn liked_station(
    http: &ureq::Agent,
    captured: &Captured,
    id: u64,
    user_agent: &str,
) -> Option<String> {
    let mut response = http
        .get(format!("https://api-v2.soundcloud.com/users/{id}/likes"))
        .query("limit", "1")
        .query("client_id", captured.client_id.as_deref()?)
        .header(
            "Authorization",
            format!("OAuth {}", captured.token.as_deref()?),
        )
        .header("User-Agent", user_agent)
        .call()
        .ok()?;
    let data: Value = response.body_mut().read_json().ok()?;
    station_url(data["collection"][0]["track"]["permalink_url"].as_str()?)
}

fn vk_account(cookies: &web::Cookies, page: &Value) -> Option<(i64, String)> {
    let address = url::Url::parse(page["url"].as_str()?).ok()?;
    if address.scheme() != "https" {
        return None;
    }
    let host = address.host_str()?;
    let site = match host {
        "vk.ru" | "m.vk.ru" => "https://vk.ru",
        "vk.com" | "m.vk.com" => "https://vk.com",
        _ => return None,
    };
    let header = cookies.header(&format!("{site}/audio")).ok()?;
    if !header.split("; ").any(|part| {
        part.split_once('=').is_some_and(|(name, value)| {
            matches!(name, "remixsid" | "remixsid6") && !value.is_empty()
        })
    }) {
        return None;
    }
    let id = page["userId"].as_i64().filter(|id| *id > 0).or_else(|| {
        header
            .split("; ")
            .find_map(|p| p.strip_prefix("remixuserid=")?.parse::<i64>().ok())
            .filter(|id| *id > 0)
    })?;
    Some((id, site.into()))
}

fn connect(source: Source, existing: &Value) -> Result<(Value, String)> {
    #[cfg(target_os = "linux")]
    ensure!(
        env::var_os("DISPLAY").is_some() || env::var_os("WAYLAND_DISPLAY").is_some(),
        "Browser login needs a graphical session; run it from your desktop terminal"
    );
    let cancelled = Arc::new(AtomicBool::new(false));
    let mut interrupts = Interrupts(Vec::new());
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        let mut previous = unsafe { std::mem::zeroed() };
        ensure!(
            unsafe { libc::sigaction(signal, std::ptr::null(), &mut previous) } == 0,
            "Cannot handle login cancellation"
        );
        let id = signal_hook::flag::register(signal, cancelled.clone())?;
        interrupts.0.push((id, signal, previous));
    }
    let program = browser_program()?;
    println!(
        "Opening {} with a temporary plavno profile.",
        program.display()
    );
    println!("Sign in in the browser. Waiting for your session · Ctrl+C cancels.");
    let mut browser = Browser::launch(&program, false, cancelled)?;
    let version = browser.call("Browser.getVersion", json!({}), None)?;
    let user_agent = version["userAgent"]
        .as_str()
        .context("Missing browser user agent")?
        .to_owned();
    let session = browser.page(match source {
        Source::SoundCloud => "https://soundcloud.com/signin",
        Source::Vk => "https://vk.ru/audio",
    })?;
    let http = agent();
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    let mut verifying = false;
    let mut vk_session_deadline = None;
    while Instant::now() < deadline {
        let drain = Instant::now() + Duration::from_millis(200);
        while browser.receive(drain)?.is_some() {}
        let page = browser.page_info(&session)?;
        let contents = browser.cookies(source, &session)?;
        let cookies = web::Cookies::parse(&contents)?;
        match source {
            Source::SoundCloud => {
                if browser.captured.token.is_none() {
                    browser.captured.token = cookies
                        .header("https://soundcloud.com/")?
                        .split("; ")
                        .find_map(|p| p.strip_prefix("oauth_token=").map(str::to_owned));
                }
                if !verifying
                    && browser.captured.token.is_some()
                    && browser.captured.client_id.is_some()
                {
                    println!("Session detected; verifying SoundCloud account access.");
                    verifying = true;
                }
                if let Some(id) =
                    soundcloud_account(&http, &browser.captured, &cookies, &user_agent)
                {
                    let seed = existing["seed"]
                        .as_str()
                        .and_then(station_url)
                        .or_else(|| page["url"].as_str().and_then(station_url))
                        .or_else(|| liked_station(&http, &browser.captured, id, &user_agent));
                    println!("Signed in to SoundCloud (account {id}).");
                    return Ok((
                        json!({"token":browser.captured.token,"client_id":browser.captured.client_id,"user_agent":user_agent,"seed":seed}),
                        contents,
                    ));
                }
            }
            Source::Vk => {
                if !verifying && page["userId"].as_i64().is_some_and(|id| id > 0) {
                    println!("Sign-in detected; capturing VK browser session.");
                    verifying = true;
                    vk_session_deadline = Some(Instant::now() + VK_SESSION_TIMEOUT);
                }
                if let Some((id, site)) = vk_account(&cookies, &page) {
                    println!("Signed in to VK Music (account {id}); browser session captured.");
                    let mut profile = json!({"user_id":id,"site":site,"user_agent":user_agent});
                    if let Some(token) = &browser.captured.vk_token {
                        profile["token"] = token.clone().into();
                    }
                    return Ok((profile, contents));
                }
                ensure!(
                    vk_session_deadline.is_none_or(|deadline| Instant::now() < deadline),
                    "VK sign-in was detected, but no valid session cookie was available within 30 seconds; open your VK page in the login window and retry"
                );
            }
        }
        let pause = Instant::now() + Duration::from_secs(2);
        while browser.receive(pause)?.is_some() {}
    }
    bail!("Browser login timed out; sign-in and service access were not confirmed")
}

pub fn login(source: &str) -> Result<()> {
    let kind = match source {
        "soundcloud" => Source::SoundCloud,
        "vk" => Source::Vk,
        _ => bail!("Browser login supports SoundCloud and VK Music"),
    };
    let path = config::profile_path(source)?;
    let existing = web::profile(&path).unwrap_or(Value::Null);
    let (mut profile, cookies) = connect(kind, &existing)?;
    if kind == Source::SoundCloud && profile["seed"].as_str().is_none() {
        loop {
            print!("Track URL for your SoundCloud station: ");
            std::io::stdout().flush()?;
            let mut value = String::new();
            ensure!(
                std::io::stdin().read_line(&mut value)? > 0,
                "No station track chosen; connection was not saved"
            );
            if let Some(seed) = station_url(&value) {
                profile["seed"] = seed.into();
                break;
            }
            println!(
                "Choose a track URL such as https://soundcloud.com/artist/track · Ctrl+C cancels."
            );
        }
    }
    let mut random = [0; 8];
    fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    let suffix: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let cookie_path = path.with_file_name(format!("{source}.cookies-{suffix}.txt"));
    config::save_private(&cookie_path, cookies.as_bytes())?;
    profile["auth"] = "browser".into();
    profile["cookies"] = cookie_path.to_string_lossy().as_ref().into();
    if let Err(error) = config::save_private(&path, &serde_json::to_vec(&profile)?) {
        let _ = fs::remove_file(&cookie_path);
        return Err(error);
    }
    if let Some(old) = existing["cookies"].as_str().map(Path::new)
        && existing["auth"] == "browser"
        && old.parent() == path.parent()
        && old
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with(&format!("{source}.cookies-")) && s.ends_with(".txt"))
    {
        let _ = fs::remove_file(old);
    }
    println!("Connected. Start playback with: plavno --source {source}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browser_pipe_is_closed_on_exec_and_transports_bytes() {
        let (read, write) = pipe().unwrap();
        for fd in [&read, &write] {
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        let mut read = fs::File::from(read);
        fs::File::from(write).write_all(b"fixture").unwrap();
        let mut bytes = Vec::new();
        read.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"fixture");
    }

    #[test]
    fn vk_browser_tokens_are_captured_only_from_trusted_https_api_requests() {
        let event = |url, body| json!({"method":"Network.requestWillBeSent","params":{"request":{"url":url,"postData":body}}});
        let mut captured = Captured::default();
        for url in [
            "http://web.api.vk.ru/method/audio.get",
            "https://web.api.vk.ru.evil.example/method/audio.get",
            "https://vk.ru.example/method/audio.get",
        ] {
            captured.observe(&event(url, "access_token=vk1.a.fixture-secret"));
        }
        assert!(captured.vk_token.is_none());
        captured.observe(&event(
            "https://web.api.vk.ru/method/audio.get",
            "access_token=vk1.a.fixture-secret",
        ));
        assert_eq!(captured.vk_token.as_deref(), Some("vk1.a.fixture-secret"));
        assert!(captured.token.is_none());
    }

    #[test]
    fn authenticated_vk_browser_session_does_not_require_legacy_audio_html() {
        let cookies =
            web::Cookies::parse("#HttpOnly_.vk.ru\tTRUE\t/\tTRUE\t0\tremixsid6\tfixture-session\n")
                .unwrap();
        let page = json!({"url":"https://vk.ru/feed","userId":123});
        assert_eq!(
            vk_account(&cookies, &page),
            Some((123, "https://vk.ru".into()))
        );
    }

    #[test]
    fn vk_login_requires_an_authenticated_identity_and_session_on_the_active_origin() {
        let cookies = web::Cookies::parse(".vk.ru\tTRUE\t/\tTRUE\t0\tremixsid\tfixture\n.vk.com\tTRUE\t/audio\tTRUE\t0\tremixsid6\tlegacy-fixture\n").unwrap();
        assert_eq!(
            vk_account(
                &cookies,
                &json!({"url":"https://m.vk.com/audio","userId":123})
            ),
            Some((123, "https://vk.com".into()))
        );
        assert!(vk_account(&cookies, &json!({"url":"https://vk.ru/feed","userId":0})).is_none());
        assert!(
            vk_account(
                &cookies,
                &json!({"url":"https://vk.ru.evil.example/feed","userId":123})
            )
            .is_none()
        );
        assert!(vk_account(&cookies, &json!({"url":"http://vk.ru/feed","userId":123})).is_none());
        let guest = web::Cookies::parse(".vk.ru\tTRUE\t/\tTRUE\t0\tremixlang\ten\n").unwrap();
        assert!(vk_account(&guest, &json!({"url":"https://vk.ru/feed","userId":123})).is_none());
        let expired = web::Cookies::parse(".vk.ru\tTRUE\t/\tTRUE\t1\tremixsid\tfixture\n").unwrap();
        assert!(vk_account(&expired, &json!({"url":"https://vk.ru/feed","userId":123})).is_none());
    }

    #[test]
    fn session_export_keeps_only_service_cookies_and_preserves_http_only_scope() {
        let values = json!([
            {"domain":".vk.ru","path":"/","name":"remixsid","value":"fixture","secure":true,"httpOnly":true,"expires":-1},
            {"domain":"m.vk.ru","path":"/audio","name":"scoped","value":"value","secure":true,"expires":-1},
            {"domain":".vk.com","path":"/","name":"legacy","value":"old","expires":-1},
            {"domain":"evilvk.ru","name":"evil","value":"excluded"},
            {"domain":".google.com","name":"foreign","value":"excluded"},
            {"domain":".vk.ru","name":"injected","value":"bad\nvalue"},
            {"domain":"soundcloud.com","name":"oauth_token","value":"sc-fixture","expires":-1}
        ]);
        let exported = cookie_file(Source::Vk, &values).unwrap();
        assert!(exported.contains("#HttpOnly_.vk.ru\tTRUE\t/\tTRUE\t0"));
        assert!(!exported.contains("excluded"));
        assert!(!exported.contains("injected"));
        let cookies = web::Cookies::parse(&exported).unwrap();
        assert_eq!(
            cookies.header("https://m.vk.ru/audio").unwrap(),
            "remixsid=fixture; scoped=value"
        );
        assert_eq!(cookies.header("https://vk.com/").unwrap(), "legacy=old");
        assert_eq!(cookies.header("https://soundcloud.com/").unwrap(), "");
        let sc = web::Cookies::parse(&cookie_file(Source::SoundCloud, &values).unwrap()).unwrap();
        assert_eq!(
            sc.header("https://soundcloud.com/").unwrap(),
            "oauth_token=sc-fixture"
        );
    }

    #[test]
    fn soundcloud_credentials_are_captured_only_from_the_https_api_origin() {
        let event = |host: &str| json!({"method":"Network.requestWillBeSent","params":{"request":{"url":format!("{host}/me?client_id=0123456789abcdefghijklmnopqrstuv"),"headers":{"authorization":"OAuth fixture-token"}}}});
        let mut captured = Captured::default();
        captured.observe(&event("https://api-v2.soundcloud.com.evil.example"));
        captured.observe(&event("http://api-v2.soundcloud.com"));
        assert!(captured.token.is_none());
        captured.observe(&event("https://api-v2.soundcloud.com"));
        assert_eq!(captured.token.as_deref(), Some("fixture-token"));
        assert_eq!(
            captured.client_id.as_deref(),
            Some("0123456789abcdefghijklmnopqrstuv")
        );
    }

    #[test]
    fn station_selection_accepts_tracks_and_rejects_profile_and_hub_urls() {
        assert_eq!(
            station_url("https://soundcloud.com/artist/track?utm_source=browser#comments")
                .as_deref(),
            Some("https://soundcloud.com/artist/track")
        );
        for value in [
            "https://soundcloud.com/artist",
            "https://soundcloud.com/artist/likes",
            "https://soundcloud.com/you/likes",
            "https://soundcloud.com.evil.example/artist/track",
            "http://soundcloud.com/artist/track",
        ] {
            assert!(station_url(value).is_none());
        }
    }

    #[test]
    #[ignore = "Requires installed Chrome/Chromium; uses a temporary headless profile"]
    fn real_chrome_pipe_supports_http_only_cookies_cancellation_and_cleanup() {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut browser =
            Browser::launch(&browser_program().unwrap(), true, cancelled.clone()).unwrap();
        let directory = browser._profile.0.clone();
        let version = browser.call("Browser.getVersion", json!({}), None).unwrap();
        assert!(version["product"].as_str().unwrap().contains("Chrome"));
        let session = browser.page("about:blank").unwrap();
        browser.call("Network.setCookies", json!({"cookies":[
            {"name":"remixsid","value":"dummy-session","domain":".vk.ru","path":"/","secure":true,"httpOnly":true},
            {"name":"login_session","value":"dummy-login-session","domain":"login.vk.ru","path":"/","secure":true,"httpOnly":true},
            {"name":"foreign","value":"dummy-foreign","domain":".example.com","path":"/"}
        ]}), Some(&session)).unwrap();
        let contents = browser.cookies(Source::Vk, &session).unwrap();
        assert!(contents.contains("dummy-session"));
        assert!(
            contents.contains("login.vk.ru\tFALSE\t/\tTRUE\t0\tlogin_session\tdummy-login-session")
        );
        assert!(contents.contains("#HttpOnly_"));
        assert!(!contents.contains("dummy-foreign"));
        assert_eq!(browser.page_info(&session).unwrap()["url"], "about:blank");
        cancelled.store(true, Ordering::Relaxed);
        let error = browser
            .call("Browser.getVersion", json!({}), None)
            .unwrap_err()
            .to_string();
        assert_eq!(error, "Browser login cancelled");
        let pid = browser.child.id();
        drop(browser);
        assert!(!directory.exists());
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }
}
