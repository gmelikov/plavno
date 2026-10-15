use crate::dance_motion::Features;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    Sway,
    Reach,
    Sweep,
    Travel,
    Crouch,
    Hop,
    Jack,
    Shuffle,
    Stomp,
    Rock,
    Punch,
    Hakken,
}

pub struct Phrase {
    pub movement: Move,
    pub duration: f32,
    pub progress: f32,
    pub start: f32,
    pub destination: f32,
    pub gesture_offset: f32,
    pub hand_delay: f32,
    pub reach: f32,
    pub pace: f32,
    pub performed: bool,
}

pub struct Choreography {
    pub phrase: Phrase,
    pub repertoire: [f32; 3],
    random: u64,
    last_direction: f32,
    since_hop: f32,
    previous: Move,
}

impl Choreography {
    pub fn new(seed: u64) -> Self {
        Self {
            repertoire: [0.0; 3],
            phrase: Phrase {
                movement: Move::Sway,
                duration: 1.5,
                progress: 0.0,
                start: 0.0,
                destination: 0.0,
                gesture_offset: 0.0,
                hand_delay: 1.65,
                reach: 1.0,
                pace: 1.0,
                performed: false,
            },
            random: seed,
            last_direction: 1.0,
            since_hop: 30.0,
            previous: Move::Sway,
        }
    }

    pub fn random(&mut self) -> f32 {
        self.random = self.random.wrapping_add(0x9e3779b97f4a7c15);
        let mut value = self.random;
        value = (value ^ (value >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        value = (value ^ (value >> 27)).wrapping_mul(0x94d049bb133111eb);
        ((value ^ (value >> 31)) >> 40) as f32 / 16_777_216.0
    }

    pub fn advance(
        &mut self,
        music: Features,
        weights: [f32; 3],
        center: f32,
        limit: f32,
        planted: bool,
        dt: f32,
    ) {
        self.since_hop += dt;
        self.phrase.progress = (self.phrase.progress + dt / self.phrase.duration).min(1.0);
        if self.phrase.progress < 1.0 || !planted || music.energy < 0.08 {
            return;
        }
        let rhythm = weights[1] + weights[2];
        let club = rhythm > 0.5 && music.energy > 0.6;
        let [house, hiphop, rave] = self.repertoire;
        let mut choices = vec![
            (Move::Sway, if club { 0.4 } else { 1.0 }),
            (Move::Reach, if club { 0.6 } else { 1.3 }),
            (Move::Sweep, 1.3),
        ];
        if limit > 0.2 {
            choices.push((Move::Travel, 2.5));
        }
        if club {
            choices.push((Move::Jack, 2.2 + house * 4.0));
            choices.push((Move::Shuffle, 2.8 + house * 3.0 + rave));
            if weights[2] > 0.25 {
                choices.push((Move::Stomp, 1.8 + rave * 3.0));
            }
            if rave > 0.1 {
                choices.push((Move::Hakken, rave * 6.0));
            }
        }
        if rhythm > 0.5 && music.energy > 0.35 {
            if hiphop > 0.1 {
                choices.push((Move::Rock, hiphop * 6.0));
            }
            if hiphop + rave > 0.1 {
                choices.push((Move::Punch, (hiphop + rave) * 3.5));
            }
        }
        if rhythm > 0.35 && music.energy > 0.45 {
            choices.push((Move::Crouch, 1.2));
        }
        if rhythm > 0.55 && music.energy > 0.65 && music.bass > 0.2 && self.since_hop > 8.0 {
            choices.push((Move::Hop, 0.9));
        }
        choices.retain(|(movement, _)| {
            *movement != self.phrase.movement && *movement != self.previous
        });
        let mut pick = self.random() * choices.iter().map(|(_, weight)| weight).sum::<f32>();
        let mut movement = Move::Sway;
        for (candidate, weight) in choices {
            movement = candidate;
            pick -= weight;
            if pick <= 0.0 {
                break;
            }
        }
        let beats = if music.energy > 0.75 && rhythm > 0.6 {
            if self.random() < 0.6 { 4.0 } else { 6.0 }
        } else if self.random() < 0.5 {
            6.0
        } else {
            8.0
        };
        let seconds = 8.0 + self.random() * 5.0;
        let mut duration = seconds * weights[0] + beats * music.period * rhythm;
        if matches!(movement, Move::Crouch | Move::Hop) {
            duration = (music.period * 4.0).clamp(1.8, 4.0);
        }
        if movement == Move::Travel {
            duration = duration.max(music.period.max(0.3) * 8.0 * rhythm);
        }
        let mut direction = if self.random() < 0.55 {
            -self.last_direction
        } else {
            self.last_direction
        };
        if center > limit * 0.55 {
            direction = -1.0;
        }
        if center < -limit * 0.55 {
            direction = 1.0;
        }
        let distance = (0.7 + self.random() * 0.8) * (0.6 + music.energy * 0.4);
        let destination = if movement == Move::Travel {
            self.last_direction = direction;
            (center + direction * distance).clamp(-limit, limit)
        } else {
            center
        };
        if movement == Move::Travel {
            duration = duration.max((destination - center).abs() / 0.45 + 0.6);
        }
        if movement == Move::Hop {
            self.since_hop = 0.0;
        }
        self.previous = self.phrase.movement;
        self.phrase = Phrase {
            movement,
            duration,
            progress: 0.0,
            start: center,
            destination,
            gesture_offset: self.random() * 8.0,
            hand_delay: 0.8 + self.random() * 2.0,
            reach: 0.8 + self.random() * 0.35,
            pace: 0.75 + self.random() * 0.5 + weights[2] * music.energy * 0.35,
            performed: false,
        };
    }

    pub fn center(&self, lookahead: f32) -> f32 {
        let progress = (self.phrase.progress + lookahead / self.phrase.duration).clamp(0.0, 1.0);
        self.phrase.start + (self.phrase.destination - self.phrase.start) * ease(progress)
    }

    pub fn finish_phrase(&mut self) {
        self.phrase.progress = 1.0;
    }

    pub fn bridge(&mut self, center: f32) {
        self.previous = self.phrase.movement;
        self.phrase.movement = Move::Sway;
        self.phrase.start = center;
        self.phrase.destination = center;
        self.phrase.progress = 0.0;
        self.phrase.duration = 1.5;
        self.phrase.performed = false;
    }

    pub fn envelope(&self) -> f32 {
        let p = self.phrase.progress;
        ease((p / 0.25).min(1.0)) * ease(((1.0 - p) / 0.25).min(1.0))
    }
}

pub fn ease(t: f32) -> f32 {
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_repertoires_bias_phrases_without_excluding_other_moves() {
        let sample = |repertoire| {
            let mut choreography = Choreography::new(42);
            choreography.repertoire = repertoire;
            let mut counts = [0; 4];
            for _ in 0..600 {
                choreography.advance(
                    Features {
                        energy: 0.95,
                        bass: 0.9,
                        confidence: 1.0,
                        period: 0.35,
                        ..Features::default()
                    },
                    [0.0, 0.4, 0.6],
                    0.0,
                    1.5,
                    true,
                    20.0,
                );
                match choreography.phrase.movement {
                    Move::Jack | Move::Shuffle => counts[0] += 1,
                    Move::Rock => counts[1] += 1,
                    Move::Hakken => counts[2] += 1,
                    Move::Travel => counts[3] += 1,
                    _ => {}
                }
            }
            counts
        };
        let house = sample([1.0, 0.0, 0.0]);
        let hiphop = sample([0.0, 1.0, 0.0]);
        let rave = sample([0.0, 0.0, 1.0]);
        let mixed = sample([0.4, 0.2, 0.4]);
        assert!(house[0] > hiphop[0] && house[0] > rave[0]);
        assert!(hiphop[1] > 0 && house[1] == 0 && rave[1] == 0);
        assert!(rave[2] > 0 && house[2] == 0 && hiphop[2] == 0);
        assert!(mixed.iter().all(|count| *count > 0));
        assert!(house[3] > 0 && hiphop[3] > 0 && rave[3] > 0);
    }

    #[test]
    fn rhythmic_phrases_include_club_motifs_without_displacing_stage_travel() {
        let music = Features {
            energy: 0.95,
            bass: 0.9,
            confidence: 1.0,
            period: 0.35,
            ..Features::default()
        };
        let mut choreography = Choreography::new(42);
        let mut seen = Vec::new();
        for _ in 0..100 {
            choreography.advance(music, [0.0, 0.0, 1.0], 0.0, 1.5, true, 20.0);
            seen.push(choreography.phrase.movement);
            if choreography.phrase.movement == Move::Travel {
                assert!(choreography.phrase.duration >= music.period * 8.0);
            }
        }
        for movement in [Move::Jack, Move::Shuffle, Move::Stomp, Move::Travel] {
            assert!(seen.contains(&movement), "Missing {movement:?}");
        }
    }

    #[test]
    fn fast_travel_leaves_time_for_weight_transfer_and_multiple_contacts() {
        let mut choreography = Choreography::new(42);
        let music = Features {
            energy: 0.95,
            confidence: 1.0,
            period: 0.18,
            ..Features::default()
        };
        let mut travels = 0;
        for _ in 0..100 {
            choreography.advance(music, [0.0, 0.0, 1.0], 0.0, 1.5, true, 20.0);
            let phrase = &choreography.phrase;
            if phrase.movement == Move::Travel {
                travels += 1;
                assert!(phrase.duration >= 2.4);
                assert!(phrase.duration >= (phrase.destination - phrase.start).abs() / 0.45 + 0.6);
            }
        }
        assert!(travels > 0);
    }

    #[test]
    fn calm_phrases_vary_between_sessions_without_repeating_recent_moves() {
        let music = Features {
            energy: 0.7,
            ..Features::default()
        };
        let sequence = |seed| {
            let mut choreography = Choreography::new(seed);
            let mut last = Move::Sway;
            let mut before_last = Move::Sway;
            let mut sequence = Vec::new();
            for _ in 0..60 {
                choreography.advance(music, [1.0, 0.0, 0.0], 0.0, 1.5, true, 20.0);
                let phrase = &choreography.phrase;
                assert_ne!(phrase.movement, last);
                assert_ne!(phrase.movement, before_last);
                assert!(!matches!(
                    phrase.movement,
                    Move::Hop | Move::Crouch | Move::Jack | Move::Shuffle | Move::Stomp
                ));
                assert!(phrase.destination.abs() <= 1.5);
                sequence.push((phrase.movement, phrase.gesture_offset.to_bits()));
                before_last = last;
                last = phrase.movement;
            }
            sequence
        };
        assert_eq!(sequence(42), sequence(42));
        assert_ne!(sequence(42), sequence(43));
    }
}
