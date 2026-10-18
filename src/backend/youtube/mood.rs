use super::*;

const TTL: Duration = Duration::from_secs(30 * 60);
const CATEGORY: &str = "FEmusic_moods_and_genres_category";

pub(super) struct Catalog {
    account: String,
    loaded: Instant,
    categories: Vec<(WaveStyle, String)>,
    playlists: Vec<(WaveStyle, Vec<String>)>,
}

pub(super) struct Queue {
    playlists: VecDeque<String>,
    offset: usize,
}

fn visit(value: &Value, f: &mut impl FnMut(&Value)) {
    match value {
        Value::Object(object) => {
            f(value);
            for value in object.values() {
                visit(value, f);
            }
        }
        Value::Array(values) => {
            for value in values {
                visit(value, f);
            }
        }
        _ => {}
    }
}

fn categories(value: &Value) -> Vec<(WaveStyle, String)> {
    let mut categories = Vec::new();
    visit(value, &mut |value| {
        let button = &value["musicNavigationButtonRenderer"];
        let title: String = button["buttonText"]["runs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|run| run["text"].as_str())
            .collect();
        let style = match title.as_str() {
            "Chill" => WaveStyle::Calm,
            "Energize" => WaveStyle::Energetic,
            "Focus" => WaveStyle::Work,
            _ => return,
        };
        let endpoint = &button["clickCommand"]["browseEndpoint"];
        if endpoint["browseId"] == CATEGORY
            && let Some(params) = endpoint["params"].as_str().filter(|p| p.len() <= 2048)
            && !params.is_empty()
            && !categories.iter().any(|(s, _)| *s == style)
        {
            categories.push((style, params.to_owned()));
        }
    });
    categories
}

fn playlists(value: &Value) -> Vec<String> {
    let mut playlists = Vec::new();
    visit(value, &mut |value| {
        // Use the card's playlist, never its "Start mix" menu radio endpoint.
        let endpoint = &value["musicTwoRowItemRenderer"]["navigationEndpoint"]["browseEndpoint"];
        if let Some(id) = endpoint["browseId"]
            .as_str()
            .and_then(|id| id.strip_prefix("VL"))
            && !id.is_empty()
            && id.len() <= 256
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            && !playlists.iter().any(|p| p == id)
            && playlists.len() < 256
        {
            playlists.push(id.to_owned());
        }
    });
    playlists
}

impl YouTube {
    fn mood_playlists(&mut self) -> Result<Vec<String>> {
        self.check_cooldown()?;
        let account = self.session()?.account.clone();
        if self
            .mood_catalog
            .as_ref()
            .is_none_or(|c| c.account != account || c.loaded.elapsed() >= TTL)
        {
            self.mood_catalog = None;
            let value = self.api("browse", json!({"browseId":"FEmusic_moods_and_genres"}))?;
            self.mood_catalog = Some(Catalog {
                account,
                loaded: Instant::now(),
                categories: categories(&value),
                playlists: Vec::new(),
            });
        }
        let catalog = self.mood_catalog.as_ref().unwrap();
        if let Some((_, playlists)) = catalog.playlists.iter().find(|(s, _)| *s == self.style) {
            return Ok(playlists.clone());
        }
        let params = catalog
            .categories
            .iter()
            .find(|(s, _)| *s == self.style)
            .map(|(_, p)| p.clone())
            .context(
                "YouTube Music did not return this mood category; choose another Flow style",
            )?;
        let value = self.api("browse", json!({"browseId":CATEGORY,"params":params}))?;
        let playlists = playlists(&value);
        ensure!(
            !playlists.is_empty(),
            "YouTube Music returned no playlists for this mood; choose another Flow style"
        );
        self.mood_catalog
            .as_mut()
            .unwrap()
            .playlists
            .push((self.style, playlists.clone()));
        Ok(playlists)
    }

    pub(super) fn mood_batch(&mut self, history: &[String]) -> Result<Vec<Track>> {
        if self.session()?.mood.is_none() {
            let mut playlists = self.mood_playlists()?;
            super::super::yandex::shuffle_favorites(
                &mut playlists,
                None,
                &mut plavno_platform::Random::new()?,
            )?;
            self.session.as_mut().unwrap().mood = Some(Queue {
                playlists: playlists.into(),
                offset: 1,
            });
        }
        let mut seen = history.iter().cloned().collect();
        // Bound requests even if playlists are empty or contain only played tracks.
        for _ in 0..3 {
            let queue = self.session()?.mood.as_ref().unwrap();
            let Some(playlist) = queue.playlists.front() else {
                break;
            };
            let start = queue.offset;
            let mut address = url::Url::parse("https://www.youtube.com/playlist")?;
            address.query_pairs_mut().append_pair("list", playlist);
            let value = self.playlist(address.as_str(), start, start + 24)?;
            let batch = tracks(&value, &mut seen)?;
            let queue = self.session.as_mut().unwrap().mood.as_mut().unwrap();
            if value["entries"].as_array().unwrap().len() < 25 {
                queue.playlists.pop_front();
                queue.offset = 1;
            } else {
                queue.offset += 25;
            }
            if !batch.is_empty() {
                return Ok(batch);
            }
        }
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        likes::tests::{account, server},
        tests::Fixture,
    };
    use super::*;

    fn catalog() -> Value {
        serde_json::from_str(include_str!(
            "../../../tests/fixtures/yt_mood_categories.json"
        ))
        .unwrap()
    }

    fn selection(ids: &[&str]) -> Value {
        json!({"contents": ids.iter().map(|id| json!({"musicTwoRowItemRenderer": {
            "navigationEndpoint":{"browseEndpoint":{"browseId":format!("VL{id}")}},
            "menu":{"watchPlaylistEndpoint":{"playlistId":"RDAMPL_unrelated_radio"}}
        }})).collect::<Vec<_>>()})
    }

    fn youtube(fixture: &Fixture) -> YouTube {
        let mut youtube = account(fixture);
        youtube.seed = None;
        youtube
    }

    fn bodies(worker: std::thread::JoinHandle<Vec<String>>) -> Vec<Value> {
        worker
            .join()
            .unwrap()
            .iter()
            .map(|r| serde_json::from_str(r.split_once("\r\n\r\n").unwrap().1).unwrap())
            .collect()
    }

    #[test]
    fn public_response_projections_map_styles_and_use_playlist_cards_only() {
        let categories = categories(&catalog());
        assert_eq!(categories.len(), 3);
        for style in [WaveStyle::Calm, WaveStyle::Energetic, WaveStyle::Work] {
            assert!(categories.iter().any(|(s, p)| *s == style && !p.is_empty()));
        }
        let value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/yt_mood_playlists.json"
        ))
        .unwrap();
        assert_eq!(playlists(&value).len(), 2);
        assert_eq!(
            playlists(&selection(&["PL_first", "PL_first", "../bad", ""])),
            ["PL_first"]
        );
    }

    #[test]
    fn styles_select_categories_without_a_seed_and_reuse_catalog_across_restarts() {
        let fixture = Fixture::new();
        let mut youtube = youtube(&fixture);
        assert!(youtube.services()[0].wave_styles);
        let mut responses = vec![(200, catalog())];
        for _ in 0..3 {
            responses.push((200, selection(&["PL_mood"])));
        }
        let (base, worker) = server(responses);
        youtube.api_base = Some(base);
        for (index, style) in [
            WaveStyle::Calm,
            WaveStyle::Energetic,
            WaveStyle::Work,
            WaveStyle::Calm,
        ]
        .into_iter()
        .enumerate()
        {
            fixture.reply(index + 1, json!({"entries":[{"id":"abcdefghijk"}]}));
            assert_eq!(
                youtube
                    .start_with_style("ytmusic", super::super::super::PlaybackMode::MyVibe, style)
                    .unwrap()[0]
                    .id,
                "abcdefghijk"
            );
        }
        let calls = fixture.calls();
        assert_eq!(calls.len(), 4);
        for call in calls {
            assert_eq!(
                call.last().unwrap(),
                "https://www.youtube.com/playlist?list=PL_mood"
            );
            assert!(call.contains(&"--flat-playlist".into()));
        }
        let bodies = bodies(worker);
        assert_eq!(
            bodies.len(),
            4,
            "One catalog fetch, one per new mood, no home or library requests"
        );
        assert_eq!(bodies[0]["browseId"], "FEmusic_moods_and_genres");
        for (request, style) in
            bodies[1..]
                .iter()
                .zip([WaveStyle::Calm, WaveStyle::Energetic, WaveStyle::Work])
        {
            assert_eq!(request["browseId"], CATEGORY);
            assert_eq!(
                request["params"],
                categories(&catalog())
                    .iter()
                    .find(|(s, _)| *s == style)
                    .unwrap()
                    .1
            );
        }
        assert!(
            web::profile(&youtube.profile)
                .unwrap()
                .get("seed")
                .is_none()
        );
    }

    #[test]
    fn mood_pages_stay_in_category_skip_played_tracks_and_advance_playlists() {
        let fixture = Fixture::new();
        let mut youtube = youtube(&fixture);
        let (base, worker) = server(vec![
            (200, catalog()),
            (200, selection(&["PL_first", "RDCLAK_second"])),
        ]);
        youtube.api_base = Some(base);
        youtube.set_wave_style(WaveStyle::Calm).unwrap();
        let entries: Vec<_> = (0..25).map(|i| json!({"id":format!("{i:011}")})).collect();
        fixture.reply(1, json!({"entries":entries}));
        let initial = youtube.start().unwrap();
        assert_eq!(initial.len(), 25);
        fixture.reply(
            2,
            json!({"entries":[{"id":"00000000000"},{"id":"abcdefghijk","live_status":"is_live"}]}),
        );
        fixture.reply(3, json!({"entries":[{"id":"9EcDe6qS0Xc"}]}));
        assert_eq!(
            youtube
                .more(&initial.into_iter().map(|t| t.id).collect::<Vec<_>>())
                .unwrap()[0]
                .id,
            "9EcDe6qS0Xc"
        );
        assert!(youtube.more(&[]).unwrap().is_empty());
        let calls = fixture.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].last(), calls[1].last());
        assert_ne!(calls[1].last(), calls[2].last());
        assert!(calls[1].contains(&"26:50".into()));
        assert!(calls[2].contains(&"1:25".into()));
        assert_eq!(bodies(worker).len(), 2);
    }

    #[test]
    fn expired_or_different_account_catalog_is_not_reused() {
        for expired in [true, false] {
            let fixture = Fixture::new();
            let mut youtube = youtube(&fixture);
            let account = youtube.session().unwrap().account.clone();
            youtube.mood_catalog = Some(Catalog {
                account: if expired {
                    account
                } else {
                    "another-account".into()
                },
                loaded: if expired {
                    Instant::now() - TTL
                } else {
                    Instant::now()
                },
                categories: Vec::new(),
                playlists: vec![(WaveStyle::Calm, vec!["PL_stale".into()])],
            });
            youtube.set_wave_style(WaveStyle::Calm).unwrap();
            let (base, worker) = server(vec![(200, catalog()), (200, selection(&["PL_fresh"]))]);
            youtube.api_base = Some(base);
            fixture.reply(1, json!({"entries":[{"id":"abcdefghijk"}]}));
            youtube.start().unwrap();
            assert!(
                fixture.calls()[0]
                    .last()
                    .unwrap()
                    .ends_with("list=PL_fresh")
            );
            assert_eq!(bodies(worker).len(), 2);
        }
    }

    #[test]
    fn mood_rate_limit_stops_requests_and_survives_restart() {
        let fixture = Fixture::new();
        let mut youtube = youtube(&fixture);
        youtube.set_wave_style(WaveStyle::Work).unwrap();
        let (base, worker) = server(vec![(200, catalog()), (429, json!({}))]);
        youtube.api_base = Some(base);
        assert!(
            youtube
                .start()
                .unwrap_err()
                .downcast_ref::<RateLimited>()
                .is_some()
        );
        assert!(
            youtube
                .start()
                .unwrap_err()
                .downcast_ref::<RateLimited>()
                .is_some()
        );
        let mut restarted = self::youtube(&fixture);
        restarted.set_wave_style(WaveStyle::Work).unwrap();
        assert!(
            restarted
                .start()
                .unwrap_err()
                .downcast_ref::<RateLimited>()
                .is_some()
        );
        assert!(fixture.calls().is_empty());
        assert_eq!(bodies(worker).len(), 2);
    }

    #[test]
    fn unavailable_category_does_not_fall_back_to_unfiltered_radio() {
        let fixture = Fixture::new();
        let mut youtube = youtube(&fixture);
        let (base, worker) = server(vec![(200, json!({"contents":[]}))]);
        youtube.api_base = Some(base);
        youtube.set_wave_style(WaveStyle::Work).unwrap();
        assert!(
            youtube
                .start()
                .unwrap_err()
                .to_string()
                .contains("mood category")
        );
        assert!(fixture.calls().is_empty());
        assert_eq!(bodies(worker).len(), 1);
    }

    #[test]
    fn balanced_after_a_mood_restores_the_personal_mix() {
        let fixture = Fixture::new();
        let mut youtube = youtube(&fixture);
        let (base, worker) = server(vec![
            (200, catalog()),
            (200, selection(&["PL_mood"])),
            (
                200,
                json!({"watchEndpoint":{
                    "videoId":"abcdefghijk","playlistId":"RDAMVMabcdefghijk",
                    "watchEndpointMusicSupportedConfigs":{"watchEndpointMusicConfig":{"musicVideoType":"MUSIC_VIDEO_TYPE_ATV"}}
                }}),
            ),
        ]);
        youtube.api_base = Some(base);
        fixture.reply(1, json!({"entries":[{"id":"9EcDe6qS0Xc"}]}));
        fixture.reply(2, json!({"entries":[{"id":"5LGUgChapj4"}]}));
        youtube.set_wave_style(WaveStyle::Calm).unwrap();
        youtube.start().unwrap();
        youtube.set_wave_style(WaveStyle::Balanced).unwrap();
        assert_eq!(youtube.start().unwrap()[0].id, "5LGUgChapj4");
        assert_eq!(
            fixture.calls()[1].last().unwrap(),
            "https://www.youtube.com/watch?v=abcdefghijk&list=RDMM"
        );
        assert_eq!(bodies(worker)[2]["browseId"], "FEmusic_home");
    }

    #[test]
    fn failed_playlist_load_is_not_retried_or_advanced_and_reuses_catalog() {
        let fixture = Fixture::new();
        let mut youtube = youtube(&fixture);
        let (base, worker) = server(vec![(200, catalog()), (200, selection(&["PL_mood"]))]);
        youtube.api_base = Some(base);
        youtube.set_wave_style(WaveStyle::Work).unwrap();
        fs::write(fixture.0.join("1.status"), "1").unwrap();
        fs::write(fixture.0.join("1.err"), "request timed out").unwrap();
        assert!(youtube.start().is_err());
        assert_eq!(
            fixture.calls().len(),
            1,
            "No automatic retry of a failed load"
        );
        fixture.reply(2, json!({"entries":[{"id":"abcdefghijk"}]}));
        assert_eq!(youtube.more(&[]).unwrap()[0].id, "abcdefghijk");
        assert_eq!(fixture.calls()[0], fixture.calls()[1]);
        assert_eq!(bodies(worker).len(), 2);
    }

    #[test]
    fn public_seed_override_keeps_track_radio_and_disables_flow() {
        let fixture = Fixture::new();
        let mut youtube = account(&fixture);
        assert!(!youtube.supports_wave_styles());
        assert!(youtube.set_wave_style(WaveStyle::Calm).is_err());
        fixture.reply(1, json!({"entries":[{"id":"abcdefghijk"}]}));
        youtube.start().unwrap();
        assert!(
            fixture.calls()[0]
                .last()
                .unwrap()
                .contains("v=5LGUgChapj4&list=RD5LGUgChapj4")
        );
    }
}
