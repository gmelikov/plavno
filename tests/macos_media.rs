#![cfg(all(target_os = "macos", feature = "test-audio"))]

#[test]
#[ignore = "Runs the native main-thread runtime in the dedicated macOS CI step"]
fn native_media_controls_publish_metadata_and_route_commands_on_the_main_thread() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_plavno"))
        .arg("--test-media-controls")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("macOS media controls OK"));
}
