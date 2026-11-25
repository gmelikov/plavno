use crate::choreography::ease;

const FRAMES: usize = 97;
const CHANNELS: usize = 16;
const FRAME_BYTES: usize = CHANNELS * 2;
const STEP_BYTES: usize = 13;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Clip {
    Soft,
    Turn,
}

#[derive(Default)]
pub struct Sample {
    pub root: [f32; 3],
    pub yaw: f32,
    pub twist: f32,
    pub look: f32,
    pub lumbar: f32,
    pub pitch: f32,
    pub wrists: [[f32; 3]; 2],
    pub foot_roll: [f32; 2],
    pub blend: f32,
}

pub struct Step {
    pub side: usize,
    pub start: f32,
    pub end: f32,
    pub x: f32,
    pub depth: f32,
    pub yaw: f32,
    pub lift: f32,
}

impl Clip {
    fn data(self) -> &'static [u8] {
        match self {
            Self::Soft => include_bytes!("dance_clips/soft.bin"),
            Self::Turn => include_bytes!("dance_clips/turn.bin"),
        }
    }

    pub fn travel(self) -> f32 {
        signed(self.data(), (FRAMES - 1) * FRAME_BYTES)
    }

    pub fn sample(self, phase: f32, direction: f32) -> Sample {
        let position = phase.clamp(0.0, 1.0) * (FRAMES - 1) as f32;
        let frame = position as usize;
        let next = (frame + 1).min(FRAMES - 1);
        let channel = |channel| {
            let before = signed(self.data(), frame * FRAME_BYTES + channel * 2);
            let after = signed(self.data(), next * FRAME_BYTES + channel * 2);
            before + (after - before) * position.fract()
        };
        let side = |side: usize| if direction < 0.0 { 1 - side } else { side };
        Sample {
            root: [channel(0) * direction, channel(1), channel(2)],
            yaw: channel(3) * direction,
            twist: channel(4) * direction,
            look: channel(5) * direction,
            lumbar: channel(6) * direction,
            pitch: channel(7),
            wrists: std::array::from_fn(|i| {
                let start = 8 + side(i) * 3;
                [
                    channel(start) * direction,
                    channel(start + 1),
                    channel(start + 2),
                ]
            }),
            foot_roll: std::array::from_fn(|i| channel(14 + side(i))),
            blend: ease((phase / 0.12).clamp(0.0, 1.0))
                * ease(((1.0 - phase) / 0.12).clamp(0.0, 1.0)),
        }
    }

    pub fn first_foot(self, direction: f32) -> usize {
        let first = usize::from(self.data()[FRAMES * FRAME_BYTES]);
        if direction < 0.0 { 1 - first } else { first }
    }

    pub fn step_between(self, previous: f32, phase: f32, direction: f32) -> Option<Step> {
        let (steps, _) = self.data()[FRAMES * FRAME_BYTES..].as_chunks::<STEP_BYTES>();
        steps.iter().find_map(|data| {
            let start = f32::from(u16::from_le_bytes([data[1], data[2]])) / 10000.0;
            if !(previous < start && start <= phase) {
                return None;
            }
            let side = usize::from(data[0]);
            Some(Step {
                side: if direction < 0.0 { 1 - side } else { side },
                start,
                end: f32::from(u16::from_le_bytes([data[3], data[4]])) / 10000.0,
                x: signed(data, 5) * direction,
                depth: signed(data, 7),
                yaw: signed(data, 9) * direction,
                lift: signed(data, 11),
            })
        })
    }
}

fn signed(data: &[u8], index: usize) -> f32 {
    f32::from(i16::from_le_bytes([data[index], data[index + 1]])) / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_data_has_ordered_contact_windows_and_mirrors_the_pose() {
        for clip in [Clip::Soft, Clip::Turn] {
            let data = clip.data();
            assert_eq!((data.len() - FRAMES * FRAME_BYTES) % STEP_BYTES, 0);
            let mut previous = 0.0;
            let mut steps = 0;
            while let Some(step) = clip.step_between(previous, 1.0, 1.0) {
                assert!(previous < step.start && step.start < step.end && step.end < 1.0);
                assert!(step.side < 2 && step.lift > 0.0);
                let mirrored = clip.step_between(previous, step.start, -1.0).unwrap();
                assert_eq!(mirrored.side, 1 - step.side);
                assert_eq!(mirrored.x, -step.x);
                previous = step.end;
                steps += 1;
            }
            assert!(steps >= 2);
            let pose = clip.sample(0.5, 1.0);
            let mirror = clip.sample(0.5, -1.0);
            assert_eq!(pose.root[0], -mirror.root[0]);
            assert_eq!(pose.wrists[0][0], -mirror.wrists[1][0]);
            assert_eq!(pose.wrists[0][1..], mirror.wrists[1][1..]);
            assert_eq!(clip.travel(), clip.sample(1.0, 1.0).root[0]);
        }
    }
}
