use crate::{
    choreography::{Choreography, Move, ease},
    dance_balance::Balance,
};
use std::{
    collections::hash_map::RandomState,
    f32::consts::{PI, TAU},
    hash::{BuildHasher, Hasher},
};

// Stage coordinates use approximately 1.7 metres per unit.
const GRAVITY: f32 = 9.81 / 1.7;
const ARM_REACH: f32 = 0.30996;

#[derive(Clone, Copy, Debug, Default)]
pub enum Style {
    #[default]
    Auto,
    Flow,
    Groove,
    Pulse,
    House,
    Hiphop,
    Rave,
}

impl Style {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "flow" => Some(Self::Flow),
            "groove" => Some(Self::Groove),
            "pulse" => Some(Self::Pulse),
            "house" => Some(Self::House),
            "hiphop" => Some(Self::Hiphop),
            "rave" => Some(Self::Rave),
            _ => None,
        }
    }

    fn weights(self, music: Features) -> [f32; 3] {
        match self {
            Self::Flow => [1.0, 0.0, 0.0],
            Self::Groove | Self::House => [0.0, 1.0, 0.0],
            Self::Hiphop => [0.0, 0.8, 0.2],
            Self::Pulse | Self::Rave => [0.0, 0.0, 1.0],
            Self::Auto => {
                let rhythm = music.confidence
                    * (0.45 + music.energy * 0.3 + music.bass * 0.25)
                    * if music.genre == Genre::Ambient {
                        0.15
                    } else {
                        1.0
                    };
                let speed = ease(((60.0 / music.period - 115.0) / 50.0).clamp(0.0, 1.0));
                let drive = ease(((music.energy + music.bass - 0.9) / 0.9).clamp(0.0, 1.0));
                let quick = speed + (1.0 - speed) * drive * 0.75;
                [1.0 - rhythm, rhythm * (1.0 - quick), rhythm * quick]
            }
        }
    }

    // Repertoire priors are separate from the rig's Flow/Groove/Pulse dynamics.
    fn repertoire(self, music: Features) -> [f32; 3] {
        match self {
            Self::House => [1.0, 0.0, 0.0],
            Self::Hiphop => [0.0, 1.0, 0.0],
            Self::Rave => [0.0, 0.0, 1.0],
            Self::Flow | Self::Groove | Self::Pulse => [0.0; 3],
            Self::Auto => {
                let active =
                    music.confidence * ease(((music.energy - 0.25) / 0.55).clamp(0.0, 1.0));
                if music.genre == Genre::Ambient {
                    return [0.0; 3];
                }
                let fast = ease(((60.0 / music.period.max(0.15) - 130.0) / 50.0).clamp(0.0, 1.0));
                let drive = ease(((music.energy + music.bass - 0.9) / 0.9).clamp(0.0, 1.0));
                let rave = fast * drive;
                let house = ease(((60.0 / music.period.max(0.15) - 95.0) / 30.0).clamp(0.0, 1.0));
                let mut mix = [(1.0 - rave) * house, (1.0 - rave) * (1.0 - house), rave];
                let prior = match music.genre {
                    Genre::House => Some(0),
                    Genre::Hiphop => Some(1),
                    Genre::Rave => Some(2),
                    Genre::Unknown | Genre::Ambient => None,
                };
                if let Some(index) = prior {
                    for weight in &mut mix {
                        *weight *= 0.4;
                    }
                    mix[index] += 0.6;
                }
                // Even a rave label needs audible drive before it gets forceful footwork.
                mix[2] *= drive;
                mix.map(|weight| weight * active)
            }
        }
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum Genre {
    #[default]
    Unknown,
    Ambient,
    House,
    Hiphop,
    Rave,
}

impl Genre {
    pub fn from_metadata(value: Option<&str>) -> Self {
        let value = value
            .unwrap_or_default()
            .to_lowercase()
            .replace(['-', '_'], " ");
        if ["ambient", "chill", "downtempo", "meditation"]
            .iter()
            .any(|tag| value.contains(tag))
        {
            Self::Ambient
        } else if [
            "hardcore",
            "gabber",
            "hardstyle",
            "rave",
            "techno",
            "drum and bass",
            "drum & bass",
            "dnb",
            "trance",
        ]
        .iter()
        .any(|tag| value.contains(tag))
        {
            Self::Rave
        } else if ["house", "disco", "funk"]
            .iter()
            .any(|tag| value.contains(tag))
        {
            Self::House
        } else if ["hip hop", "hiphop", "rap", "r&b", "rnb"]
            .iter()
            .any(|tag| value.contains(tag))
        {
            Self::Hiphop
        } else {
            Self::Unknown
        }
    }
}

#[derive(Clone, Copy)]
pub struct Features {
    pub energy: f32,
    pub bass: f32,
    pub accent: f32,
    pub treble: f32,
    pub period: f32,
    pub confidence: f32,
    pub hit: bool,
    pub genre: Genre,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            energy: 0.0,
            bass: 0.0,
            accent: 0.0,
            treble: 0.0,
            period: 0.5,
            confidence: 0.0,
            hit: false,
            genre: Genre::Unknown,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub fn new(x: f32, y: f32) -> Self {
        Self { x, y }
    }
    pub fn offset(self, x: f32, y: f32) -> Self {
        Self::new(self.x + x, self.y + y)
    }
    pub fn lerp(self, other: Self, t: f32) -> Self {
        self.offset((other.x - self.x) * t, (other.y - self.y) * t)
    }
    pub fn distance(self, other: Self) -> f32 {
        (self.x - other.x).hypot(self.y - other.y)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pose {
    pub head: Point,
    pub neck: Point,
    pub waist: Point,
    pub shoulders: [Point; 2],
    pub elbows: [Point; 2],
    pub wrists: [Point; 2],
    pub arm_depth: [[f32; 2]; 2],
    pub hips: [Point; 2],
    pub knees: [Point; 2],
    pub ankles: [Point; 2],
    pub toes: [Point; 2],
    pub tail: [Point; 4],
}

#[derive(Default)]
struct Spring {
    position: f32,
    velocity: f32,
}

#[derive(Default)]
struct Arm {
    elevation: Spring,
    flexion: Spring,
    depth: [Spring; 2],
}

impl Arm {
    fn targets(start: Point, end: Point, torso: f32, side: f32) -> (f32, f32) {
        let x = end.x - start.x;
        let y = end.y - start.y;
        let outward = side * (x * torso.cos() + y * torso.sin());
        let down = -x * torso.sin() + y * torso.cos();
        let distance = x.hypot(y).clamp(0.1, ARM_REACH);
        let mut bearing = outward.atan2(down);
        // Keep the actual overhead bearing across atan2's seam. Folding is a
        // projection choice; forcing the other elbow branch splays the shoulders.
        if bearing < -PI / 2.0 {
            bearing += TAU;
        }
        let offset = ((0.16_f32.powi(2) + distance.powi(2) - 0.15_f32.powi(2))
            / (2.0 * 0.16 * distance))
            .clamp(-1.0, 1.0)
            .acos();
        let flexion = ((distance.powi(2) - 0.16_f32.powi(2) - 0.15_f32.powi(2))
            / (2.0 * 0.16 * 0.15))
            .clamp(-1.0, 1.0)
            .acos();
        (
            (bearing - offset).clamp(-1.25, 2.85),
            flexion.clamp(0.025, 2.5),
        )
    }

    fn joints(&self, shoulder: Point, torso: f32, side: f32) -> (Point, Point) {
        let direction = |angle: f32, length: f32| {
            let x = side * angle.sin() * length;
            let y = angle.cos() * length;
            Point::new(
                x * torso.cos() - y * torso.sin(),
                x * torso.sin() + y * torso.cos(),
            )
        };
        let upper = direction(self.elevation.position, 0.16);
        let lower = direction(self.elevation.position + self.flexion.position, 0.15);
        let elbow = shoulder.offset(upper.x, upper.y);
        (elbow, elbow.offset(lower.x, lower.y))
    }
}

impl Spring {
    fn advance(&mut self, target: f32, omega: f32, dt: f32) {
        (self.position, self.velocity) =
            crate::spring::advance(self.position, self.velocity, target, omega, dt);
    }

    fn push(&mut self, target: f32, omega: f32, dt: f32) {
        let steps = (dt * 240.0).ceil().max(1.0) as usize;
        let step = dt / steps as f32;
        for _ in 0..steps {
            let force = omega * omega * (target - self.position) - 2.0 * omega * self.velocity;
            self.velocity += force.clamp(-GRAVITY * 1.2, GRAVITY * 1.2) * step;
            self.position += self.velocity * step;
        }
    }

    fn rotate(&mut self, target: f32, omega: f32, dt: f32, limits: (f32, f32), speed: f32) {
        let previous = self.position;
        self.advance(target.clamp(limits.0, limits.1), omega, dt);
        let position = self
            .position
            .clamp(previous - speed * dt, previous + speed * dt)
            .clamp(limits.0, limits.1);
        if position != self.position {
            self.velocity = (position - previous) / dt;
        }
        if (position == limits.0 && self.velocity < 0.0)
            || (position == limits.1 && self.velocity > 0.0)
        {
            self.velocity = 0.0;
        }
        self.position = position;
    }
}

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
enum Footwork {
    #[default]
    Grounded,
    Slide,
    Skim,
    Lift,
}

impl Footwork {
    fn choose(movement: Move, energy: f32, variation: f32) -> Self {
        match movement {
            Move::Shuffle | Move::Stomp | Move::Hakken => Self::Lift,
            Move::Crouch | Move::Hop => Self::Grounded,
            Move::Travel => match variation {
                v if v < 0.5 => Self::Slide,
                v if v < 0.85 => Self::Skim,
                _ => Self::Lift,
            },
            _ => {
                let grounded = if matches!(movement, Move::Jack | Move::Rock | Move::Punch) {
                    0.75
                } else {
                    0.45 + 0.25 * (1.0 - energy)
                };
                if variation < grounded {
                    Self::Grounded
                } else if variation < grounded + (1.0 - grounded) * 0.7 {
                    Self::Slide
                } else {
                    Self::Skim
                }
            }
        }
    }
}

struct Swing {
    foot: usize,
    start: Point,
    target: Point,
    lift: f32,
    stomp: bool,
    footwork: Footwork,
    excursion: f32,
}

impl Swing {
    fn path_progress(&self, progress: f32) -> f32 {
        if self.footwork == Footwork::Slide {
            // Unload before dragging, then settle before returning the weight.
            ((progress - 0.15) / 0.7).clamp(0.0, 1.0)
        } else {
            progress
        }
    }

    fn arc(&self, progress: f32) -> f32 {
        if self.stomp {
            ease((progress / 0.7).min(1.0)) * (1.0 - ease(((progress - 0.7) / 0.3).clamp(0.0, 1.0)))
        } else {
            (PI * progress).sin().powi(2)
        }
    }

    fn position(&self, progress: f32) -> Point {
        let path = self.path_progress(progress);
        if path <= 0.0 {
            return self.start;
        }
        if path >= 1.0 {
            return self.target;
        }
        let position = self.start.lerp(self.target, ease(path)).offset(
            self.excursion * (PI * path).sin().powi(2),
            -self.lift * self.arc(progress),
        );
        if self.footwork != Footwork::Slide && position.y == 0.97 {
            if progress < 0.5 {
                self.start
            } else {
                self.target
            }
        } else {
            position
        }
    }
}

struct Jump {
    elapsed: f32,
    duration: f32,
    height: f32,
    preparation: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Transition {
    Following,
    Finishing,
    Blending(f32),
}

impl Jump {
    fn new(height: f32, period: f32) -> Self {
        let preparation = (period * 0.45).clamp(0.16, 0.35);
        Self {
            elapsed: 0.0,
            duration: preparation * 2.1 + 2.0 * (2.0 * height / GRAVITY).sqrt(),
            height,
            preparation,
        }
    }

    fn flight_time(&self) -> f32 {
        2.0 * (2.0 * self.height / GRAVITY).sqrt()
    }

    fn vertical(&self) -> (f32, f32) {
        if self.elapsed >= self.duration {
            return (0.0, 0.0);
        }
        let elapsed = self.elapsed - self.preparation;
        let flight_time = self.flight_time();
        if elapsed < 0.0 {
            (0.09 * ease(self.elapsed / self.preparation), 0.0)
        } else if elapsed < flight_time {
            let flight = elapsed / flight_time;
            (
                0.025 + 0.065 * (flight * 2.0 - 1.0).powi(2),
                ((2.0 * GRAVITY * self.height).sqrt() * elapsed
                    - 0.5 * GRAVITY * elapsed * elapsed)
                    .max(0.0),
            )
        } else {
            (
                0.09 * (1.0 - ease(((elapsed - flight_time) / (self.preparation * 1.1)).min(1.0))),
                0.0,
            )
        }
    }
}

pub struct Motion {
    style: Style,
    weights: [f32; 3],
    repertoire: [f32; 3],
    phrase: f32,
    step_phase: f32,
    step_index: u64,
    step_planned: bool,
    footwork: Footwork,
    feet: [Point; 2],
    swing: Option<Swing>,
    root: Spring,
    chest: Spring,
    head: Spring,
    hands: [Arm; 2],
    choreography: Choreography,
    jump: Option<Jump>,
    crouch: Spring,
    space: f32,
    transition: Transition,
    cadence: Spring,
    bounce: Spring,
    jack: Spring,
    shoulder_roll: Spring,
    balance: Balance,
    impulse_direction: f32,
    pub pose: Pose,
    pub spatial: crate::dance_space::Rig,
}

impl Motion {
    pub fn new(style: Style) -> Self {
        Self::with_seed(style, RandomState::new().build_hasher().finish())
    }

    fn with_seed(style: Style, seed: u64) -> Self {
        let mut motion = Self {
            style,
            weights: style.weights(Features::default()),
            repertoire: style.repertoire(Features::default()),
            phrase: 0.0,
            step_phase: 0.0,
            step_index: 0,
            step_planned: false,
            footwork: Footwork::Grounded,
            feet: [Point::new(-0.065, 0.97), Point::new(0.065, 0.97)],
            swing: None,
            root: Spring::default(),
            chest: Spring::default(),
            head: Spring::default(),
            hands: std::array::from_fn(|_| Arm::default()),
            choreography: Choreography::new(seed),
            jump: None,
            crouch: Spring::default(),
            space: 1.5,
            transition: Transition::Following,
            bounce: Spring::default(),
            jack: Spring::default(),
            shoulder_roll: Spring::default(),
            balance: Balance::default(),
            impulse_direction: 1.0,
            cadence: Spring {
                position: match style {
                    Style::Flow | Style::Auto => 1.8,
                    Style::Groove | Style::House | Style::Hiphop => 0.5,
                    Style::Pulse | Style::Rave => 1.0,
                },
                velocity: 0.0,
            },
            pose: Pose::default(),
            spatial: crate::dance_space::Rig::default(),
        };
        motion.pose = motion.make_pose(Features::default());
        let torso = motion.torso_angle();
        for (index, hand) in motion.hands.iter_mut().enumerate() {
            let side = if index == 0 { -1.0 } else { 1.0 };
            let (elevation, flexion) = Arm::targets(
                motion.pose.shoulders[index],
                motion.pose.wrists[index],
                torso,
                side,
            );
            hand.elevation.position = elevation;
            hand.flexion.position = flexion;
            for depth in &mut hand.depth {
                depth.position = 1.0;
            }
            (motion.pose.elbows[index], motion.pose.wrists[index]) =
                hand.joints(motion.pose.shoulders[index], torso, side);
        }
        motion.spatial = crate::dance_space::Rig::new(&motion.pose);
        motion
    }

    pub fn set_space(&mut self, columns: u16) {
        self.space = (f32::from(columns.saturating_sub(6)) / 35.2 - 0.45).clamp(0.0, 2.4);
    }

    pub fn begin_transition(&mut self) {
        if self.transition == Transition::Following {
            self.transition = Transition::Finishing;
            self.choreography.finish_phrase();
        }
    }

    pub fn transitioning(&self) -> bool {
        self.transition != Transition::Following
    }

    pub fn advance(&mut self, music: Features, dt: f32) {
        if dt <= 0.0 {
            return;
        }
        let target = self.style.weights(music);
        let blend = 1.0 - (-dt / 3.0).exp();
        for (weight, target) in self.weights.iter_mut().zip(target) {
            *weight += (target - *weight) * blend;
        }
        for (weight, target) in self.repertoire.iter_mut().zip(self.style.repertoire(music)) {
            *weight += (target - *weight) * blend;
        }
        let [flow, groove, pulse] = self.weights;
        let rhythm = groove + pulse;
        let planted = self.swing.is_none() && self.jump.is_none();
        self.transition = match self.transition {
            Transition::Finishing
                if planted && self.crouch.position < 0.015 && !self.balance.recovering() =>
            {
                self.choreography
                    .bridge((self.feet[0].x + self.feet[1].x) / 2.0);
                Transition::Blending(0.0)
            }
            Transition::Blending(elapsed) if elapsed + dt >= 1.5 => Transition::Following,
            Transition::Blending(elapsed) => Transition::Blending(elapsed + dt),
            other => other,
        };
        let following = self.transition == Transition::Following;
        if music.hit && following && rhythm > 0.25 {
            let strength =
                (music.energy * 0.55 + music.bass * 0.45) * (0.6 + music.accent * 0.4) * rhythm;
            self.bounce.velocity += strength * (0.7 + pulse * 0.9);
            match self.choreography.phrase.movement {
                Move::Jack => self.jack.velocity += strength * 3.5,
                Move::Stomp => self.bounce.velocity += strength * 0.55,
                _ => self.jack.velocity += strength * groove * 0.7,
            }
            let variation = self.choreography.random();
            if self.choreography.random() < 0.65 {
                self.impulse_direction = -self.impulse_direction;
            }
            let vigorous =
                music.energy > 0.8 && music.bass > 0.55 && music.confidence > 0.6 && pulse > 0.35;
            let impulse =
                if vigorous && planted && variation < 0.012 && self.balance.can_overbalance() {
                    9.0 + variation * 80.0
                } else if vigorous && variation < 0.08 {
                    3.4
                } else {
                    0.65 + variation * 0.6
                };
            self.balance
                .kick(self.impulse_direction * strength * impulse);
        }
        self.bounce.rotate(0.0, 11.0, dt, (0.0, 0.14), 1.1);
        self.jack.rotate(0.0, 9.0, dt, (0.0, 0.45), 3.5);
        let recovering = self.balance.recovering();
        self.balance.advance(dt, planted);
        if recovering && !self.balance.recovering() {
            self.choreography
                .bridge((self.feet[0].x + self.feet[1].x) / 2.0);
        }
        self.choreography.repertoire = self.repertoire;
        self.choreography.advance(
            music,
            self.weights,
            (self.feet[0].x + self.feet[1].x) / 2.0,
            self.space,
            planted && following && !self.balance.recovering(),
            dt,
        );
        // Sustained gestures use a free phrase clock; rhythmic phrases span eight beats.
        self.phrase = (self.phrase
            + dt * TAU
                * (flow / 12.0 + rhythm / (music.period * 8.0))
                * (0.35 + music.energy * 0.65)
                * self.choreography.phrase.pace)
            % TAU;
        // Energetic profiles take quicker steps; sustained Flow keeps its own cadence.
        self.cadence.advance(
            flow * 1.8 + rhythm * music.period * (1.0 + pulse * (0.45 - music.energy * 0.4)),
            3.0,
            dt,
        );
        // Fast music changes stride and lift; contacts cannot be compressed without limit.
        let step_period = self.cadence.position.max(0.3);
        self.step_phase += dt / step_period;
        if self.step_phase >= 1.0 {
            if let Some(swing) = self.swing.take() {
                self.feet[swing.foot] = swing.target;
            }
            self.step_phase %= 1.0;
            self.step_index = (self.step_index + 1) % 2;
            self.step_planned = false;
        }
        let movement = self.choreography.phrase.movement;
        if movement == Move::Hop
            && following
            && !self.choreography.phrase.performed
            && self.jump.is_none()
            && self.swing.is_none()
            && !self.balance.recovering()
            && music.energy > 0.55
            && rhythm > 0.35
            && self.root.velocity.abs() < 0.05
            && (self.root.position - (self.feet[0].x + self.feet[1].x) / 2.0).abs() < 0.01
            && self
                .feet
                .iter()
                .all(|foot| foot.x.abs() < self.space + 0.015)
        {
            self.jump = Some(Jump::new(
                (0.1 + music.bass * 0.08 + music.energy * 0.03).min(0.22),
                music.period,
            ));
            self.choreography.phrase.performed = true;
        }
        let mut airborne_dt = 0.0;
        if let Some(jump) = &mut self.jump {
            let previous = jump.elapsed;
            jump.elapsed += dt;
            airborne_dt = (jump.preparation + jump.flight_time() - previous).clamp(0.0, dt)
                - (jump.preparation - previous).clamp(0.0, dt);
            if jump.elapsed >= jump.duration {
                self.jump = None;
            }
        }
        if let Some(direction) = self.balance.catch_direction()
            && self.swing.is_none()
            && self.jump.is_none()
        {
            let foot = usize::from(direction > 0.0);
            let target = self.foot_target(foot, self.feet[foot].x + direction * 0.2);
            self.step_phase = 0.12;
            self.swing = Some(Swing {
                foot,
                start: self.feet[foot],
                target: Point::new(target, 0.97),
                lift: 0.06,
                stomp: false,
                footwork: Footwork::Lift,
                excursion: 0.0,
            });
            self.balance.step_started();
        }
        if self.swing.is_none()
            && following
            && !self.balance.recovering()
            && self.jump.is_none()
            && !matches!(movement, Move::Crouch | Move::Hop)
            // Do not resume by entering a partly completed contact trajectory.
            && (0.12..0.12 + dt / step_period).contains(&self.step_phase)
            && (rhythm > 0.25 || movement == Move::Travel)
            && music.energy > 0.08
            && !self.step_planned
        {
            self.step_planned = true;
            self.footwork = Footwork::choose(movement, music.energy, self.choreography.random());
            if self.footwork != Footwork::Grounded {
                if movement == Move::Travel && !self.choreography.phrase.performed {
                    self.step_index = u64::from(
                        self.choreography.phrase.destination > self.choreography.phrase.start,
                    );
                    self.choreography.phrase.performed = true;
                }
                let foot = self.step_index as usize % 2;
                let side = if foot == 0 { -1.0 } else { 1.0 };
                let travel = self.choreography.center(step_period * 0.6);
                let spread = match movement {
                    Move::Shuffle => 0.11,
                    Move::Stomp => 0.09,
                    Move::Hakken => 0.055 + 0.02 * (self.phrase * 2.0).sin(),
                    _ => 0.065,
                };
                let stride = 0.32 * (step_period / 0.35).min(1.0);
                let target = self
                    .foot_target(foot, travel + side * spread)
                    .clamp(self.feet[foot].x - stride, self.feet[foot].x + stride);
                let margin = if foot == 0 {
                    self.feet[foot].x.min(target) + self.space + 0.065
                } else {
                    self.space + 0.065 - self.feet[foot].x.max(target)
                };
                self.swing = Some(Swing {
                    foot,
                    start: self.feet[foot],
                    target: Point::new(target, 0.97),
                    lift: (match self.footwork {
                        Footwork::Grounded | Footwork::Slide => 0.0,
                        Footwork::Skim => 0.008 + 0.014 * music.energy,
                        Footwork::Lift => {
                            (0.01 * flow + 0.04 * groove + 0.085 * pulse) * music.energy
                                + music.bass * rhythm * 0.008
                                + match movement {
                                    Move::Shuffle => 0.085,
                                    Move::Stomp => 0.045,
                                    Move::Hakken => 0.06,
                                    _ => 0.0,
                                } * music.energy
                        }
                    })
                    .min(6.0 * GRAVITY * (step_period * 0.6 / PI).powi(2)),
                    stomp: movement == Move::Stomp,
                    footwork: self.footwork,
                    excursion: if self.footwork == Footwork::Slide && movement != Move::Travel {
                        side * (0.06 * music.energy).min(margin.max(0.0))
                    } else {
                        0.0
                    },
                });
            }
        }
        let mut support = (self.feet[0].x + self.feet[1].x) / 2.0;
        if let Some(swing) = &self.swing {
            let progress = ((self.step_phase - 0.12) / 0.6).clamp(0.0, 1.0);
            let arc = swing.arc(progress);
            self.feet[swing.foot] = swing.position(progress);
            let center = (self.feet[0].x + self.feet[1].x) / 2.0;
            let travel = (swing.target.x - swing.start.x).abs();
            let transfer = 0.85 * (1.0 - (travel / 0.2).min(1.0));
            support = center + (self.feet[1 - swing.foot].x - center) * arc * transfer;
        } else if following && self.footwork == Footwork::Grounded && self.jump.is_none() {
            let transfer = ((self.step_phase + self.step_index as f32) * PI).sin();
            let width = (self.feet[1].x - self.feet[0].x).abs();
            support += transfer * width.min(0.13) * 0.3 * rhythm * music.energy;
        }
        let sway = (self.phrase * 2.0).sin() * (0.045 * flow + 0.012 * rhythm) * music.energy;
        let velocity = self.root.velocity;
        self.root
            .push(support + sway, 7.0 + pulse * 3.0, dt - airborne_dt);
        let drift = self.root.velocity * airborne_dt;
        self.root.position += drift;
        for foot in &mut self.feet {
            foot.x += drift;
        }
        let acceleration = (self.root.velocity - velocity) / dt;
        let envelope = self.choreography.envelope();
        let dip = if movement == Move::Crouch {
            (0.09 + music.bass * 0.05) * envelope
        } else {
            0.0
        };
        self.crouch.advance(
            dip + self.jump.as_ref().map_or(0.0, |jump| jump.vertical().0)
                + self.balance.brace() * 0.28,
            9.0,
            dt,
        );
        // Relative joint angles carry the body with the pelvis. Acceleration causes
        // elastic lag, then an active lean into the push; braking reverses that lean.
        let rock = if movement == Move::Rock {
            ((self.step_phase + self.step_index as f32) * PI).sin() * 0.12 * music.energy * envelope
        } else {
            0.0
        };
        let lean = (self.phrase - 0.25).sin() * 0.09 * music.energy
            + rock
            + (acceleration / GRAVITY).atan() * 0.35
            + self.crouch.position * 0.8;
        self.chest.velocity -= acceleration * dt * 0.12;
        self.chest.advance(lean, 6.0 + pulse * 2.0, dt);
        self.head
            .advance(-self.chest.position * 0.2, 5.0 + pulse * 2.0, dt);
        if self
            .jump
            .as_ref()
            .is_none_or(|jump| jump.vertical().1 == 0.0)
        {
            let capture = self.root.position
                + self.torso_angle().sin() * 0.18
                + self.root.velocity / (GRAVITY / 0.6).sqrt();
            let mut left = self.feet[0].x.min(self.feet[1].x);
            let mut right = self.feet[0].x.max(self.feet[1].x);
            // A walking body may move beyond the stance foot if the next contact
            // can catch it. Only momentum beyond that planned support is unsafe.
            if let Some(swing) = &self.swing {
                left = left.min(swing.target.x);
                right = right.max(swing.target.x);
            }
            let error = capture - capture.clamp(left - 0.055, right + 0.055);
            self.balance.observe_support(error, dt);
        } else {
            self.balance.observe_support(0.0, dt);
        }
        self.shoulder_roll.advance(
            self.bounce.position
                * rhythm
                * 0.5
                * ((self.step_phase + self.step_index as f32) * PI).cos(),
            12.0,
            dt,
        );
        let mut pose = self.make_pose(music);
        let torso = self.torso_angle();
        for (index, hand) in self.hands.iter_mut().enumerate() {
            let side = if index == 0 { -1.0 } else { 1.0 };
            let (elevation, mut flexion) =
                Arm::targets(pose.shoulders[index], pose.wrists[index], torso, side);
            if (elevation - hand.elevation.position).abs() > 1.8 {
                flexion = flexion.max(1.7);
            }
            hand.elevation
                .rotate(elevation, 7.0 + pulse * 3.0, dt, (-1.25, 2.85), 4.0);
            hand.flexion
                .rotate(flexion, 7.0 + pulse * 3.0, dt, (0.025, 2.5), 5.0);
            (pose.elbows[index], pose.wrists[index]) =
                hand.joints(pose.shoulders[index], torso, side);
            let chest = pose.shoulders[0].lerp(pose.shoulders[1], 0.5);
            let elbow = pose.elbows[index];
            let outward =
                side * ((elbow.x - chest.x) * torso.cos() + (elbow.y - chest.y) * torso.sin());
            let crossing = ease(((0.025 - outward) / 0.05).clamp(0.0, 1.0));
            pose.arm_depth[index][0] -= crossing * (1.0 - self.balance.brace());
            for (depth, target) in hand.depth.iter_mut().zip(&mut pose.arm_depth[index]) {
                depth.advance(target.clamp(-1.0, 1.0), 6.0, dt);
                *target = depth.position;
            }
        }
        self.pose = pose;
        self.spatial.advance(
            &pose,
            crate::dance_space::Cue {
                energy: music.energy,
                travel: if movement == Move::Travel {
                    self.root.velocity / 0.35
                } else {
                    0.0
                },
                brace: self.balance.brace(),
                airborne: self
                    .jump
                    .as_ref()
                    .is_some_and(|jump| jump.vertical().1 > 0.0),
                swing: self.swing.as_ref().map(|swing| {
                    (
                        swing.foot,
                        swing.path_progress(((self.step_phase - 0.12) / 0.6).clamp(0.0, 1.0)),
                    )
                }),
                sliding: self.sliding_foot().is_some(),
            },
            dt,
        );
    }

    fn sliding_foot(&self) -> Option<usize> {
        self.swing
            .as_ref()
            .filter(|swing| swing.footwork == Footwork::Slide)
            .map(|swing| swing.foot)
    }

    fn foot_target(&self, foot: usize, target: f32) -> f32 {
        // A corrective step also needs a reachable stance for both legs.
        target
            .clamp(-self.space - 0.065, self.space + 0.065)
            .clamp(self.feet[1 - foot].x - 0.45, self.feet[1 - foot].x + 0.45)
    }

    fn torso_angle(&self) -> f32 {
        self.balance.angle
            + (self.jack.position + self.chest.position) * (1.0 - self.balance.brace())
    }

    fn make_pose(&self, music: Features) -> Pose {
        let [flow, groove, pulse] = self.weights;
        let rhythm = groove + pulse;
        let bounce = self.bounce.position
            + (1.0 - (self.step_phase * TAU).cos())
                * (0.006 * groove + 0.012 * pulse)
                * music.energy
            + music.accent * pulse * 0.008;
        let horizontal = self
            .feet
            .iter()
            .enumerate()
            .map(|(i, foot)| {
                (self.root.position + if i == 0 { -0.044 } else { 0.044 } - foot.x).abs()
            })
            .fold(0.0_f32, f32::max);
        let altitude = self.jump.as_ref().map_or(0.0, |jump| jump.vertical().1);
        let hip_tilt = (self.phrase * 2.0 - 0.15).sin() * music.energy * 0.009;
        let extension = 0.449 - hip_tilt.abs() * 0.3;
        // Crouching and fall recovery can overlap; keep the pelvis clear of the floor.
        let hip_y = (0.97 - (extension.powi(2) - horizontal.powi(2)).max(0.0).sqrt()
            + bounce
            + self.crouch.position
            - altitude)
            .min(0.93 - altitude);
        let hip = Point::new(self.root.position, hip_y);
        let tilt = hip_tilt + self.shoulder_roll.position;
        let angle = self.torso_angle();
        let rotate = |x: f32, y: f32| {
            Point::new(
                x * angle.cos() - y * angle.sin(),
                x * angle.sin() + y * angle.cos(),
            )
        };
        let shoulder = |x: f32, y: f32| {
            let p = rotate(x, y);
            Point::new(
                hip.x + angle.sin() * 0.27 + p.x,
                hip.y - angle.cos() * 0.27 + p.y,
            )
        };
        let chest = shoulder(0.0, 0.0);
        let neck = shoulder(0.0, -0.022);
        let head_angle = angle + self.head.position;
        let head = chest.offset(head_angle.sin() * 0.102, -head_angle.cos() * 0.102);
        let mut pose = Pose {
            head,
            neck,
            waist: hip.lerp(chest, 0.36),
            shoulders: [shoulder(-0.075, tilt), shoulder(0.075, -tilt)],
            arm_depth: [[1.0; 2]; 2],
            hips: [
                hip.offset(-0.044, -hip_tilt * 0.3),
                hip.offset(0.044, hip_tilt * 0.3),
            ],
            ankles: self.feet.map(|foot| foot.offset(0.0, -altitude)),
            ..Pose::default()
        };
        for (index, side) in [-1.0, 1.0].into_iter().enumerate() {
            let phrase = &self.choreography.phrase;
            let beat = self.phrase / TAU * 8.0 + phrase.gesture_offset;
            let delay = if index == 0 { 0.15 } else { phrase.hand_delay };
            let flow_hand = gesture(&FLOW, beat - delay);
            let groove_hand = gesture(&GROOVE, beat - delay + 1.0);
            let pulse_hand = gesture(&PULSE, beat - delay + 0.5);
            let mut depth = gesture(&ARM_DEPTH, beat - delay);
            let mut hand = Point::new(
                flow_hand.x * flow + groove_hand.x * groove + pulse_hand.x * pulse,
                flow_hand.y * flow + groove_hand.y * groove + pulse_hand.y * pulse,
            );
            let envelope = self.choreography.envelope();
            hand.x *= phrase.reach;
            match phrase.movement {
                Move::Reach => hand.y -= 0.075 * envelope,
                Move::Sweep => hand.x -= 0.15 * envelope * if index == 0 { 1.0 } else { -0.5 },
                Move::Travel => {
                    hand.x *= 0.8;
                    hand.y += 0.045 * envelope;
                }
                Move::Crouch => hand.y += 0.08 * envelope,
                Move::Hop => hand.y -= altitude * 0.6,
                Move::Jack | Move::Shuffle | Move::Stomp | Move::Hakken | Move::Punch => {
                    let keys = match phrase.movement {
                        Move::Hakken => &HAKKEN,
                        Move::Punch => &PUNCH,
                        _ => &PUMP,
                    };
                    let pump_phase = (self.step_phase + self.step_index as f32) * 4.0
                        + if index == 0 { 0.0 } else { 4.0 };
                    let pump = gesture(keys, pump_phase);
                    hand = hand.lerp(pump, envelope);
                    let pump_depth = if phrase.movement == Move::Punch {
                        Point::new(0.5, 0.85)
                    } else {
                        gesture(&ARM_DEPTH, pump_phase)
                    };
                    depth = depth.lerp(pump_depth, envelope);
                    if phrase.movement == Move::Jack {
                        hand.y -= self.jack.position * 0.5;
                    }
                }
                Move::Rock => {
                    let rock = gesture(
                        &ROCK,
                        (self.step_phase + self.step_index as f32) * 4.0
                            + if index == 0 { 0.0 } else { 4.0 },
                    );
                    hand = hand.lerp(rock, envelope);
                }
                Move::Sway => {}
            }
            let depth = Point::new(1.0, 1.0)
                .lerp(depth, music.energy.sqrt())
                .lerp(Point::new(1.0, 1.0), self.balance.brace());
            pose.arm_depth[index] = [depth.x, depth.y];
            let rest = pose.shoulders[index].offset(side * 0.035, 0.295);
            let target = chest.offset(side * hand.x, hand.y).offset(
                side * music.treble * rhythm * 0.008,
                -music.accent * pulse * 0.012,
            );
            let brace = Point::new(pose.shoulders[index].x + angle.sin() * 0.08, 0.96);
            let mut goal = rest.lerp(target, music.energy.sqrt());
            {
                let (gather, span) = if phrase.movement == Move::Reach {
                    (0.08, 0.12)
                } else {
                    (0.14, 0.09)
                };
                let stretch = envelope
                    * ease(((hand.x - gather) / span).clamp(0.0, 1.0))
                    * ((music.energy - 0.2) / 0.45).clamp(0.0, 1.0);
                let distance = pose.shoulders[index].distance(goal);
                if distance > 0.0001 {
                    let length = distance + (ARM_REACH - distance).max(0.0) * stretch;
                    goal = pose.shoulders[index].lerp(goal, length / distance);
                }
            }
            pose.wrists[index] = reachable(
                pose.shoulders[index],
                goal.lerp(brace, self.balance.brace()),
                ARM_REACH,
            );
            // Both knees bend forward in this projection, avoiding a mirrored squat.
            pose.knees[index] = joint(pose.hips[index], pose.ankles[index], 0.22, 0.23, 1.0);
            pose.toes[index] = pose.ankles[index].offset(0.042, 0.012);
            if phrase.movement == Move::Hakken {
                // Heel-toe accents in the frontal projection; the ankle contact stays fixed.
                pose.toes[index].x +=
                    side * 0.024 * envelope * ((self.step_phase + index as f32) * PI).sin();
            }
        }
        let hair = (self.phrase - 0.8).sin() * music.energy * 0.024;
        pose.tail = [
            head.offset(0.023, -0.035),
            head.offset(0.065, -0.015),
            head.offset(0.07 + hair, 0.028),
            head.offset(0.076 + hair, 0.075),
        ];
        pose
    }
}

fn reachable(start: Point, end: Point, length: f32) -> Point {
    let distance = start.distance(end);
    if distance < 0.0001 {
        return start.offset(0.0, 0.03);
    }
    start.lerp(end, distance.clamp(0.03, length) / distance)
}

fn joint(start: Point, end: Point, upper: f32, lower: f32, bend: f32) -> Point {
    let distance = start.distance(end).clamp(0.001, upper + lower - 0.0001);
    let along = (upper * upper - lower * lower + distance * distance) / (2.0 * distance);
    let height = (upper * upper - along * along).max(0.0).sqrt();
    start.lerp(end, along / distance).offset(
        bend * (end.y - start.y) / distance * height,
        -bend * (end.x - start.x) / distance * height,
    )
}

// Original gesture paths: unfold, reach, suspend, gather. These describe targets,
// not joint rotations, so style blending retains fixed arm lengths through IK.
const FLOW: [Point; 8] = [
    Point { x: 0.10, y: 0.24 },
    Point { x: 0.20, y: 0.18 },
    Point { x: 0.28, y: 0.03 },
    Point { x: 0.22, y: -0.12 },
    Point { x: 0.09, y: -0.19 },
    Point { x: -0.04, y: -0.1 },
    Point { x: -0.07, y: 0.1 },
    Point { x: 0.02, y: 0.23 },
];
const GROOVE: [Point; 8] = [
    Point { x: 0.13, y: 0.21 },
    Point { x: 0.22, y: 0.13 },
    Point { x: 0.26, y: 0.06 },
    Point { x: 0.16, y: 0.06 },
    Point { x: 0.02, y: 0.1 },
    Point { x: -0.04, y: 0.16 },
    Point { x: 0.06, y: 0.23 },
    Point { x: 0.14, y: 0.22 },
];
const PULSE: [Point; 8] = [
    Point { x: 0.14, y: 0.17 },
    Point { x: 0.24, y: 0.1 },
    Point { x: 0.26, y: -0.04 },
    Point { x: 0.21, y: 0.02 },
    Point { x: 0.05, y: 0.1 },
    Point { x: -0.1, y: 0.12 },
    Point { x: 0.02, y: 0.18 },
    Point { x: 0.15, y: 0.22 },
];

const PUMP: [Point; 8] = [
    Point { x: 0.12, y: 0.21 },
    Point { x: 0.18, y: 0.1 },
    Point { x: 0.23, y: -0.17 },
    Point { x: 0.16, y: -0.08 },
    Point { x: 0.05, y: 0.14 },
    Point { x: -0.025, y: 0.22 },
    Point { x: 0.025, y: 0.25 },
    Point { x: 0.08, y: 0.23 },
];

// Compact counterposed arm work for rapid footwork, alternating strikes, and rocks.
const HAKKEN: [Point; 8] = [
    Point { x: 0.06, y: 0.18 },
    Point { x: 0.10, y: 0.10 },
    Point { x: 0.20, y: 0.03 },
    Point { x: 0.12, y: 0.06 },
    Point { x: 0.02, y: 0.13 },
    Point { x: 0.06, y: 0.19 },
    Point { x: 0.17, y: 0.23 },
    Point { x: 0.11, y: 0.21 },
];
const PUNCH: [Point; 8] = [
    Point { x: 0.04, y: 0.02 },
    Point { x: 0.09, y: -0.01 },
    Point { x: 0.30, y: -0.12 },
    Point { x: 0.23, y: -0.10 },
    Point { x: 0.06, y: 0.01 },
    Point { x: 0.02, y: 0.10 },
    Point { x: 0.08, y: 0.15 },
    Point { x: 0.05, y: 0.08 },
];
const ROCK: [Point; 8] = [
    Point { x: 0.11, y: 0.18 },
    Point { x: 0.18, y: 0.15 },
    Point { x: 0.25, y: 0.12 },
    Point { x: 0.17, y: 0.10 },
    Point { x: 0.03, y: 0.12 },
    Point { x: -0.03, y: 0.15 },
    Point { x: 0.04, y: 0.21 },
    Point { x: 0.12, y: 0.22 },
];

// Relative drawing depth at the elbow and wrist: positive is in front of the body.
const ARM_DEPTH: [Point; 8] = [
    Point { x: -0.45, y: -0.65 },
    Point { x: -0.2, y: 0.2 },
    Point { x: 0.15, y: 0.6 },
    Point { x: 0.4, y: 0.75 },
    Point { x: 0.2, y: 0.65 },
    Point { x: -0.15, y: 0.75 },
    Point { x: -0.45, y: 0.6 },
    Point { x: -0.65, y: -0.15 },
];

fn gesture(keys: &[Point; 8], phase: f32) -> Point {
    let phase = phase.rem_euclid(8.0);
    let index = phase.floor() as usize % 8;
    let t = phase.fract();
    let [a, b, c, d] = [
        keys[(index + 7) % 8],
        keys[index],
        keys[(index + 1) % 8],
        keys[(index + 2) % 8],
    ];
    let axis = |a: f32, b: f32, c: f32, d: f32| {
        0.5 * (2.0 * b
            + (c - a) * t
            + (2.0 * a - 5.0 * b + 4.0 * c - d) * t * t
            + (3.0 * b - a - 3.0 * c + d) * t * t * t)
    };
    Point::new(axis(a.x, b.x, c.x, d.x), axis(a.y, b.y, c.y, d.y))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_repertoire_uses_genre_as_a_prior_and_requires_audible_rhythm() {
        let music = Features {
            energy: 0.95,
            bass: 0.9,
            confidence: 1.0,
            period: 0.35,
            ..Features::default()
        };
        let auto = Style::Auto.repertoire(music);
        let slow = Style::Auto.repertoire(Features {
            period: 0.7,
            ..music
        });
        assert!(auto[2] > auto[0] && auto[2] > auto[1]);
        assert!(slow[1] > slow[0] && slow[1] > slow[2]);
        for (index, genre) in [Genre::House, Genre::Hiphop, Genre::Rave]
            .into_iter()
            .enumerate()
        {
            let mix = Style::Auto.repertoire(Features { genre, ..music });
            assert!(mix[index] > auto[index] || mix[index] > 0.6);
            assert!(mix.iter().sum::<f32>() <= 1.00001);
            assert_eq!(
                Style::Auto.repertoire(Features {
                    genre,
                    confidence: 0.0,
                    ..music
                }),
                [0.0; 3]
            );
        }
        let ambient = Features {
            genre: Genre::Ambient,
            ..music
        };
        assert_eq!(Style::Auto.repertoire(ambient), [0.0; 3]);
        assert!(Style::Auto.weights(ambient)[0] > 0.85);
        let quiet = Features {
            energy: 0.3,
            bass: 0.1,
            genre: Genre::Rave,
            ..music
        };
        assert_eq!(Style::Auto.repertoire(quiet)[2], 0.0);
        for (label, genre) in [
            ("Hip-Hop", Genre::Hiphop),
            ("deep-house", Genre::House),
            ("hardcore", Genre::Rave),
            ("ambientgenre", Genre::Ambient),
            ("classical", Genre::Unknown),
        ] {
            assert_eq!(Genre::from_metadata(Some(label)), genre);
        }
    }

    #[test]
    fn repertoire_changes_blend_while_the_current_phrase_finishes() {
        let mut motion = Motion::with_seed(Style::Auto, 42);
        let music = Features {
            energy: 0.8,
            bass: 0.8,
            confidence: 1.0,
            genre: Genre::House,
            ..Features::default()
        };
        for _ in 0..1200 {
            motion.advance(music, 1.0 / 120.0);
        }
        let before = motion.repertoire;
        let movement = motion.choreography.phrase.movement;
        let pose = motion.pose;
        motion.begin_transition();
        motion.advance(
            Features {
                genre: Genre::Hiphop,
                ..music
            },
            1.0 / 120.0,
        );
        assert_eq!(motion.choreography.phrase.movement, movement);
        assert!(pose.head.distance(motion.pose.head) < 0.04);
        assert!(motion.repertoire[1] > before[1]);
        for (a, b) in motion.repertoire.into_iter().zip(before) {
            assert!((a - b).abs() < 0.003);
        }
    }

    #[test]
    fn gesture_wrap_handles_rounding_at_the_last_key() {
        for keys in [&FLOW, &GROOVE, &PULSE, &PUMP] {
            assert!(gesture(keys, -f32::EPSILON).distance(keys[0]) < 0.00001);
            assert_eq!(gesture(keys, 8.0), keys[0]);
        }
    }

    #[test]
    fn a_reach_phrase_extends_and_gathers_the_arm() {
        for movement in [Move::Reach, Move::Sway] {
            let mut motion = Motion::with_seed(Style::Flow, 42);
            motion.choreography.phrase.movement = movement;
            motion.choreography.phrase.duration = 30.0;
            let mut smallest_bend = f32::INFINITY;
            let mut largest_bend = 0.0_f32;
            for _ in 0..120 * 25 {
                motion.advance(
                    Features {
                        energy: 0.8,
                        ..Features::default()
                    },
                    1.0 / 120.0,
                );
                smallest_bend = smallest_bend.min(motion.hands[0].flexion.position);
                largest_bend = largest_bend.max(motion.hands[0].flexion.position);
            }
            assert!(
                smallest_bend < 0.2,
                "Arm never straightens: minimum bend={smallest_bend}"
            );
            assert!(
                largest_bend > 1.5,
                "Arm never gathers: maximum bend={largest_bend}"
            );
        }
    }

    #[test]
    fn raised_reaches_follow_the_wrist_without_pinning_the_shoulder() {
        for side in [-1.0, 1.0] {
            for torso in [-0.6_f32, 0.0, 0.6] {
                let shoulder = Point::new(0.1, 0.3);
                for target in [
                    Point::new(0.22, -0.08),
                    Point::new(0.1, -0.23),
                    Point::new(-0.04, -0.19),
                ] {
                    let x = side * target.x;
                    let y = target.y;
                    let wrist = shoulder.offset(
                        x * torso.cos() - y * torso.sin(),
                        x * torso.sin() + y * torso.cos(),
                    );
                    let (elevation, flexion) = Arm::targets(shoulder, wrist, torso, side);
                    let arm = Arm {
                        elevation: Spring {
                            position: elevation,
                            velocity: 0.0,
                        },
                        flexion: Spring {
                            position: flexion,
                            velocity: 0.0,
                        },
                        ..Arm::default()
                    };
                    let (_, actual) = arm.joints(shoulder, torso, side);
                    assert!(
                        actual.distance(wrist) < 0.001,
                        "Raised reach misses the wrist: error={}",
                        actual.distance(wrist)
                    );
                    assert!(elevation < 2.75, "Shoulder is pinned overhead");
                }
            }
        }
    }

    #[test]
    fn arms_stay_bounded_without_winding_or_elbow_flips() {
        for style in [
            Style::Flow,
            Style::Groove,
            Style::Pulse,
            Style::House,
            Style::Hiphop,
            Style::Rave,
        ] {
            let mut motion = Motion::with_seed(style, 42);
            for frame in 0..120 * 60 {
                let previous: [(f32, f32); 2] = std::array::from_fn(|i| {
                    (
                        motion.hands[i].elevation.position,
                        motion.hands[i].flexion.position,
                    )
                });
                motion.advance(
                    Features {
                        energy: 0.95,
                        bass: 0.4,
                        confidence: 1.0,
                        hit: frame % 42 == 0,
                        period: 0.35,
                        ..Features::default()
                    },
                    1.0 / 120.0,
                );
                let torso = motion.torso_angle();
                for (index, side) in [-1.0, 1.0].into_iter().enumerate() {
                    let direction = |start: Point, end: Point| {
                        let x = end.x - start.x;
                        let y = end.y - start.y;
                        let outward = side * (x * torso.cos() + y * torso.sin());
                        let down = -x * torso.sin() + y * torso.cos();
                        outward.atan2(down)
                    };
                    let shoulder =
                        direction(motion.pose.shoulders[index], motion.pose.elbows[index]);
                    let forearm = direction(motion.pose.elbows[index], motion.pose.wrists[index]);
                    assert!(
                        (-1.251..=2.851).contains(&shoulder),
                        "Shoulder leaves the front arc: angle={shoulder}, frame={frame}"
                    );
                    let bend = (forearm - shoulder + PI).rem_euclid(TAU) - PI;
                    assert!(
                        (0.024..=2.501).contains(&bend),
                        "Elbow changes its folding branch: {bend}"
                    );
                    assert!((shoulder - previous[index].0).abs() <= 4.01 / 120.0);
                    assert!((bend - previous[index].1).abs() <= 5.01 / 120.0);
                }
            }
        }
    }

    #[test]
    fn resting_legs_are_nearly_straight_and_crouching_still_bends_them() {
        let mut motion = Motion::with_seed(Style::Pulse, 42);
        for i in 0..2 {
            assert!(
                motion.pose.hips[i].distance(motion.pose.ankles[i]) > 0.4485,
                "Resting pose holds a permanent knee bend"
            );
        }
        motion.choreography.phrase.movement = Move::Crouch;
        motion.choreography.phrase.duration = 60.0;
        motion.choreography.phrase.progress = 0.5;
        for _ in 0..120 {
            motion.advance(
                Features {
                    energy: 0.9,
                    bass: 0.4,
                    ..Features::default()
                },
                1.0 / 120.0,
            );
        }
        assert!((0..2).all(|i| motion.pose.hips[i].distance(motion.pose.ankles[i]) < 0.4));
    }

    #[test]
    fn fast_travel_and_reversals_carry_a_connected_torso() {
        for fps in [30, 60, 120] {
            let mut motion = Motion::with_seed(Style::Pulse, 42);
            let mut maximum_acceleration = 0.0_f32;
            let mut minimum = 0.0_f32;
            let mut maximum = 0.0_f32;
            for direction in [1.0, -1.0, 1.0] {
                motion.choreography.phrase.movement = Move::Travel;
                motion.choreography.phrase.start = motion.root.position;
                motion.choreography.phrase.destination = direction * 1.4;
                motion.choreography.phrase.duration = 4.0;
                motion.choreography.phrase.progress = 0.0;
                motion.choreography.phrase.performed = false;
                for _ in 0..fps * 5 {
                    let previous = motion.pose;
                    let velocity = motion.root.velocity;
                    motion.advance(
                        Features {
                            energy: 0.95,
                            period: 0.18,
                            confidence: 1.0,
                            ..Features::default()
                        },
                        1.0 / fps as f32,
                    );
                    let pelvis = motion.pose.hips[0].lerp(motion.pose.hips[1], 0.5);
                    let chest = motion.pose.shoulders[0].lerp(motion.pose.shoulders[1], 0.5);
                    minimum = minimum.min(pelvis.x);
                    maximum = maximum.max(pelvis.x);
                    let acceleration = (motion.root.velocity - velocity).abs() * fps as f32;
                    maximum_acceleration = maximum_acceleration.max(acceleration);
                    assert!(acceleration <= GRAVITY * 1.201);
                    assert!(previous.head.distance(motion.pose.head) < 4.0 / fps as f32);
                    assert!((chest.distance(motion.pose.head) - 0.102).abs() < 0.002);
                    assert!(
                        (pelvis.distance(chest) - 0.27).abs() < 0.002,
                        "Torso stretches while travelling: fps={fps}, distance={}",
                        pelvis.distance(chest)
                    );
                }
            }
            assert!(maximum_acceleration > 0.5, "Travel does not push the body");
            assert!(
                maximum - minimum > 1.0,
                "Missing multi-step travel: range={} fps={fps}",
                maximum - minimum
            );
            motion.begin_transition();
            for _ in 0..fps * 8 {
                motion.advance(Features::default(), 1.0 / fps as f32);
            }
            assert!(motion.root.velocity.abs() < 0.001, "Body does not brake");
            assert!(!motion.transitioning());
        }
    }

    #[test]
    fn fast_steps_shorten_the_stride_and_lift_instead_of_scaling_the_same_pose() {
        let measure = |period| {
            let mut motion = Motion::with_seed(Style::Pulse, 42);
            motion.cadence.position = period * 1.07;
            motion.choreography.phrase.movement = Move::Shuffle;
            motion.choreography.phrase.duration = 4.0;
            motion.choreography.phrase.destination = 1.4;
            let mut stride = 0.0_f32;
            let mut lift = 0.0_f32;
            for _ in 0..120 * 10 {
                motion.advance(
                    Features {
                        energy: 0.95,
                        period,
                        ..Features::default()
                    },
                    1.0 / 120.0,
                );
                if let Some(swing) = &motion.swing {
                    stride = stride.max((swing.target.x - swing.start.x).abs());
                    lift = lift.max(swing.lift);
                }
            }
            (stride, lift)
        };
        let regular = measure(0.5);
        let fast = measure(0.18);
        assert!(
            fast.0 < regular.0 * 0.9,
            "Same stride: {fast:?}, {regular:?}"
        );
        assert!(fast.1 < regular.1 * 0.8, "Same lift: {fast:?}, {regular:?}");
    }

    #[test]
    fn flight_uses_gravity_and_does_not_accelerate_with_the_music() {
        let mut fast = Jump::new(0.2, 0.18);
        let mut slow = Jump::new(0.2, 0.8);
        assert_eq!(fast.flight_time(), slow.flight_time());
        let dt = 0.01;
        for fraction in [0.2, 0.5, 0.8] {
            let time = fast.flight_time() * fraction;
            let sample = |jump: &mut Jump, offset| {
                jump.elapsed = jump.preparation + time + offset;
                jump.vertical().1
            };
            let before = sample(&mut fast, -dt);
            let height = sample(&mut fast, 0.0);
            let after = sample(&mut fast, dt);
            let acceleration = (after - 2.0 * height + before) / (dt * dt);
            assert!((acceleration + GRAVITY).abs() < 0.003);
            assert!((height - sample(&mut slow, 0.0)).abs() < 0.00001);
        }
        fast.elapsed = fast.preparation + fast.flight_time() / 2.0;
        assert!((fast.vertical().1 - fast.height).abs() < 0.00001);
        fast.elapsed = fast.duration;
        assert_eq!(fast.vertical(), (0.0, 0.0));
    }

    #[test]
    fn an_airborne_body_keeps_takeoff_momentum_without_a_ground_push() {
        let mut motion = Motion::with_seed(Style::Pulse, 42);
        let mut jump = Jump::new(0.2, 0.35);
        jump.elapsed = jump.preparation + 0.05;
        motion.jump = Some(jump);
        motion.root.velocity = 0.15;
        let feet = motion.feet;
        for _ in 0..24 {
            motion.advance(Features::default(), 1.0 / 120.0);
            assert_eq!(motion.root.velocity, 0.15);
        }
        assert!((motion.root.position - 0.03).abs() < 0.00001);
        for (foot, start) in motion.feet.iter().zip(feet) {
            assert!((foot.x - start.x - 0.03).abs() < 0.00001);
        }
    }

    #[test]
    fn club_motifs_have_distinct_knee_lifts_landing_dynamics_and_torso_accents() {
        let measure = |movement| {
            let mut motion = Motion::with_seed(Style::Pulse, 42);
            motion.choreography.phrase.movement = movement;
            motion.choreography.phrase.duration = 60.0;
            let mut peak_lift = 0.0_f32;
            let mut landing_speed = 0.0_f32;
            let mut jack = 0.0_f32;
            for frame in 0..1800 {
                let previous = motion.pose;
                let hit = frame % 60 == 0;
                motion.advance(
                    Features {
                        energy: 0.95,
                        bass: 0.4,
                        period: 0.5,
                        hit,
                        accent: if hit { 1.0 } else { 0.0 },
                        ..Features::default()
                    },
                    1.0 / 120.0,
                );
                for i in 0..2 {
                    peak_lift = peak_lift.max(0.97 - motion.pose.ankles[i].y);
                    landing_speed =
                        landing_speed.max((motion.pose.ankles[i].y - previous.ankles[i].y) * 120.0);
                }
                jack = jack.max(motion.jack.position);
            }
            (peak_lift, landing_speed, jack)
        };
        let shuffle = measure(Move::Shuffle);
        let stomp = measure(Move::Stomp);
        let jack = measure(Move::Jack);
        assert!(shuffle.0 > 0.15 && shuffle.0 > jack.0 * 1.5);
        assert!(
            stomp.1 > shuffle.1 * 1.1,
            "Stomp does not land more sharply: stomp={stomp:?} shuffle={shuffle:?}"
        );
        assert!(jack.2 > 0.07 && shuffle.2 == 0.0);
    }

    #[test]
    fn a_track_change_preserves_a_fall_and_waits_for_recovery_before_blending() {
        let mut motion = Motion::with_seed(Style::Pulse, 42);
        motion.balance.kick(8.0);
        for _ in 0..60 {
            motion.advance(Features::default(), 1.0 / 120.0);
        }
        assert!(motion.balance.brace() > 0.9);
        let old_pose = motion.pose;
        let old_angle = motion.balance.angle;
        motion.begin_transition();
        assert_eq!(motion.pose, old_pose);
        assert_eq!(motion.balance.angle, old_angle);
        let mut previous = old_pose;
        let mut waited = false;
        for _ in 0..960 {
            motion.advance(Features::default(), 1.0 / 120.0);
            if motion.balance.recovering() {
                waited = true;
                assert_eq!(motion.transition, Transition::Finishing);
            }
            assert!(previous.head.distance(motion.pose.head) < 0.04);
            assert!(motion.pose.ankles.iter().all(|foot| foot.y == 0.97));
            previous = motion.pose;
        }
        assert!(waited && !motion.transitioning() && !motion.balance.recovering());
    }

    #[test]
    fn beats_drive_the_body_and_sustained_flow_remains_independent_of_onsets() {
        let energetic = Features {
            energy: 0.95,
            bass: 0.85,
            confidence: 1.0,
            period: 0.5,
            ..Features::default()
        };
        assert!(
            Style::Auto.weights(energetic)[2] > 0.5,
            "Strong moderate-tempo music never reaches Pulse"
        );
        let mut driven = Motion::with_seed(Style::Pulse, 42);
        let mut steady = Motion::with_seed(Style::Pulse, 42);
        let mut flow = Motion::with_seed(Style::Flow, 42);
        let mut flow_control = Motion::with_seed(Style::Flow, 42);
        let mut driven_vertical = 0.0;
        let mut steady_vertical = 0.0;
        for frame in 0..3600 {
            let hit = frame % 60 == 0;
            let features = Features {
                hit,
                accent: if hit { 1.0 } else { 0.0 },
                ..energetic
            };
            let previous = [driven.pose.head.y, steady.pose.head.y];
            driven.advance(features, 1.0 / 120.0);
            steady.advance(
                Features {
                    hit: false,
                    ..features
                },
                1.0 / 120.0,
            );
            flow.advance(features, 1.0 / 120.0);
            flow_control.advance(
                Features {
                    hit: false,
                    ..features
                },
                1.0 / 120.0,
            );
            driven_vertical += (driven.pose.head.y - previous[0]).abs();
            steady_vertical += (steady.pose.head.y - previous[1]).abs();
            assert_eq!(
                flow.pose, flow_control.pose,
                "Onsets disturb sustained Flow"
            );
        }
        assert!(
            driven_vertical > steady_vertical * 1.3,
            "Beats add too little body movement: driven={driven_vertical}, steady={steady_vertical}"
        );
    }

    #[test]
    fn real_dance_elbows_clear_the_rendered_body() {
        let mut worst = [
            (f32::NEG_INFINITY, String::new()),
            (f32::NEG_INFINITY, String::new()),
            (f32::NEG_INFINITY, String::new()),
        ];
        for style in [Style::Flow, Style::Auto, Style::Groove, Style::Pulse] {
            for seed in [1, 42, 12345] {
                let mut motion = Motion::with_seed(style, seed);
                for frame in 0..3600 {
                    motion.advance(
                        Features {
                            energy: 0.8,
                            bass: 0.4,
                            confidence: 1.0,
                            hit: frame % 30 == 0,
                            ..Features::default()
                        },
                        1.0 / 60.0,
                    );
                    let pose = motion.spatial.pose;
                    let chest = pose.shoulders[0].lerp(pose.shoulders[1], 0.5);
                    let hip = pose.hips[0].lerp(pose.hips[1], 0.5);
                    let width = |front: f32, side: f32, yaw: f32| {
                        (front * yaw.cos()).hypot(side * yaw.sin())
                    };
                    let body = [
                        (
                            "chest",
                            chest,
                            pose.waist,
                            [width(0.092, 0.05, pose.yaw), 0.053],
                        ),
                        (
                            "pelvis",
                            pose.waist,
                            hip,
                            [0.053, width(0.088, 0.055, pose.hip_yaw)],
                        ),
                        ("left thigh", pose.hips[0], pose.knees[0], [0.055, 0.037]),
                        ("right thigh", pose.hips[1], pose.knees[1], [0.055, 0.037]),
                    ];
                    for i in 0..2 {
                        for (probe, a, b, radii, from) in [
                            (0, pose.elbows[i], pose.elbows[i], [0.028, 0.028], 0.0),
                            (1, pose.shoulders[i], pose.elbows[i], [0.036, 0.027], 0.45),
                            (2, pose.elbows[i], pose.wrists[i], [0.028, 0.018], 0.0),
                        ] {
                            for section in 0..=if probe == 0 { 0 } else { 16 } {
                                let t = from + (1.0 - from) * section as f32 / 16.0;
                                let point = a.lerp(b, t);
                                let arm_radius = radii[0] + (radii[1] - radii[0]) * t;
                                for (name, start, end, radii) in body {
                                    // Sample the rendered sphere volumes independently of the solver.
                                    for sample in 0..=32 {
                                        let t = sample as f32 / 32.0;
                                        let center = start.lerp(end, t);
                                        let radius = radii[0] + (radii[1] - radii[0]) * t;
                                        let overlap =
                                            radius + arm_radius - point.sub(center).length();
                                        if overlap > worst[probe].0 {
                                            worst[probe] = (
                                                overlap,
                                                format!(
                                                    "{name}: style={style:?} seed={seed} frame={frame} arm={i} section={section} pose={pose:?}"
                                                ),
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        assert!(
            worst.iter().all(|(overlap, _)| *overlap < 0.0001),
            "Arm/body overlap (elbow, upper arm, forearm): {worst:?}"
        );
    }

    #[test]
    fn energetic_improvisation_recovers_keeps_contacts_and_limits_falls() {
        let mut total_falls = 0;
        let mut catches = 0;
        for (style, period) in [
            Style::Pulse,
            Style::House,
            Style::Hiphop,
            Style::Rave,
            Style::Auto,
        ]
        .into_iter()
        .flat_map(|style| [0.18, 0.35, 0.7].map(|period| (style, period)))
        {
            for fps in [30, 60, 120] {
                for seed in [1, 42, 12345] {
                    let mut motion = Motion::with_seed(style, seed);
                    let mut previous = motion.pose;
                    let mut spatial_previous = motion.spatial.pose;
                    let mut braced = false;
                    let mut falls = 0;
                    for frame in 0..fps * 180 {
                        let dt = 1.0 / fps as f32;
                        let hit = (frame as f32 * dt / period).floor()
                            != ((frame + 1) as f32 * dt / period).floor();
                        let sliding_before = motion.sliding_foot();
                        motion.advance(
                            Features {
                                energy: 0.95,
                                bass: 0.9,
                                confidence: 1.0,
                                period,
                                accent: if hit { 1.0 } else { 0.0 },
                                hit,
                                ..Features::default()
                            },
                            dt,
                        );
                        let brace = motion.balance.brace() > 0.0;
                        falls += usize::from(brace && !braced);
                        catches += usize::from(
                            motion.balance.catch_direction().is_some()
                                || (motion.balance.recovering() && !brace),
                        );
                        braced = brace;
                        let pose = motion.pose;
                        assert!(
                            pose.head.y > -0.15 && pose.head.y < 0.95,
                            "Head leaves the stage: style={style:?} seed={seed} frame={frame} fps={fps} period={period} pose={pose:?}"
                        );
                        assert!(motion.feet.iter().all(|foot| foot.x.abs() <= 1.566));
                        assert!(
                            previous.head.distance(pose.head) < dt * 5.0,
                            "Head snaps: style={style:?} seed={seed} frame={frame} fps={fps} period={period}"
                        );
                        for i in 0..2 {
                            if previous.ankles[i].y == 0.97
                                && pose.ankles[i].y == 0.97
                                && sliding_before != Some(i)
                                && motion.sliding_foot() != Some(i)
                            {
                                assert_eq!(previous.ankles[i].x, pose.ankles[i].x);
                            }
                            assert!(
                                (pose.hips[i].distance(pose.knees[i]) - 0.22).abs() < 0.002,
                                "Leg reach: style={style:?} period={period} fps={fps} seed={seed} frame={frame} movement={:?} pose={pose:?} root={} brace={}",
                                motion.choreography.phrase.movement,
                                motion.root.position,
                                motion.balance.brace()
                            );
                            assert!((pose.knees[i].distance(pose.ankles[i]) - 0.23).abs() < 0.002);
                            assert!(
                                (pose.shoulders[i].distance(pose.elbows[i]) - 0.16).abs() < 0.001
                            );
                            assert!((pose.elbows[i].distance(pose.wrists[i]) - 0.15).abs() < 0.001);
                            assert!(
                                previous.wrists[i].distance(pose.wrists[i]) < dt * 6.0,
                                "Wrist snaps: style={style:?} seed={seed} frame={frame} fps={fps}"
                            );
                        }
                        let spatial = motion.spatial.pose;
                        for i in 0..2 {
                            assert!(
                                (spatial.elbows[i].sub(spatial.shoulders[i]).length() - 0.16).abs()
                                    < 0.001
                            );
                            assert!(
                                (spatial.wrists[i].sub(spatial.elbows[i]).length() - 0.15).abs()
                                    < 0.001
                            );
                            assert!(
                                (spatial.knees[i].sub(spatial.hips[i]).length() - 0.22).abs()
                                    < 0.002
                            );
                            assert!(
                                (spatial.ankles[i].sub(spatial.knees[i]).length() - 0.23).abs()
                                    < 0.002,
                                "3D leg reach: {style:?} seed={seed} period={period} fps={fps} frame={frame} pose={spatial:?}"
                            );
                            if previous.ankles[i].y == 0.97
                                && pose.ankles[i].y == 0.97
                                && sliding_before != Some(i)
                                && motion.sliding_foot() != Some(i)
                            {
                                assert_eq!(spatial_previous.ankles[i], spatial.ankles[i]);
                            }
                            assert!(
                                spatial.elbows[i].sub(spatial_previous.elbows[i]).length()
                                    < dt * 8.0,
                                "3D elbow snaps: {style:?} seed={seed} period={period} fps={fps} frame={frame}"
                            );
                            assert!(
                                spatial.wrists[i].sub(spatial_previous.wrists[i]).length()
                                    < dt * 8.0,
                                "3D wrist snaps: {style:?} seed={seed} period={period} fps={fps} frame={frame}"
                            );
                        }
                        let hip = spatial.hips[0].lerp(spatial.hips[1], 0.5);
                        let chest = spatial.shoulders[0].lerp(spatial.shoulders[1], 0.5);
                        assert!((chest.sub(hip).length() - 0.27).abs() < 0.001);
                        assert!((spatial.head.sub(chest).length() - 0.102).abs() < 0.001);
                        spatial_previous = spatial;
                        previous = pose;
                    }
                    assert!(falls <= 5, "Too many falls in three minutes: {falls}");
                    total_falls += falls;
                    motion.begin_transition();
                    for _ in 0..fps * 8 {
                        motion.advance(Features::default(), 1.0 / fps as f32);
                    }
                    assert!(!motion.transitioning() && !motion.balance.recovering());
                }
            }
        }
        assert!(total_falls > 0 && catches > 0, "Missing reactive recovery");
    }

    #[test]
    fn transitions_finish_contacts_then_blend_and_keep_the_rig_continuous() {
        let energetic = Features {
            energy: 0.9,
            bass: 0.7,
            confidence: 1.0,
            period: 0.35,
            ..Features::default()
        };
        for airborne in [false, true] {
            let mut motion = Motion::with_seed(Style::Auto, 1);
            let mut reached = false;
            for _ in 0..36000 {
                motion.advance(energetic, 1.0 / 120.0);
                let flight = motion
                    .jump
                    .as_ref()
                    .is_some_and(|jump| jump.vertical().1 > 0.06);
                if (airborne && flight) || (!airborne && motion.swing.is_some() && !flight) {
                    reached = true;
                    break;
                }
            }
            assert!(reached, "Did not reach the outgoing move");
            let pose = motion.pose;
            let root = motion.root.position;
            motion.begin_transition();
            assert_eq!(motion.pose, pose);
            assert_eq!(motion.root.position, root);
            assert_eq!(motion.transition, Transition::Finishing);
            let calm = Features {
                energy: 0.3,
                period: 1.2,
                ..Features::default()
            };
            let mut previous = pose;
            let mut blended = false;
            for frame in 0..900 {
                if frame < 60 && frame % 12 == 0 {
                    motion.begin_transition();
                }
                let sliding_before = motion.sliding_foot();
                motion.advance(calm, 1.0 / 120.0);
                let pose = motion.pose;
                if matches!(motion.transition, Transition::Blending(_)) {
                    assert!(pose.ankles.iter().all(|foot| foot.y == 0.97));
                    blended = true;
                }
                assert!(previous.head.distance(pose.head) < 0.04);
                for i in 0..2 {
                    if previous.ankles[i].y == 0.97
                        && pose.ankles[i].y == 0.97
                        && sliding_before != Some(i)
                        && motion.sliding_foot() != Some(i)
                    {
                        assert_eq!(previous.ankles[i].x, pose.ankles[i].x);
                    }
                    assert!((pose.hips[i].distance(pose.knees[i]) - 0.22).abs() < 0.002);
                    assert!((pose.knees[i].distance(pose.ankles[i]) - 0.23).abs() < 0.002);
                    assert!(
                        previous.wrists[i].distance(pose.wrists[i]) < 0.04,
                        "Wrist jump: previous={:?} current={:?} radius={}",
                        previous.wrists[i],
                        pose.wrists[i],
                        pose.wrists[i].distance(pose.shoulders[i])
                    );
                }
                previous = pose;
            }
            assert!(blended && !motion.transitioning());
            assert!(motion.weights[0] > 0.9, "New music did not become Flow");
        }
    }

    #[test]
    fn loaded_feet_do_not_slide_and_the_rig_keeps_its_limb_lengths() {
        for style in [
            Style::Flow,
            Style::Groove,
            Style::Pulse,
            Style::House,
            Style::Hiphop,
            Style::Rave,
        ] {
            let mut motion = Motion::with_seed(style, 42);
            let features = Features {
                energy: 0.8,
                bass: 0.4,
                confidence: 1.0,
                ..Features::default()
            };
            let mut previous = motion.pose;
            let mut stepped = false;
            for _ in 0..2400 {
                let sliding_before = motion.sliding_foot();
                motion.advance(features, 1.0 / 120.0);
                let pose = motion.pose;
                for i in 0..2 {
                    if previous.ankles[i].y == 0.97
                        && pose.ankles[i].y == 0.97
                        && sliding_before != Some(i)
                        && motion.sliding_foot() != Some(i)
                    {
                        assert_eq!(
                            previous.ankles[i].x, pose.ankles[i].x,
                            "Planted foot slides"
                        );
                    }
                    stepped |= pose.ankles[i] != previous.ankles[i];
                    assert!((pose.shoulders[i].distance(pose.elbows[i]) - 0.16).abs() < 0.001);
                    assert!((pose.elbows[i].distance(pose.wrists[i]) - 0.15).abs() < 0.001);
                    assert!((pose.hips[i].distance(pose.knees[i]) - 0.22).abs() < 0.002);
                    assert!((pose.knees[i].distance(pose.ankles[i]) - 0.23).abs() < 0.002);
                    assert!(previous.wrists[i].distance(pose.wrists[i]) < 0.025);
                }
                if motion.jump.as_ref().map_or(0.0, |jump| jump.vertical().1) == 0.0 {
                    assert!(pose.ankles.iter().any(|foot| foot.y == 0.97));
                } else {
                    assert!(pose.ankles.iter().all(|foot| foot.y < 0.97));
                }
                previous = pose;
            }
            assert!(stepped);
        }
    }

    #[test]
    fn footwork_separates_weight_shifts_drags_and_lifts() {
        assert_eq!(Footwork::choose(Move::Jack, 0.95, 0.7), Footwork::Grounded);
        assert_eq!(Footwork::choose(Move::Rock, 0.95, 0.8), Footwork::Slide);
        assert_eq!(Footwork::choose(Move::Travel, 0.8, 0.2), Footwork::Slide);
        assert_eq!(Footwork::choose(Move::Travel, 0.8, 0.6), Footwork::Skim);
        assert_eq!(Footwork::choose(Move::Travel, 0.8, 0.9), Footwork::Lift);
        assert_eq!(Footwork::choose(Move::Shuffle, 0.8, 0.0), Footwork::Lift);
    }

    #[test]
    fn floor_drag_moves_continuously_in_3d_and_finishes_before_a_transition() {
        let mut motion = Motion::with_seed(Style::Groove, 42);
        motion.step_phase = 0.12;
        motion.step_planned = true;
        motion.choreography.phrase.duration = 60.0;
        motion.swing = Some(Swing {
            foot: 1,
            start: motion.feet[1],
            target: Point::new(0.2, 0.97),
            lift: 0.0,
            stomp: false,
            footwork: Footwork::Slide,
            excursion: 0.0,
        });
        for _ in 0..300 {
            motion.spatial.advance(
                &motion.pose,
                crate::dance_space::Cue {
                    energy: 0.8,
                    ..crate::dance_space::Cue::default()
                },
                1.0 / 60.0,
            );
        }
        let loaded = motion.spatial.pose.ankles[0];
        let mut previous = motion.spatial.pose.ankles[1];
        let dt = 1.0 / 120.0;
        let mut frames_moving = 0;
        for frame in 0..50 {
            if frame == 20 {
                motion.begin_transition();
            }
            motion.advance(
                Features {
                    energy: 0.8,
                    ..Features::default()
                },
                dt,
            );
            let foot = motion.spatial.pose.ankles[1];
            assert_eq!(foot.y, 0.97);
            assert_eq!(motion.spatial.pose.ankles[0], loaded);
            assert!(
                foot.sub(previous).length() < dt * 2.0,
                "Dragging foot snaps"
            );
            frames_moving += usize::from(foot.x != previous.x);
            previous = foot;
            if frame == 20 {
                assert_eq!(motion.transition, Transition::Finishing);
            }
        }
        assert!(frames_moving > 15, "Drag is a single foot teleport");
        assert!((previous.x - 0.2).abs() < 0.00001);
        assert!(
            previous.z.abs() > 0.02,
            "Depth still pins the dragging foot"
        );
        for _ in 0..360 {
            motion.advance(Features::default(), dt);
        }
        assert!(!motion.transitioning());
    }

    #[test]
    fn resuming_mid_cycle_waits_before_starting_a_floor_drag() {
        let mut motion = Motion::with_seed(Style::Groove, 42);
        motion.choreography.phrase.duration = 60.0;
        motion.step_phase = 0.5;
        let feet = motion.feet;
        let music = Features {
            energy: 0.8,
            ..Features::default()
        };
        motion.advance(music, 1.0 / 120.0);
        assert!(
            motion.swing.is_none(),
            "Resumption enters a half-completed step"
        );
        assert_eq!(motion.feet, feet);
        for _ in 0..45 {
            motion.advance(music, 1.0 / 120.0);
        }
        assert_eq!(motion.sliding_foot(), Some(1));
    }

    #[test]
    fn skimming_travel_barely_clears_the_floor() {
        let mut motion = Motion::with_seed(Style::Pulse, 42);
        motion.choreography.phrase.movement = Move::Travel;
        motion.choreography.phrase.duration = 60.0;
        motion.choreography.phrase.progress = 0.25;
        motion.choreography.phrase.destination = 0.5;
        motion.step_phase = 0.115;
        let mut peak = 0.0_f32;
        for _ in 0..90 {
            motion.advance(
                Features {
                    energy: 1.0,
                    ..Features::default()
                },
                1.0 / 120.0,
            );
            assert_eq!(motion.footwork, Footwork::Skim);
            for foot in motion.pose.ankles {
                peak = peak.max(0.97 - foot.y);
            }
        }
        assert!(
            (0.015..=0.02201).contains(&peak),
            "Low step lifts too far: {peak}"
        );
    }

    #[test]
    fn grounded_groove_moves_the_body_without_lifting_either_foot() {
        let mut motion = Motion::with_seed(Style::Groove, 42);
        motion.choreography.phrase.movement = Move::Jack;
        motion.choreography.phrase.duration = 60.0;
        let feet = motion.spatial.pose.ankles;
        let mut left = 0.0_f32;
        let mut right = 0.0_f32;
        let mut low = 1.0_f32;
        let mut high = 0.0_f32;
        for frame in 0..600 {
            if frame >= 56 {
                motion.step_planned = true;
            }
            motion.advance(
                Features {
                    energy: 0.8,
                    bass: 0.4,
                    hit: frame >= 56 && frame % 60 == 0,
                    ..Features::default()
                },
                1.0 / 120.0,
            );
            assert_eq!(motion.spatial.pose.ankles, feet);
            assert_eq!(
                motion.footwork,
                Footwork::Grounded,
                "Contact choice rerolls within a cycle"
            );
            left = left.min(motion.root.position);
            right = right.max(motion.root.position);
            low = low.min(motion.pose.head.y);
            high = high.max(motion.pose.head.y);
        }
        assert!(right - left > 0.025, "Missing planted weight transfer");
        assert!(
            high - low > 0.015,
            "Missing grounded bounce: {}",
            high - low
        );
    }

    #[test]
    fn ambient_uses_free_phrasing_and_style_changes_blend_without_a_jump() {
        let mut slow = Motion::with_seed(Style::Flow, 7);
        let mut fast = Motion::with_seed(Style::Flow, 7);
        for _ in 0..360 {
            slow.advance(
                Features {
                    energy: 0.7,
                    period: 1.0,
                    ..Features::default()
                },
                1.0 / 60.0,
            );
            fast.advance(
                Features {
                    energy: 0.7,
                    period: 0.3,
                    ..Features::default()
                },
                1.0 / 60.0,
            );
        }
        assert_eq!(slow.pose, fast.pose, "Ambient is being driven by BPM");
        let mut motion = Motion::new(Style::Auto);
        let old = motion.pose;
        motion.advance(
            Features {
                energy: 0.8,
                confidence: 1.0,
                period: 0.35,
                ..Features::default()
            },
            1.0 / 60.0,
        );
        assert!(motion.weights[2] < 0.01);
        assert!(old.head.distance(motion.pose.head) < 0.005);
        let silent_beat = Style::Auto.weights(Features {
            energy: 0.8,
            period: 0.35,
            confidence: 0.0,
            ..Features::default()
        });
        assert_eq!(silent_beat, [1.0, 0.0, 0.0]);
    }

    #[test]
    fn phrases_travel_both_ways_crouch_and_land_without_breaking_the_rig() {
        for seed in [1, 42, 12345] {
            let mut motion = Motion::with_seed(Style::Pulse, seed);
            let features = Features {
                energy: 0.9,
                bass: 0.7,
                confidence: 1.0,
                period: 0.35,
                ..Features::default()
            };
            let mut left = 0.0_f32;
            let mut right = 0.0_f32;
            let mut jumped = false;
            let mut crouched = false;
            let mut previous = motion.pose;
            let mut phrases = std::collections::HashSet::new();
            for frame in 0..18000 {
                let sliding_before = motion.sliding_foot();
                motion.advance(features, 1.0 / 120.0);
                left = left.min(motion.root.position);
                right = right.max(motion.root.position);
                crouched |= motion.crouch.position > 0.1;
                jumped |= motion.pose.ankles.iter().all(|foot| foot.y < 0.9);
                phrases.insert(motion.choreography.phrase.gesture_offset.to_bits());
                let pose = motion.pose;
                assert!(motion.feet.iter().all(|foot| foot.x.abs() < 1.57));
                assert!(
                    previous.head.distance(pose.head) < 0.04,
                    "Head snaps between phrases"
                );
                for i in 0..2 {
                    assert!((pose.hips[i].distance(pose.knees[i]) - 0.22).abs() < 0.002);
                    assert!((pose.knees[i].distance(pose.ankles[i]) - 0.23).abs() < 0.002);
                    assert!(
                        (pose.shoulders[i].distance(pose.elbows[i]) - 0.16).abs() < 0.001,
                        "seed={seed} frame={frame} move={:?} hand distance={}",
                        motion.choreography.phrase.movement,
                        pose.shoulders[i].distance(pose.wrists[i])
                    );
                    assert!((pose.elbows[i].distance(pose.wrists[i]) - 0.15).abs() < 0.001);
                    assert!(
                        previous.wrists[i].distance(pose.wrists[i]) < 0.04,
                        "Wrist jump: previous={:?} current={:?} radius={}",
                        previous.wrists[i],
                        pose.wrists[i],
                        pose.wrists[i].distance(pose.shoulders[i])
                    );
                    if previous.ankles[i].y == 0.97
                        && pose.ankles[i].y == 0.97
                        && sliding_before != Some(i)
                        && motion.sliding_foot() != Some(i)
                    {
                        assert_eq!(previous.ankles[i].x, pose.ankles[i].x);
                    }
                }
                previous = pose;
            }
            assert!(right - left > 1.5, "No multi-step travel");
            assert!(jumped && crouched, "Missing flight or crouch");
            assert!(phrases.len() > 20, "Short repeating choreography");
        }
    }
}
