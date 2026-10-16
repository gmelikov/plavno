use super::*;
use std::sync::mpsc;

pub fn run() -> Result<()> {
    let _runtime = Runtime::new()?;
    // Exercise the same WinRT -> WASAPI initialization order as the real player.
    use rodio::cpal::traits::HostTrait;
    let _ = rodio::cpal::default_host().default_output_device();
    let (sender, receiver) = mpsc::channel();
    let initial = Snapshot {
        id: (7, 9),
        title: Some("Название трека".into()),
        artist: "Artist".into(),
        duration: Some(120.0),
        position: 12.0,
        rate: 1.25,
        status: "Playing",
        capabilities: [true; 5],
        seek_revision: 0,
    };
    let controls = MediaControls::create(initial.clone(), sender.clone())?;
    controls.poll();
    assert!(controls.center.IsEnabled()?);
    assert!(controls.center.IsPlayEnabled()?);
    assert!(controls.center.IsPauseEnabled()?);
    assert_eq!(
        controls.center.PlaybackStatus()?,
        MediaPlaybackStatus::Playing
    );
    assert_eq!(controls.center.PlaybackRate()?, 1.25);
    let display = controls.center.DisplayUpdater()?;
    assert_eq!(display.Type()?, MediaPlaybackType::Music);
    assert_eq!(
        display.MusicProperties()?.Title()?,
        HSTRING::from("Название трека")
    );
    assert_eq!(
        display.MusicProperties()?.Artist()?,
        HSTRING::from("Artist")
    );
    assert_eq!(controls.bindings.len(), 3);
    for (button, expected) in [
        (SystemMediaTransportControlsButton::Play, "play"),
        (SystemMediaTransportControlsButton::Pause, "pause"),
        (SystemMediaTransportControlsButton::Stop, "stop"),
        (SystemMediaTransportControlsButton::Next, "next"),
        (SystemMediaTransportControlsButton::Previous, "previous"),
    ] {
        dispatch(Action::Button(button), &controls.snapshot, &sender);
        let Event::Control(command) = receiver.recv_timeout(std::time::Duration::from_secs(1))?
        else {
            panic!("Unexpected event")
        };
        assert!(matches!(
            (command, expected),
            (Command::Play, "play")
                | (Command::Pause, "pause")
                | (Command::Stop, "stop")
                | (Command::Next, "next")
                | (Command::Previous, "previous")
        ));
    }
    dispatch(Action::Position(150.0), &controls.snapshot, &sender);
    assert!(matches!(
        receiver.try_recv()?,
        Event::Control(Command::SetPosition((7, 9), 120.0))
    ));
    dispatch(Action::Rate(1.5), &controls.snapshot, &sender);
    assert!(matches!(
        receiver.try_recv()?,
        Event::Control(Command::Rate(1.5))
    ));
    for action in [
        Action::Position(-1.0),
        Action::Position(f64::NAN),
        Action::Rate(f64::NAN),
        Action::Rate(0.0),
        Action::Rate(3.0),
    ] {
        dispatch(action, &controls.snapshot, &sender);
    }
    assert!(receiver.try_recv().is_err());
    let cover = Arc::new(RgbImage::from_pixel(16, 16, image::Rgb([30, 60, 90])));
    controls.publish(initial.clone(), Some(&cover))?;
    let thumbnail = display.Thumbnail()?;
    let bytes = thumbnail.OpenReadAsync()?.get()?;
    assert!(bytes.Size()? > 0);
    controls.publish(initial.clone(), Some(&cover))?;
    assert_eq!(display.Thumbnail()?, thumbnail);
    let mut paused = initial.clone();
    paused.status = "Paused";
    controls.publish(paused.clone(), Some(&cover))?;
    assert_eq!(
        controls.center.PlaybackStatus()?,
        MediaPlaybackStatus::Paused
    );
    paused.capabilities = [false; 5];
    paused.seek_revision = 1;
    controls.publish(paused, None)?;
    assert!(!controls.center.IsNextEnabled()?);
    assert!(!controls.center.IsPreviousEnabled()?);
    assert!(display.Thumbnail().is_err());
    dispatch(
        Action::Button(SystemMediaTransportControlsButton::Next),
        &controls.snapshot,
        &sender,
    );
    dispatch(Action::Position(15.0), &controls.snapshot, &sender);
    assert!(receiver.try_recv().is_err());
    let mut next = initial.clone();
    next.id = (8, 10);
    next.title = Some("Next".into());
    next.position = 0.0;
    controls.publish(next, None)?;
    dispatch(Action::Position(10.0), &controls.snapshot, &sender);
    assert!(matches!(
        receiver.try_recv()?,
        Event::Control(Command::SetPosition((8, 10), 10.0))
    ));
    let snapshot = controls.snapshot.clone();
    let center = controls.center.clone();
    drop(controls);
    assert!(!center.IsEnabled()?);
    dispatch(
        Action::Button(SystemMediaTransportControlsButton::Play),
        &snapshot,
        &sender,
    );
    assert!(receiver.try_recv().is_err());
    drop(center);
    drop(display);
    drop(thumbnail);
    drop(bytes);
    drop(MediaControls::create(initial, sender)?);
    println!("Windows media controls OK");
    Ok(())
}
