use super::Input;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

pub const USER_AGENT: &str =
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 Chrome/131.0.0.0 Safari/537.36";

#[cfg(test)]
pub fn fixture_server(
    responses: impl FnOnce(&str) -> Vec<String>,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    fixture_server_with_headers(|base| responses(base).into_iter().map(|body| (body, "")).collect())
}

#[cfg(test)]
pub fn fixture_server_with_headers(
    responses: impl FnOnce(&str) -> Vec<(String, &'static str)>,
) -> (String, std::thread::JoinHandle<Vec<String>>) {
    use std::{
        io::{BufRead, BufReader, Read, Write},
        net::TcpListener,
        time::Duration,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/", listener.local_addr().unwrap());
    let responses = responses(&base);
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for (response, extra_headers) in responses {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let mut header = String::new();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if let Some(size) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = size.trim().parse().unwrap();
                }
                header.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            header.push_str(&String::from_utf8(body).unwrap());
            requests.push(header);
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{extra_headers}\r\n{response}",response.len()).unwrap();
        }
        requests
    });
    (base, worker)
}

pub fn private(path: &Path) -> Result<String> {
    ensure!(
        plavno_platform::private_file(path).context("Missing private connection file")?,
        "Connection files must be private regular files"
    );
    fs::read_to_string(path).context("Cannot read connection file")
}

pub fn profile(path: &Path) -> Result<Value> {
    serde_json::from_str(&private(path)?).map_err(|_| anyhow::anyhow!("Invalid connection profile"))
}

pub fn user_agent(profile: &Value) -> Result<String> {
    let value = profile["user_agent"].as_str().unwrap_or(USER_AGENT);
    ensure!(
        !value.is_empty()
            && value.len() <= 1024
            && value.is_ascii()
            && !value.chars().any(char::is_control),
        "Invalid browser user agent"
    );
    Ok(value.into())
}

pub fn text(value: &str) -> String {
    let stripped = regex_lite::Regex::new("<[^>]*>")
        .unwrap()
        .replace_all(value, "");
    html_escape::decode_html_entities(&stripped).into_owned()
}

pub fn transcode(url: String, quality: String) -> Result<Input> {
    transcode_request(url, quality, false, Vec::new())
}

pub fn transcode_http(
    url: String,
    quality: String,
    headers: Vec<(String, String)>,
) -> Result<Input> {
    transcode_request(url, quality, true, headers)
}

fn transcode_request(
    url: String,
    quality: String,
    direct: bool,
    headers: Vec<(String, String)>,
) -> Result<Input> {
    ensure!(url.starts_with("https://"), "Insecure audio stream URL");
    Ok(Input::Pipe {
        program: std::env::current_exe()?,
        args: vec!["--stream-audio".into()],
        request: json!({"url":url,"direct":direct,"headers":headers}),
        quality: format!("{quality} · FLAC transport"),
    })
}

pub fn youtube_fallbacks(address: &str) -> Vec<String> {
    let Ok(url) = url::Url::parse(address) else {
        return vec![];
    };
    let host =
        regex_lite::Regex::new(r"^(rr?)([0-9]+)---(sn-[a-zA-Z0-9-]+)\.googlevideo\.com$").unwrap();
    let Some(parts) = url
        .host_str()
        .and_then(|host_name| host.captures(host_name))
    else {
        return vec![];
    };
    if url.scheme() != "https" || url.port().is_some() || url.path() != "/videoplayback" {
        return vec![];
    }
    let parameters: std::collections::HashMap<_, _> = url.query_pairs().collect();
    let Some(nodes) = parameters.get("mn") else {
        return vec![];
    };
    let server = parameters
        .get("fvip")
        .map_or(&parts[2], |value| value.as_ref());
    if server.is_empty() || server.len() > 3 || !server.bytes().all(|b| b.is_ascii_digit()) {
        return vec![];
    }
    let node = regex_lite::Regex::new(r"^sn-[a-zA-Z0-9-]+$").unwrap();
    let mut fallbacks = Vec::new();
    for candidate in nodes
        .split(',')
        .filter(|candidate| *candidate != &parts[3] && node.is_match(candidate))
    {
        let mut fallback = url.clone();
        if fallback
            .set_host(Some(&format!(
                "{}{server}---{candidate}.googlevideo.com",
                &parts[1]
            )))
            .is_ok()
        {
            let fallback = fallback.to_string();
            if !fallbacks.contains(&fallback) {
                fallbacks.push(fallback);
                if fallbacks.len() == 2 {
                    break;
                }
            }
        }
    }
    fallbacks
}

#[derive(Default)]
pub struct Cookies(Vec<Cookie>);

struct Cookie {
    domain: String,
    subdomains: bool,
    path: String,
    secure: bool,
    expires: u64,
    name: String,
    value: String,
}

impl Cookies {
    pub fn netscape(&self) -> String {
        let mut data = String::from("# Netscape HTTP Cookie File\n");
        for cookie in &self.0 {
            data.push_str(&format!(
                "{}{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                if cookie.subdomains { "." } else { "" },
                cookie.domain,
                if cookie.subdomains { "TRUE" } else { "FALSE" },
                cookie.path,
                if cookie.secure { "TRUE" } else { "FALSE" },
                if cookie.expires == 0 {
                    String::new()
                } else {
                    cookie.expires.to_string()
                },
                cookie.name,
                cookie.value
            ));
        }
        data
    }
    pub fn from_header(header: &str, site: &str) -> Result<Self> {
        ensure!(
            header.is_ascii() && !header.chars().any(char::is_control),
            "Invalid cookie header"
        );
        let url = url::Url::parse(site).context("Invalid cookie site")?;
        ensure!(url.scheme() == "https", "Cookie site must use HTTPS");
        let domain = url.host_str().context("Missing cookie domain")?;
        let mut cookies = Vec::new();
        for part in header.split(';').map(str::trim).filter(|s| !s.is_empty()) {
            let (name, value) = part.split_once('=').context("Invalid cookie header")?;
            ensure!(
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)),
                "Invalid cookie header"
            );
            cookies.push(Cookie {
                domain: domain.into(),
                subdomains: true,
                path: "/".into(),
                secure: true,
                expires: 0,
                name: name.into(),
                value: value.into(),
            });
        }
        ensure!(!cookies.is_empty(), "Cookie header is empty");
        Ok(Self(cookies))
    }

    pub fn read(path: &Path) -> Result<Self> {
        Self::parse(&private(path)?)
    }

    pub(crate) fn parse(contents: &str) -> Result<Self> {
        let mut cookies = Vec::new();
        for line in contents.lines() {
            let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
            if line.starts_with('#') || line.trim().is_empty() {
                continue;
            }
            let fields: Vec<_> = line.splitn(7, '\t').collect();
            ensure!(fields.len() == 7, "Invalid Netscape cookie file");
            ensure!(
                !fields[5].is_empty()
                    && !fields[5].contains([';', '\r', '\n'])
                    && !fields[6].contains([';', '\r', '\n']),
                "Invalid cookie value"
            );
            cookies.push(Cookie {
                domain: fields[0].trim_start_matches('.').to_ascii_lowercase(),
                subdomains: fields[1] == "TRUE",
                path: fields[2].into(),
                secure: fields[3] == "TRUE",
                expires: if fields[4].is_empty() {
                    0
                } else {
                    fields[4].parse().context("Invalid cookie expiration")?
                },
                name: fields[5].into(),
                value: fields[6].into(),
            });
        }
        Ok(Self(cookies))
    }

    pub fn header(&self, address: &str) -> Result<String> {
        let url = url::Url::parse(address).context("Invalid request URL")?;
        let host = url.host_str().context("Missing request host")?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        Ok(self
            .0
            .iter()
            .filter(|cookie| {
                (host == cookie.domain
                    || cookie.subdomains && host.ends_with(&format!(".{}", cookie.domain)))
                    && (!cookie.secure || url.scheme() == "https")
                    && (cookie.expires == 0 || cookie.expires > now)
                    && (url.path() == cookie.path
                        || url.path().strip_prefix(&cookie.path).is_some_and(|tail| {
                            cookie.path.ends_with('/') || tail.starts_with('/')
                        }))
            })
            .map(|c| format!("{}={}", c.name, c.value))
            .collect::<Vec<_>>()
            .join("; "))
    }

    pub fn seed_agent(&self, agent: &ureq::Agent) -> Result<()> {
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let mut jar = agent.cookie_jar_lock();
        for cookie in &self.0 {
            if cookie.expires != 0 && cookie.expires <= now {
                continue;
            }
            let uri: ureq::http::Uri = format!("https://{}{}", cookie.domain, cookie.path)
                .parse()
                .map_err(|_| anyhow::anyhow!("Invalid cookie origin"))?;
            let mut value = format!("{}={}; Path={}", cookie.name, cookie.value, cookie.path);
            if cookie.subdomains {
                value.push_str(&format!("; Domain={}", cookie.domain));
            }
            if cookie.secure {
                value.push_str("; Secure");
            }
            if cookie.expires > now {
                value.push_str(&format!("; Max-Age={}", cookie.expires - now));
            }
            if jar
                .get(&cookie.domain, &cookie.path, &cookie.name)
                .is_some()
            {
                continue;
            }
            let parsed = ureq::Cookie::parse(value, &uri)
                .map_err(|_| anyhow::anyhow!("Invalid session cookie"))?;
            jar.insert(parsed, &uri)
                .map_err(|_| anyhow::anyhow!("Invalid session cookie scope"))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn netscape_export_keeps_domain_and_session_cookie_semantics() {
        let cookies =
            Cookies::from_header("SID=fixture-secret; pref=value", "https://www.youtube.com/")
                .unwrap();
        let serialized = cookies.netscape();
        assert!(serialized.contains(".www.youtube.com\tTRUE\t/\tTRUE\t\tSID\tfixture-secret"));
        let imported = Cookies::parse(&serialized).unwrap();
        assert_eq!(
            imported.header("https://www.youtube.com/").unwrap(),
            "SID=fixture-secret; pref=value"
        );
        assert!(imported.header("https://other.test/").unwrap().is_empty());
    }

    #[test]
    fn youtube_backups_use_signed_routing_nodes_without_changing_the_query() {
        let url = "https://rr4---sn-primary.googlevideo.com/videoplayback?mn=sn-primary,sn-backup,sn-backup,sn-third,sn-fourth&fvip=2&sig=secret%2Bsignature&lsig=private&range=0-1023";
        let backups = youtube_fallbacks(url);
        assert_eq!(backups.len(), 2);
        let original = url::Url::parse(url).unwrap();
        for (backup, host) in backups.iter().zip([
            "rr2---sn-backup.googlevideo.com",
            "rr2---sn-third.googlevideo.com",
        ]) {
            let parsed = url::Url::parse(backup).unwrap();
            assert_eq!(parsed.host_str(), Some(host));
            assert_eq!(parsed.query(), original.query());
            assert_eq!(parsed.path(), original.path());
        }
        for invalid in [
            "https://rr4---sn-primary.googlevideo.com.evil.test/videoplayback?mn=sn-primary,sn-backup",
            "https://example.com/videoplayback?mn=sn-primary,sn-backup",
            "http://rr4---sn-primary.googlevideo.com/videoplayback?mn=sn-primary,sn-backup",
            "https://rr4---sn-primary.googlevideo.com/other?mn=sn-primary,sn-backup",
            "https://rr4---sn-primary.googlevideo.com/videoplayback?mn=sn-primary,sn-backup.evil.test",
            "https://rr4---sn-primary.googlevideo.com/videoplayback?mn=sn-primary,sn-backup&fvip=bad",
            "https://rr4---sn-primary.googlevideo.com/videoplayback?mn=sn-primary,sn-backup%2Fbad",
        ] {
            assert!(youtube_fallbacks(invalid).is_empty());
        }
    }

    #[test]
    fn manual_cookie_header_is_scoped_and_rejects_header_injection() {
        let cookies = Cookies::from_header(
            "remixsid6=fixture; remixuserid=123; value=a=b",
            "https://vk.ru",
        )
        .unwrap();
        assert_eq!(
            cookies.header("https://m.vk.ru/audio").unwrap(),
            "remixsid6=fixture; remixuserid=123; value=a=b"
        );
        for address in [
            "https://api.vk.com/method/",
            "https://fakevk.ru/",
            "http://vk.ru/",
            "https://cdn.example/",
        ] {
            assert_eq!(cookies.header(address).unwrap(), "");
        }
        for header in [
            "",
            "Cookie: secret=value",
            "invalid",
            "secret=x\r\nInjected: value",
            "bad name=value",
        ] {
            let error = Cookies::from_header(header, "https://vk.ru")
                .err()
                .unwrap()
                .to_string();
            assert!(!error.contains("secret") && !error.contains("value"));
        }
    }

    #[test]
    fn cookies_match_domains_paths_security_and_expiration() {
        let cookies = Cookies::parse("# Netscape\n#HttpOnly_.vk.ru\tTRUE\t/\tTRUE\t0\tsession\tsecret\n.vk.ru\tTRUE\t/audio\tFALSE\t0\tscope\tvalue\n.vk.ru\tTRUE\t/\tFALSE\t1\texpired\tvalue\n").unwrap();
        assert_eq!(
            cookies.header("https://m.vk.ru/audio").unwrap(),
            "session=secret; scope=value"
        );
        assert_eq!(
            cookies.header("https://vk.ru/audiobook").unwrap(),
            "session=secret"
        );
        assert_eq!(cookies.header("http://vk.ru/audio").unwrap(), "scope=value");
        assert_eq!(cookies.header("https://fakevk.ru/audio").unwrap(), "");
        assert_eq!(cookies.header("https://cdn.example/audio").unwrap(), "");
        assert_eq!(text("A &amp; <b>B</b>"), "A & B");
    }
}
