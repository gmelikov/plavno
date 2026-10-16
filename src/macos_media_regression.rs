use super::*;
use objc2_foundation::NSDictionary;
use std::{sync::mpsc, time::Duration};

fn number(info: &NSDictionary<NSString, AnyObject>, key: &NSString) -> f64 {
    info.objectForKey(key)
        .unwrap()
        .downcast_ref::<NSNumber>()
        .unwrap()
        .as_f64()
}

// Invoked by the feature-gated binary entrypoint so AppKit runs on the process's
// main thread, unlike the normal Rust unit-test worker threads.
pub fn run() -> Result<()> {
    autoreleasepool(|_| unsafe {
        let (events, receiver) = mpsc::channel();
        let initial = Snapshot {
            id: (3, 7),
            title: Some("Тестовый трек".into()),
            artist: "Исполнитель".into(),
            duration: Some(60.0),
            position: 12.5,
            rate: 1.25,
            status: "Playing",
            capabilities: [true; 5],
            seek_revision: 0,
        };
        eprintln!("media test: initialize AppKit/MediaPlayer");
        let controls = MediaControls::create(initial.clone(), events.clone())?;
        eprintln!("media test: run loop and Now Playing");
        let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = polled.clone();
        NSRunLoop::mainRunLoop().performBlock(&RcBlock::new(move || {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }));
        controls.poll();
        assert!(
            polled.load(std::sync::atomic::Ordering::Relaxed),
            "Main run loop did not execute its pending block"
        );
        let info = controls
            .center
            .nowPlayingInfo()
            .context("Now Playing metadata missing")?;
        assert_eq!(
            info.objectForKey(MPMediaItemPropertyTitle)
                .unwrap()
                .downcast_ref::<NSString>()
                .unwrap()
                .to_string(),
            "Тестовый трек"
        );
        assert_eq!(
            info.objectForKey(MPMediaItemPropertyArtist)
                .unwrap()
                .downcast_ref::<NSString>()
                .unwrap()
                .to_string(),
            "Исполнитель"
        );
        assert_eq!(number(&info, MPMediaItemPropertyPlaybackDuration), 60.0);
        assert_eq!(
            number(&info, MPNowPlayingInfoPropertyElapsedPlaybackTime),
            12.5
        );
        assert_eq!(number(&info, MPNowPlayingInfoPropertyPlaybackRate), 1.25);
        assert_eq!(
            controls.center.playbackState(),
            MPNowPlayingPlaybackState::Playing
        );
        assert!(
            controls
                .bindings
                .iter()
                .all(|binding| binding.command.isEnabled())
        );

        // Native events have framework-owned initializers. Exercise the dispatcher
        // behind the registered blocks without constructing invalid native events.
        for (action, expected) in [
            (Action::Play, Command::Play),
            (Action::Pause, Command::Pause),
            (Action::Toggle, Command::PlayPause),
            (Action::Stop, Command::Stop),
            (Action::Next, Command::Next),
            (Action::Previous, Command::Previous),
        ] {
            assert_eq!(
                dispatch(action, &controls.snapshot, &events),
                MPRemoteCommandHandlerStatus::Success
            );
            let Event::Control(actual) = receiver.recv_timeout(Duration::from_secs(1))? else {
                panic!("Unexpected event")
            };
            assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
        }
        assert_eq!(
            dispatch(Action::Position(25.0), &controls.snapshot, &events),
            MPRemoteCommandHandlerStatus::Success
        );
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1))?,
            Event::Control(Command::SetPosition((3, 7), 25.0))
        ));
        for position in [-1.0, f64::NAN, f64::INFINITY, 61.0] {
            assert_eq!(
                dispatch(Action::Position(position), &controls.snapshot, &events),
                MPRemoteCommandHandlerStatus::CommandFailed
            );
        }
        assert_eq!(
            dispatch(Action::Rate(1.5), &controls.snapshot, &events),
            MPRemoteCommandHandlerStatus::Success
        );
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1))?,
            Event::Control(Command::Rate(1.5))
        ));

        eprintln!("media test: artwork and playback changes");
        let image = Arc::new(RgbImage::from_pixel(4, 4, image::Rgb([20, 40, 60])));
        let mut paused = initial.clone();
        paused.status = "Paused";
        controls.publish(paused.clone(), Some(&image));
        let info = controls.center.nowPlayingInfo().unwrap();
        assert_eq!(number(&info, MPNowPlayingInfoPropertyPlaybackRate), 0.0);
        assert_eq!(
            controls.center.playbackState(),
            MPNowPlayingPlaybackState::Paused
        );
        let art = info.objectForKey(MPMediaItemPropertyArtwork).unwrap();
        let art = art.downcast_ref::<MPMediaItemArtwork>().unwrap();
        assert!(art.imageWithSize(NSSize::new(4.0, 4.0)).is_some());
        let pointer = Retained::as_ptr(&controls.artwork.borrow().as_ref().unwrap().1);
        paused.position += 1.0;
        controls.publish(paused.clone(), Some(&image));
        assert_eq!(
            pointer,
            Retained::as_ptr(&controls.artwork.borrow().as_ref().unwrap().1)
        );

        let mut changed = paused.clone();
        changed.id = (3, 8);
        changed.title = Some("Next track".into());
        changed.seek_revision = 1;
        changed.position = 3.0;
        controls.publish(changed.clone(), None);
        let info = controls.center.nowPlayingInfo().unwrap();
        assert!(info.objectForKey(MPMediaItemPropertyArtwork).is_none());
        assert_eq!(
            number(&info, MPNowPlayingInfoPropertyElapsedPlaybackTime),
            3.0
        );
        assert_eq!(
            dispatch(Action::Position(5.0), &controls.snapshot, &events),
            MPRemoteCommandHandlerStatus::Success
        );
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1))?,
            Event::Control(Command::SetPosition((3, 8), 5.0))
        ));

        changed.capabilities = [false; 5];
        changed.title = None;
        changed.status = "Stopped";
        controls.publish(changed, None);
        assert!(controls.center.nowPlayingInfo().is_none());
        assert_eq!(
            controls.center.playbackState(),
            MPNowPlayingPlaybackState::Stopped
        );
        assert!(
            controls
                .bindings
                .iter()
                .all(|binding| !binding.command.isEnabled())
        );
        assert_eq!(
            dispatch(Action::Next, &controls.snapshot, &events),
            MPRemoteCommandHandlerStatus::NoActionableNowPlayingItem
        );
        assert!(receiver.try_recv().is_err());

        let state = controls.snapshot.clone();
        drop(controls);
        assert!(
            MPNowPlayingInfoCenter::defaultCenter()
                .nowPlayingInfo()
                .is_none()
        );
        assert_eq!(
            dispatch(Action::Play, &state, &events),
            MPRemoteCommandHandlerStatus::NoActionableNowPlayingItem
        );
        // Re-registration after cleanup must not leave duplicate callbacks/metadata.
        let controls = MediaControls::create(initial, events)?;
        controls.poll();
        drop(controls);
        println!("macOS media controls OK");
        Ok(())
    })
}
