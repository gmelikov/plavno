#![cfg(all(windows, feature = "test-audio"))]

#[test]
#[ignore = "Runs native SMTC in the dedicated Windows CI step"]
fn native_media_controls_publish_metadata_artwork_and_route_commands() {
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("Windows media controls OK"));
}
