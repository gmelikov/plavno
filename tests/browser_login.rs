use std::{
    fs,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::PathBuf,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn cli_vk_signin_completes_and_failed_logins_keep_existing_sessions() {
    for mode in ["authenticated", "interrupt", "close"] {
        let root =
            std::env::temp_dir().join(format!("plavno-login-test-{}-{mode}", std::process::id()));
        fs::create_dir(&root).unwrap();
        let fixture = Fixture(root);
        let directory = fixture.0.join("plavno");
        fs::create_dir(&directory).unwrap();
        let profile = directory.join("vk.json");
        let old = br#"{"user_id":123,"token":"previous-fixture-token"}"#;
        fs::write(&profile, old).unwrap();
        fs::set_permissions(&profile, fs::Permissions::from_mode(0o600)).unwrap();
        let program = fixture.0.join("browser");
        fs::write(&program, r#"#!/bin/bash
printf '%s' "$$" > "$PID_FILE"
for argument in "$@"; do
  case "$argument" in --user-data-dir=*) printf '%s' "${argument#--user-data-dir=}" > "$PROFILE_FILE";; esac
done
while IFS= read -r -d '' request <&3; do
  id=${request#*\"id\":}; id=${id%%,*}
  case "$request" in
    *Browser.getVersion*) result='{"userAgent":"Fixture browser","product":"Chrome/fixture"}';;
    *Target.createTarget*) result='{"targetId":"page"}';;
    *Target.attachToTarget*) result='{"sessionId":"session"}';;
    *Runtime.evaluate*)
      if [ "$FIXTURE_MODE" = authenticated ]; then
        result='{"result":{"value":{"url":"https://vk.ru/feed","userId":123}}}'
      else result='{"result":{"value":{"url":"https://vk.ru/audio","userId":0}}}'; fi;;
    *Network.getCookies*)
      if [ "$FIXTURE_MODE" = close ]; then exit 0; fi
      if [ "$FIXTURE_MODE" = authenticated ]; then
        result='{"cookies":[{"domain":".vk.ru","path":"/","name":"remixsid6","value":"fixture-session","httpOnly":true,"secure":true,"expires":-1}]}'
      else result='{"cookies":[]}'; fi;;
    *Browser.close*) exit 0;;
    *) result='{}';;
  esac
  printf '{"id":%s,"result":%s}\0' "$id" "$result" >&4
done
"#).unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o700)).unwrap();
        let pid_file = fixture.0.join("pid");
        let profile_file = fixture.0.join("browser-profile");
        let mut child = Command::new(env!("CARGO_BIN_EXE_plavno"))
            .args(["login", "vk"])
            .env("XDG_CONFIG_HOME", &fixture.0)
            .env("PLAVNO_BROWSER", &program)
            .env("DISPLAY", ":fixture")
            .env("PID_FILE", &pid_file)
            .env("PROFILE_FILE", &profile_file)
            .env("FIXTURE_MODE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while (!pid_file.exists() || !profile_file.exists()) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        if mode == "interrupt" {
            unsafe {
                libc::kill(child.id() as i32, libc::SIGINT);
            }
        }
        while child.try_wait().unwrap().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let timed_out = child.try_wait().unwrap().is_none();
        if timed_out {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(!timed_out, "Login did not stop after cancellation");
        let stderr = String::from_utf8(output.stderr).unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        if mode == "authenticated" {
            assert!(output.status.success(), "{stderr}");
            assert!(stdout.contains("Connected. Start playback with: plavno --source vk"));
            let saved: serde_json::Value =
                serde_json::from_slice(&fs::read(&profile).unwrap()).unwrap();
            assert_eq!(saved["user_id"], 123);
            assert_eq!(saved["site"], "https://vk.ru");
            assert_eq!(saved["auth"], "browser");
            assert!(saved.get("token").is_none());
            let cookies = PathBuf::from(saved["cookies"].as_str().unwrap());
            assert!(
                fs::read_to_string(&cookies)
                    .unwrap()
                    .contains("remixsid6\tfixture-session")
            );
            for path in [&profile, &cookies] {
                assert_eq!(
                    fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    0o600
                );
            }
        } else {
            assert!(!output.status.success());
            assert_eq!(fs::read(&profile).unwrap(), old);
            assert!(
                stderr.contains(if mode == "interrupt" {
                    "Browser login cancelled"
                } else {
                    "Browser"
                }),
                "{stderr}"
            );
        }
        assert!(!stderr.contains("previous-fixture-token"));
        assert!(!stderr.contains("fixture-session"));
        assert!(!stdout.contains("previous-fixture-token"));
        assert!(!stdout.contains("fixture-session"));
        let pid: i32 = fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        let temporary = fs::read_to_string(profile_file).unwrap();
        assert!(!PathBuf::from(temporary).exists());
        assert_eq!(
            fs::read_dir(directory).unwrap().count(),
            if mode == "authenticated" { 2 } else { 1 }
        );
    }
}
