use super::Input;
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
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
    let metadata = fs::symlink_metadata(path).context("Missing private connection file")?;
    ensure!(
        metadata.file_type().is_file() && metadata.permissions().mode() & 0o077 == 0,
        "Connection files must be regular files with permissions 0600"
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
    ensure!(url.starts_with("https://"), "Insecure audio stream URL");
    Ok(Input::Pipe {
        program: std::env::current_exe()?,
        args: vec!["--stream-audio".into()],
        request: json!({"url":url}),
        quality: format!("{quality} · FLAC transport"),
    })
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
                expires: fields[4].parse().context("Invalid cookie expiration")?,
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
