use crate::dance_motion::{Point, Pose};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
    pub fn add(self, other: Self) -> Self {
        Self::new(self.x + other.x, self.y + other.y, self.z + other.z)
    }
    pub fn sub(self, other: Self) -> Self {
        self.add(other.scale(-1.0))
    }
    pub fn scale(self, t: f32) -> Self {
        Self::new(self.x * t, self.y * t, self.z * t)
    }
    pub fn dot(self, other: Self) -> f32 {
        self.x * other.x + self.y * other.y + self.z * other.z
    }
    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }
    pub fn unit(self) -> Self {
        self.scale(1.0 / self.length().max(0.00001))
    }
    pub fn lerp(self, other: Self, t: f32) -> Self {
        self.add(other.sub(self).scale(t))
    }
    pub fn project(self) -> Point {
        Point::new(self.x, self.y)
    }
    pub fn yaw(self, angle: f32) -> Self {
        Self::new(
            self.x * angle.cos() + self.z * angle.sin(),
            self.y,
            -self.x * angle.sin() + self.z * angle.cos(),
        )
    }
    fn pitch(self, angle: f32) -> Self {
        Self::new(
            self.x,
            self.y * angle.cos() - self.z * angle.sin(),
            self.y * angle.sin() + self.z * angle.cos(),
        )
    }
    fn roll(self, angle: f32) -> Self {
        Self::new(
            self.x * angle.cos() - self.y * angle.sin(),
            self.x * angle.sin() + self.y * angle.cos(),
            self.z,
        )
    }
}

// Positive Z points toward the viewer. Orthographic projection preserves stage contacts.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Skeleton {
    pub head: Vec3,
    pub neck: Vec3,
    pub waist: Vec3,
    pub shoulders: [Vec3; 2],
    pub elbows: [Vec3; 2],
    pub wrists: [Vec3; 2],
    pub wrist_bend: [f32; 2],
    pub hips: [Vec3; 2],
    pub knees: [Vec3; 2],
    pub ankles: [Vec3; 2],
    pub heels: [Vec3; 2],
    pub toes: [Vec3; 2],
    pub tail: [Vec3; 4],
    pub yaw: f32,
    pub hip_yaw: f32,
    pub head_yaw: f32,
    pub head_roll: f32,
    pub head_pitch: f32,
}

impl Skeleton {
    pub fn points(self) -> impl Iterator<Item = Vec3> {
        [self.head, self.neck, self.waist]
            .into_iter()
            .chain(self.shoulders)
            .chain(self.elbows)
            .chain(self.wrists)
            .chain(self.hips)
            .chain(self.knees)
            .chain(self.ankles)
            .chain(self.heels)
            .chain(self.toes)
            .chain(self.tail)
    }

    pub fn body_capsules(&self) -> [Capsule; 8] {
        let width = |front: f32, side: f32, yaw: f32| (front * yaw.cos()).hypot(side * yaw.sin());
        [
            Capsule::new(self.hips[0], self.knees[0], [0.055, 0.037]),
            Capsule::new(self.knees[0], self.ankles[0], [0.037, 0.017]),
            Capsule::new(self.heels[0], self.toes[0], [0.017, 0.014]),
            Capsule::new(self.hips[1], self.knees[1], [0.055, 0.037]),
            Capsule::new(self.knees[1], self.ankles[1], [0.037, 0.017]),
            Capsule::new(self.heels[1], self.toes[1], [0.017, 0.014]),
            Capsule::new(
                self.shoulders[0].lerp(self.shoulders[1], 0.5),
                self.waist,
                [width(0.092, 0.05, self.yaw), 0.053],
            ),
            Capsule::new(
                self.waist,
                self.hips[0].lerp(self.hips[1], 0.5),
                [0.053, width(0.088, 0.055, self.hip_yaw)],
            ),
        ]
    }

    pub fn body_parts(&self) -> impl Iterator<Item = Capsule> + '_ {
        self.body_capsules()
            .into_iter()
            .chain(std::iter::once(Capsule::new(
                self.neck, self.head, [0.018; 2],
            )))
            .chain(
                self.tail
                    .windows(2)
                    .map(|pair| Capsule::new(pair[0], pair[1], [0.021, 0.013])),
            )
    }

    pub fn arm_parts(&self, index: usize, scale: f32) -> impl Iterator<Item = Capsule> {
        let wrist = self.wrists[index];
        let forearm = wrist.sub(self.elbows[index]).unit();
        let side = if index == 0 { -1.0 } else { 1.0 };
        let across = Vec3::new(side * 0.45, 0.3, 1.0).yaw(self.yaw);
        let across = across.sub(forearm.scale(across.dot(forearm))).unit();
        let (sin, cos) = self.wrist_bend[index].sin_cos();
        let direction = forearm.scale(cos).add(across.scale(sin)).unit();
        [
            Capsule::new(self.shoulders[index], self.elbows[index], [0.036, 0.027]),
            Capsule::new(self.elbows[index], wrist, [0.028, 0.018]),
            Capsule::new(wrist, wrist.add(direction.scale(0.04)), [0.016, 0.024]),
            Capsule::new(
                wrist.add(direction.scale(0.012)),
                wrist
                    .add(direction.scale(0.025))
                    .add(Vec3::new(side * 0.025, 0.0, 0.0).yaw(self.yaw)),
                [0.009; 2],
            ),
        ]
        .into_iter()
        .take(if scale < 20.0 {
            2
        } else if scale < 28.0 {
            3
        } else {
            4
        })
    }

    pub fn head_width(&self) -> f32 {
        (0.047 * self.head_yaw.cos()).hypot(0.04 * self.head_yaw.sin())
    }

    pub fn nose(&self) -> Capsule {
        Capsule::new(
            self.head,
            self.head_point(Vec3::new(0.0, 0.005, 0.063)),
            [0.012, 0.009],
        )
    }

    #[cfg(test)]
    pub fn project(&self) -> Pose {
        Pose {
            head: self.head.project(),
            neck: self.neck.project(),
            waist: self.waist.project(),
            shoulders: self.shoulders.map(Vec3::project),
            elbows: self.elbows.map(Vec3::project),
            wrists: self.wrists.map(Vec3::project),
            hips: self.hips.map(Vec3::project),
            knees: self.knees.map(Vec3::project),
            ankles: self.ankles.map(Vec3::project),
            toes: self.toes.map(Vec3::project),
            tail: self.tail.map(Vec3::project),
            arm_depth: std::array::from_fn(|i| [self.elbows[i].z, self.wrists[i].z]),
        }
    }

    pub fn face(&self) -> [Vec3; 3] {
        [-0.019, 0.019, 0.0]
            .map(|x| self.head_point(Vec3::new(x, if x == 0.0 { 0.02 } else { -0.012 }, 0.052)))
    }

    pub fn head_point(&self, local: Vec3) -> Vec3 {
        self.head.add(
            local
                .yaw(self.head_yaw)
                .pitch(self.head_pitch)
                .roll(self.head_roll),
        )
    }
}

#[derive(Default)]
struct Angular {
    position: f32,
    velocity: f32,
}

impl Angular {
    fn follow(&mut self, target: f32, dt: f32) {
        self.follow_at(target, 3.0, 0.9, dt);
    }

    fn follow_at(&mut self, target: f32, omega: f32, speed: f32, dt: f32) {
        let (next, velocity) =
            crate::spring::advance(self.position, self.velocity, target, omega, dt);
        self.velocity = velocity;
        self.position += (next - self.position).clamp(-dt * speed, dt * speed);
    }
}

#[derive(Clone, Copy, Default)]
pub struct Cue {
    pub energy: f32,
    pub travel: f32,
    pub brace: f32,
    pub airborne: bool,
    pub swing: Option<(usize, f32)>,
    pub sliding: bool,
    pub turn: f32,
    pub look: f32,
    pub lumbar: f32,
    pub twist: f32,
    pub pitch: f32,
    pub depth: f32,
    pub foot_roll: [f32; 2],
    pub foot_target: Option<(f32, f32)>,
}

fn foot_points(base: Vec3, yaw: f32, roll: f32) -> [Vec3; 3] {
    let heel = Vec3::new(0.0, 0.012, -0.025);
    let toe = Vec3::new(0.0, 0.012, 0.055);
    let pivot = if roll >= 0.0 { toe } else { heel };
    [Vec3::default(), heel, toe].map(|point| {
        let point = if roll == 0.0 {
            point
        } else {
            pivot.add(point.sub(pivot).pitch(-roll))
        };
        base.add(point.yaw(yaw))
    })
}

struct FootSwing {
    foot: usize,
    depth: [f32; 2],
    yaw: [f32; 2],
}

#[derive(Default)]
pub struct Rig {
    pub pose: Skeleton,
    pelvis: Angular,
    lumbar: Angular,
    torso: Angular,
    head: Angular,
    pitch: Angular,
    depth_drive: Angular,
    phase: f32,
    foot_depth: [f32; 2],
    foot_yaw: [f32; 2],
    foot_roll: [Angular; 2],
    swing: Option<FootSwing>,
    arms: [Arm; 2],
    wrists: [Angular; 2],
    hair_yaw: Angular,
    hair_roll: Angular,
}

impl Rig {
    pub fn new(pose: &Pose) -> Self {
        let mut rig = Self::default();
        rig.advance(pose, Cue::default(), 0.0);
        rig
    }

    pub fn advance(&mut self, planar: &Pose, cue: Cue, dt: f32) {
        let hip2 = planar.hips[0].lerp(planar.hips[1], 0.5);
        let chest2 = planar.shoulders[0].lerp(planar.shoulders[1], 0.5);
        let support = planar.ankles[0].lerp(planar.ankles[1], 0.5);
        let stance = (planar.ankles[1].x - planar.ankles[0].x).abs().max(0.08);
        let weight = ((hip2.x - support.x) / stance).clamp(-0.6, 0.6);
        let reach = (planar.arm_depth[0][1] - planar.arm_depth[1][1]).clamp(-2.0, 2.0);
        self.phase = (self.phase + dt * (0.16 + cue.energy * 0.17)) % std::f32::consts::TAU;
        let turn = (self.phase.sin() * 0.95 + (self.phase * 2.0).sin() * 0.18) * cue.energy;
        let travel = cue.travel.clamp(-1.0, 1.0);
        let recovery = 1.0 - cue.brace;
        self.depth_drive.follow(cue.energy * recovery, dt);
        // Weight transfer leads the pelvis; the shoulder girdle counters it and
        // follows asymmetric reaches instead of turning the whole body as one block.
        let pivot = cue.turn.abs().min(1.0);
        let facing = (turn * (1.0 - travel.abs()) + travel * 1.2 + weight * cue.energy * 0.4)
            * (1.0 - pivot)
            + cue.turn;
        self.pelvis
            .follow_at(facing * recovery, 3.0 + 6.0 * pivot, 0.9 + 2.7 * pivot, dt);
        let twist = ((self.phase + 0.8).sin() * 0.3 + reach * 0.18 - weight * 0.5)
            .clamp(-0.65, 0.65)
            * cue.energy;
        self.torso.follow_at(
            (self.pelvis.position + twist * (1.0 - pivot * 0.55) + cue.twist) * recovery,
            3.0 + 5.0 * pivot,
            0.9 + 2.3 * pivot,
            dt,
        );
        let look = cue.look + travel * 0.18 + (facing - self.torso.position) * pivot * 0.32;
        let head_target =
            self.torso.position + (look + (self.phase - 0.8).sin() * cue.energy * 0.12) * recovery;
        self.head
            .follow_at(head_target.clamp(-1.5, 1.5), 5.5, 2.3, dt);
        self.pitch.follow_at(
            ((self.phase * 2.0).sin() * cue.energy * 0.12 + cue.pitch) * recovery,
            5.0,
            1.5,
            dt,
        );
        let lumbar = (weight * 0.17 * cue.energy + cue.lumbar).clamp(-0.26, 0.26);
        self.lumbar.follow_at(lumbar * recovery, 7.5, 1.7, dt);

        if !cue.airborne {
            if let Some((foot, progress)) = cue.swing {
                if self.swing.as_ref().is_none_or(|swing| swing.foot != foot) {
                    if let Some(previous) = self.swing.take() {
                        self.foot_depth[previous.foot] = previous.depth[1];
                        self.foot_yaw[previous.foot] = previous.yaw[1];
                    }
                    let side = if foot == 0 { -1.0 } else { 1.0 };
                    let (depth, yaw) = cue.foot_target.unwrap_or((
                        -side * self.pelvis.position.sin() * 0.075,
                        self.pelvis.position,
                    ));
                    self.swing = Some(FootSwing {
                        foot,
                        depth: [self.foot_depth[foot], depth],
                        yaw: [self.foot_yaw[foot], yaw],
                    });
                }
                let swing = self.swing.as_ref().unwrap();
                let position = if planar.ankles[foot].y == 0.97 && !cue.sliding {
                    f32::from(progress >= 0.5)
                } else {
                    crate::choreography::ease(progress)
                };
                let interpolate = |[from, to]: [f32; 2]| match position {
                    0.0 => from,
                    1.0 => to,
                    _ => from + (to - from) * position,
                };
                self.foot_depth[foot] = interpolate(swing.depth);
                self.foot_yaw[foot] = interpolate(swing.yaw);
            } else if let Some(swing) = self.swing.take() {
                self.foot_depth[swing.foot] = swing.depth[1];
                self.foot_yaw[swing.foot] = swing.yaw[1];
            }
        }
        let roll = (chest2.x - hip2.x).atan2(hip2.y - chest2.y);
        let frame = |p: Vec3| {
            p.yaw(self.torso.position)
                .pitch(self.pitch.position)
                .roll(roll)
        };
        let depth = (self.foot_depth[0] + self.foot_depth[1]) * 0.5 + cue.depth * 0.45;
        let mut hip = Vec3::new(hip2.x, hip2.y, depth);
        let hip_offsets = std::array::from_fn::<_, 2, _>(|i| {
            Vec3::new(planar.hips[i].x - hip2.x, planar.hips[i].y - hip2.y, 0.0)
                .yaw(self.pelvis.position)
        });
        let feet = std::array::from_fn::<_, 2, _>(|i| {
            let base = Vec3::new(planar.ankles[i].x, planar.ankles[i].y, self.foot_depth[i]);
            let target = cue.foot_roll[i] * recovery;
            let flex = &mut self.foot_roll[i];
            flex.follow_at(target, 18.0, 5.0, dt);
            if target == 0.0 && flex.position.abs() < 0.0001 && flex.velocity.abs() < 0.001 {
                *flex = Angular::default();
            }
            foot_points(base, self.foot_yaw[i], flex.position)
        });
        let ankles = feet.map(|[ankle, _, _]| ankle);
        // Solve height against both 3D contacts before solving knees; yaw cannot stretch legs.
        for i in 0..2 {
            let horizontal = (hip.x + hip_offsets[i].x - ankles[i].x).powi(2)
                + (hip.z + hip_offsets[i].z - ankles[i].z).powi(2);
            let minimum =
                ankles[i].y - (0.449_f32.powi(2) - horizontal).max(0.0).sqrt() - hip_offsets[i].y;
            hip.y = hip.y.max(minimum);
        }
        let waist = hip.add(
            Vec3::new(0.0, -0.0972, 0.0)
                .pitch(self.pitch.position)
                .roll(roll + self.lumbar.position),
        );
        let chest = waist.add(frame(Vec3::new(0.0, -0.1728, 0.0)));
        let head_roll = (planar.head.x - chest2.x).atan2(chest2.y - planar.head.y) - roll;
        let head = chest.add(
            Vec3::new(0.0, -0.102, 0.0)
                .yaw(self.head.position)
                .pitch(self.pitch.position)
                .roll(roll + head_roll),
        );
        let mut pose = Skeleton {
            head,
            neck: chest.add(frame(Vec3::new(0.0, -0.022, 0.0))),
            waist,
            hips: hip_offsets.map(|offset| hip.add(offset)),
            ankles,
            heels: feet.map(|[_, heel, _]| heel),
            toes: feet.map(|[_, _, toe]| toe),
            yaw: self.torso.position,
            hip_yaw: self.pelvis.position,
            head_yaw: self.head.position,
            head_roll: roll + head_roll,
            head_pitch: self.pitch.position,
            ..Skeleton::default()
        };
        let local =
            |point: Point| Vec3::new(point.x - chest2.x, point.y - chest2.y, 0.0).roll(-roll);
        for i in 0..2 {
            pose.shoulders[i] = chest.add(frame(local(planar.shoulders[i])));
            let knee_pole = Vec3::new(0.02, 0.0, 1.0).yaw(self.pelvis.position);
            let (knee, _) = chain(pose.hips[i], pose.ankles[i], knee_pole, 0.22, 0.23);
            pose.knees[i] = knee;
        }
        let parts = pose.body_capsules();
        let body = Body { parts: &parts };
        for (i, side) in [-1.0, 1.0].into_iter().enumerate() {
            let wrist = local(planar.wrists[i]);
            let wrist = Vec3::new(
                wrist.x,
                wrist.y,
                planar.arm_depth[i][1] * 0.15 * self.depth_drive.position,
            );
            let target = chest.add(frame(wrist));
            let pole = frame(local(planar.elbows[i]).sub(local(planar.shoulders[i])).add(
                Vec3::new(side * 0.025, 0.0, -0.065 * self.depth_drive.position),
            ));
            (pose.elbows[i], pose.wrists[i]) = self.arms[i].solve(
                pose.shoulders[i],
                target,
                pole,
                frame(Vec3::new(side, 0.2, 0.0)),
                &body,
                dt,
            );
            let current = pose.wrists[i].sub(pose.shoulders[i]);
            let previous = self.pose.wrists[i].sub(self.pose.shoulders[i]);
            let inertia = if dt > 0.0 {
                let delta = current.sub(previous);
                -(delta.y * 0.30 + delta.x * side * 0.22) / dt
            } else {
                0.0
            };
            self.wrists[i].follow_at(
                ((0.13 * cue.energy + inertia) * recovery).clamp(-0.46, 0.46),
                10.0,
                3.2,
                dt,
            );
            pose.wrist_bend[i] = self.wrists[i].position;
        }
        if dt > 0.0 {
            let yaw = -(pose.head_yaw - self.pose.head_yaw) * 0.32 / dt;
            let roll = -(pose.head.x - self.pose.head.x) * 0.85 / dt;
            self.hair_yaw
                .follow_at(yaw.clamp(-0.75, 0.75), 8.0, 4.0, dt);
            self.hair_roll
                .follow_at(roll.clamp(-0.65, 0.65), 8.0, 4.0, dt);
        }
        let tie = Vec3::new(0.0, -0.035, -0.045);
        pose.tail = std::array::from_fn(|i| {
            let flex = i as f32 / 3.0;
            let offset = Vec3::new(
                0.0,
                [0.0, 0.035, 0.08, 0.13][i],
                [0.0, -0.05, -0.065, -0.075][i],
            );
            pose.head_point(
                tie.add(
                    offset
                        .yaw(self.hair_yaw.position * flex)
                        .roll(self.hair_roll.position * flex),
                ),
            )
        });
        self.pose = pose;
    }
}

// A pole chooses the elbow/knee plane. Segment lengths remain fixed in world space.
fn chain(start: Vec3, target: Vec3, pole: Vec3, upper: f32, lower: f32) -> (Vec3, Vec3) {
    let delta = target.sub(start);
    let axis = if delta.length() < 0.00001 {
        Vec3::new(0.0, 1.0, 0.0)
    } else {
        delta.unit()
    };
    let distance = delta
        .length()
        .clamp((upper - lower).abs() + 0.0001, upper + lower - 0.0001);
    let along = (upper * upper - lower * lower + distance * distance) / (2.0 * distance);
    let mut bend = pole.sub(axis.scale(pole.dot(axis)));
    if bend.length() < 0.0001 {
        let fallback = if axis.z.abs() < 0.8 {
            Vec3::new(0.0, 0.0, 1.0)
        } else {
            Vec3::new(1.0, 0.0, 0.0)
        };
        bend = fallback.sub(axis.scale(fallback.dot(axis)));
    }
    (
        start.add(axis.scale(along)).add(
            bend.unit()
                .scale((upper * upper - along * along).max(0.0).sqrt()),
        ),
        start.add(axis.scale(distance)),
    )
}

#[derive(Clone, Copy)]
pub struct Capsule {
    pub start: Vec3,
    pub end: Vec3,
    pub radii: [f32; 2],
}

impl Capsule {
    pub fn new(start: Vec3, end: Vec3, radii: [f32; 2]) -> Self {
        Self { start, end, radii }
    }

    fn contact(&self, point: Vec3, padding: f32) -> (Vec3, f32) {
        let axis = self.end.sub(self.start);
        let delta = point.sub(self.start);
        let length2 = axis.dot(axis).max(0.00001);
        let along = delta.dot(axis) / length2;
        let perpendicular = delta.sub(axis.scale(along)).length();
        let taper = self.radii[1] - self.radii[0];
        let t = (along + taper * perpendicular / (length2 * (length2 - taper * taper)).sqrt())
            .clamp(0.0, 1.0);
        (
            self.start.lerp(self.end, t),
            self.radii[0] + taper * t + padding,
        )
    }
}

struct Body<'a> {
    parts: &'a [Capsule],
}

impl Body<'_> {
    fn contact(&self, point: Vec3, padding: f32) -> (Vec3, f32) {
        const BLEND_WIDTH: f32 = 0.008;
        let mut nearest = (Vec3::default(), 0.0);
        let mut clearance = f32::INFINITY;
        for part in self.parts {
            let (center, radius) = part.contact(point, padding);
            let delta = point.sub(center);
            let distance = delta.length();
            let gap = distance - radius;
            // A conservative smooth union avoids switching abruptly between
            // the pelvis and thighs. Staying outside it also clears every part.
            let blend = (0.5 + 0.5 * (gap - clearance) / BLEND_WIDTH).clamp(0.0, 1.0);
            if blend == 0.0 {
                clearance = gap;
                nearest = (center, radius);
            } else if blend < 1.0 {
                let previous = point.sub(nearest.0);
                let previous_distance = previous.length();
                let normal = previous
                    .unit()
                    .scale(blend)
                    .add(delta.unit().scale(1.0 - blend));
                let normal = if normal.length() < 0.00001 {
                    previous.unit()
                } else {
                    normal.unit()
                };
                clearance =
                    gap * (1.0 - blend) + clearance * blend - BLEND_WIDTH * blend * (1.0 - blend);
                let distance = previous_distance * blend + distance * (1.0 - blend);
                nearest = (point.sub(normal.scale(distance)), distance - clearance);
            }
        }
        nearest
    }

    fn clearance(&self, elbow: Vec3) -> f32 {
        let (center, radius) = self.contact(elbow, 0.028);
        elbow.sub(center).length() - radius
    }
}

#[derive(Default)]
struct Arm {
    upper: Vec3,
    lower: Vec3,
}

impl Arm {
    fn solve(
        &mut self,
        start: Vec3,
        target: Vec3,
        pole: Vec3,
        escape: Vec3,
        body: &Body<'_>,
        dt: f32,
    ) -> (Vec3, Vec3) {
        let (goal, wrist) = chain(start, target, pole, 0.16, 0.15);
        let direction = turn_toward(self.upper, goal.sub(start).unit(), dt);
        let previous = start.add(self.upper.scale(0.16));
        let escape = if self.upper.length() > 0.5 && body.clearance(previous) >= 0.0 {
            self.upper
        } else {
            escape
        };
        // The shoulder socket is embedded; constrain the exposed upper arm.
        let direction = Limb {
            length: 0.16,
            radii: [0.036, 0.028],
            from: 0.45,
        }
        .avoid_body(start, direction, escape, body);
        let elbow = start.add(direction.scale(0.16));
        let forearm = wrist.sub(elbow);
        let goal = if forearm.length() < 0.00001 {
            wrist.sub(goal).unit()
        } else {
            forearm.unit()
        };
        self.upper = elbow.sub(start).unit();
        let direction = turn_toward(self.lower, goal, dt);
        let escape = if self.lower.length() > 0.5 {
            self.lower
        } else {
            escape
        };
        self.lower = Limb {
            length: 0.15,
            radii: [0.028, 0.018],
            from: 0.1,
        }
        .avoid_body(elbow, direction, escape, body);
        (elbow, elbow.add(self.lower.scale(0.15)))
    }
}

fn turn_toward(previous: Vec3, goal: Vec3, dt: f32) -> Vec3 {
    if dt <= 0.0 || previous.length() < 0.5 {
        return goal;
    }
    let cosine = previous.dot(goal).clamp(-1.0, 1.0);
    let angle = cosine.acos();
    let step = dt * 6.0;
    if angle <= step {
        return goal;
    }
    let mut tangent = goal.sub(previous.scale(cosine));
    if tangent.length() < 0.00001 {
        let axis = if previous.y.abs() < 0.8 {
            Vec3::new(0.0, 1.0, 0.0)
        } else {
            Vec3::new(0.0, 0.0, 1.0)
        };
        tangent = axis.sub(previous.scale(axis.dot(previous)));
    }
    previous
        .scale(step.cos())
        .add(tangent.unit().scale(step.sin()))
        .unit()
}

struct Limb {
    length: f32,
    radii: [f32; 2],
    from: f32,
}

impl Limb {
    fn avoid_body(&self, start: Vec3, mut direction: Vec3, escape: Vec3, body: &Body<'_>) -> Vec3 {
        const CLEARANCE: f32 = 0.0005;
        for _ in 0..24 {
            let mut worst = (f32::INFINITY, Vec3::default(), 0.0, 0.0);
            for sample in 0..=8 {
                let t = self.from + (1.0 - self.from) * sample as f32 / 8.0;
                let length = self.length * t;
                let point = start.add(direction.scale(length));
                let padding = self.radii[0] + (self.radii[1] - self.radii[0]) * t;
                let (center, radius) = body.contact(point, padding);
                let gap = point.sub(center).length() - radius;
                if gap < worst.0 {
                    worst = (gap, center, radius, length);
                }
            }
            let (gap, center, radius, length) = worst;
            if gap >= CLEARANCE - 0.00001 {
                return direction;
            }
            // Project the most deeply intersecting limb section onto its contact
            // cone. The bone keeps its length and the exposed segment clears the body.
            let delta = center.sub(start);
            let distance = delta.length().max(0.00001);
            let axis = delta.scale(1.0 / distance);
            // Match the acceptance gap; extra pushout causes repeated retreat and re-entry.
            let radius = radius + CLEARANCE;
            let limit = ((length * length + distance * distance - radius * radius)
                / (2.0 * length * distance))
                .clamp(-1.0, 1.0);
            let mut tangent = direction.sub(axis.scale(direction.dot(axis)));
            if tangent.length() < 0.00001 {
                tangent = escape.sub(axis.scale(escape.dot(axis)));
            }
            if tangent.length() < 0.00001 {
                let fallback = if axis.y.abs() < 0.8 {
                    Vec3::new(0.0, 1.0, 0.0)
                } else {
                    Vec3::new(0.0, 0.0, 1.0)
                };
                tangent = fallback.sub(axis.scale(fallback.dot(axis)));
            }
            direction = axis
                .scale(limit)
                .add(tangent.unit().scale((1.0 - limit * limit).sqrt()))
                .unit();
        }
        direction
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dance_motion::{Motion, Style};

    #[test]
    fn shoe_roll_lifts_the_unloaded_end_around_a_fixed_contact() {
        for yaw in [-1.0, 0.0, 1.0] {
            let base = Vec3::new(0.25, 0.97, -0.11);
            let [ankle, heel, toe] = foot_points(base, yaw, 0.0);
            let [raised_ankle, raised_heel, fixed_toe] = foot_points(base, yaw, 0.5);
            assert_eq!(fixed_toe, toe);
            assert!(raised_heel.y < heel.y - 0.03);
            assert!(raised_ankle.y < ankle.y);
            let [_, fixed_heel, raised_toe] = foot_points(base, yaw, -0.4);
            assert_eq!(fixed_heel, heel);
            assert!(raised_toe.y < toe.y - 0.025);
        }
    }

    #[test]
    fn weight_transfers_and_reaches_twist_hips_and_shoulders_independently() {
        let planar = Motion::new(Style::Flow).pose;
        let mut rigs = std::array::from_fn::<_, 2, _>(|_| Rig::new(&planar));
        let feet = rigs[0].pose.ankles;
        for frame in 0..360 {
            for (i, side) in [-1.0, 1.0].into_iter().enumerate() {
                let mut shifted = planar;
                for hip in &mut shifted.hips {
                    hip.x += side * 0.04;
                }
                // Hold the slow facing phase still to isolate the weight transfer.
                rigs[i].phase = -(0.16 + 0.8 * 0.17) / 60.0;
                rigs[i].advance(
                    &shifted,
                    Cue {
                        energy: 0.8,
                        ..Cue::default()
                    },
                    1.0 / 60.0,
                );
                assert_eq!(
                    rigs[i].pose.ankles, feet,
                    "Twisting moves planted feet at {frame}"
                );
            }
        }
        assert!(
            rigs[1].pose.hip_yaw - rigs[0].pose.hip_yaw > 0.12,
            "Pelvis does not react to weight transfer"
        );
        assert!(
            rigs[0].pose.yaw - rigs[1].pose.yaw > 0.04,
            "Shoulders do not counter the weight transfer"
        );

        let mut rigs = std::array::from_fn::<_, 2, _>(|_| Rig::new(&planar));
        for _ in 0..360 {
            for (i, side) in [-1.0, 1.0].into_iter().enumerate() {
                let mut reach = planar;
                reach.arm_depth = [[0.0, side], [0.0, -side]];
                rigs[i].phase = -(0.16 + 0.8 * 0.17) / 60.0;
                rigs[i].advance(
                    &reach,
                    Cue {
                        energy: 0.8,
                        ..Cue::default()
                    },
                    1.0 / 60.0,
                );
                assert_eq!(rigs[i].pose.ankles, feet);
            }
        }
        assert!(
            rigs[1].pose.yaw - rigs[0].pose.yaw > 0.2,
            "Shoulders do not follow the reach"
        );
        assert!(
            (rigs[1].pose.hip_yaw - rigs[0].pose.hip_yaw).abs() < 0.00001,
            "Arm reach rotates the pelvis with the shoulders"
        );
    }

    #[test]
    fn a_slow_hand_sweep_past_the_hips_does_not_jerk() {
        let pose = Motion::new(Style::Flow).spatial.pose;
        let parts = pose.body_capsules();
        let body = Body { parts: &parts };
        let hip = pose.hips[0].lerp(pose.hips[1], 0.5);
        for (i, side) in [-1.0, 1.0].into_iter().enumerate() {
            for fps in [30, 60, 120] {
                let shoulder = pose.shoulders[i];
                let mut arm = Arm::default();
                let dt = 1.0 / fps as f32;
                let mut previous = None;
                let mut previous_velocity = None;
                let mut peak = (0.0_f32, 0);
                let mut touched = false;
                for frame in 0..20 * fps {
                    let phase = frame as f32 * dt * 0.7;
                    let target = hip.add(Vec3::new(
                        side * (0.075 - 0.09 * phase.sin()),
                        0.015 + 0.035 * phase.cos(),
                        0.045,
                    ));
                    let pole = Vec3::new(side * 0.08, 0.12, -0.04);
                    let (elbow, wrist) =
                        arm.solve(shoulder, target, pole, Vec3::new(side, 0.2, 0.0), &body, dt);
                    touched |= wrist.sub(target).length() > 0.025;
                    if let Some(before) = previous {
                        let velocity = wrist.sub(before).scale(1.0 / dt);
                        if let Some(before_velocity) = previous_velocity {
                            let acceleration = velocity.sub(before_velocity).length() / dt;
                            if frame > fps && acceleration > peak.0 {
                                peak = (acceleration, frame);
                            }
                        }
                        previous_velocity = Some(velocity);
                    }
                    assert!((elbow.sub(shoulder).length() - 0.16).abs() < 0.00001);
                    assert!((wrist.sub(elbow).length() - 0.15).abs() < 0.00001);
                    previous = Some(wrist);
                }
                assert!(touched, "Fixture never reaches body contact");
                assert!(
                    peak.0 < 8.0,
                    "Hip contact jerks arm={i} fps={fps}: acceleration={} frame={}",
                    peak.0,
                    peak.1,
                );
            }
        }
    }

    #[test]
    fn elbow_contact_keeps_the_joint_outside_the_torso_while_the_hand_moves() {
        let parts = [
            Capsule::new(Vec3::default(), Vec3::new(0.0, 0.173, 0.0), [0.092, 0.053]),
            Capsule::new(
                Vec3::new(0.0, 0.173, 0.0),
                Vec3::new(0.0, 0.27, 0.0),
                [0.053, 0.088],
            ),
        ];
        let body = Body { parts: &parts };
        for side in [-1.0, 1.0] {
            let shoulder = Vec3::new(side * 0.075, 0.0, 0.0);
            let pole = Vec3::new(-side * 0.2, 0.1, 0.0);
            let escape = Vec3::new(side, 0.2, 0.0);
            let mut contacted = false;
            let mut previous = None;
            let mut first_wrist = None;
            let mut last_wrist = Vec3::default();
            let mut arm = Arm::default();
            for frame in 0..121 {
                let target = Vec3::new(-side * (0.04 + frame as f32 * 0.0005), 0.12, 0.05);
                let (free_elbow, _) = chain(shoulder, target, pole, 0.16, 0.15);
                contacted |= body.clearance(free_elbow) < 0.0;
                let (elbow, wrist) = arm.solve(shoulder, target, pole, escape, &body, 1.0 / 120.0);
                assert!(
                    body.clearance(elbow) >= -0.00001,
                    "Elbow enters torso: {elbow:?}"
                );
                assert!((elbow.sub(shoulder).length() - 0.16).abs() < 0.00001);
                assert!((wrist.sub(elbow).length() - 0.15).abs() < 0.00001);
                if let Some((old_elbow, old_wrist)) = previous {
                    assert!(elbow.sub(old_elbow).length() < 0.01);
                    assert!(wrist.sub(old_wrist).length() < 0.01);
                }
                previous = Some((elbow, wrist));
                first_wrist.get_or_insert(wrist);
                last_wrist = wrist;
            }
            assert!(contacted, "Fixture never touches the torso");
            assert!(
                last_wrist.sub(first_wrist.unwrap()).length() > 0.02,
                "Hand freezes at elbow contact"
            );
        }
    }

    #[test]
    fn elbow_contacts_work_through_turns_and_cross_body_reaches() {
        let mut planar = Motion::new(Style::Flow).pose;
        let chest = planar.shoulders[0].lerp(planar.shoulders[1], 0.5);
        for yaw in [-1.5, -0.75, 0.0, 0.75, 1.5] {
            let mut rig = Rig::new(&planar);
            rig.torso.position = yaw;
            rig.pelvis.position = yaw;
            rig.depth_drive.position = 0.8;
            rig.advance(&planar, Cue::default(), 0.0);
            let mut previous: Option<Skeleton> = None;
            for frame in 0..720 {
                let phase = frame as f32 * std::f32::consts::TAU / 720.0;
                for (i, side) in [-1.0, 1.0].into_iter().enumerate() {
                    planar.wrists[i] =
                        chest.offset(side * 0.2 * phase.sin(), 0.12 + 0.08 * phase.cos());
                    planar.elbows[i] = planar.shoulders[i].offset(-side * 0.1, 0.12);
                    planar.arm_depth[i] = [0.0, 0.8 * phase.sin()];
                }
                rig.advance(
                    &planar,
                    Cue {
                        energy: 0.8,
                        ..Cue::default()
                    },
                    1.0 / 120.0,
                );
                let pose = rig.pose;
                let parts = pose.body_capsules();
                let body = Body { parts: &parts };
                for i in 0..2 {
                    assert!(
                        body.clearance(pose.elbows[i]) >= -0.00001,
                        "Contact fails at yaw={yaw} frame={frame}"
                    );
                    assert!(
                        (pose.elbows[i].sub(pose.shoulders[i]).length() - 0.16).abs() < 0.00001
                    );
                    assert!((pose.wrists[i].sub(pose.elbows[i]).length() - 0.15).abs() < 0.00001);
                    if let Some(before) = previous {
                        assert!(
                            pose.elbows[i].sub(before.elbows[i]).length() < 0.015,
                            "Contact elbow snaps"
                        );
                        assert!(
                            pose.wrists[i].sub(before.wrists[i]).length() < 0.015,
                            "Contact hand snaps"
                        );
                    }
                }
                previous = Some(pose);
            }
        }
    }

    #[test]
    fn spatial_chains_keep_real_lengths_while_projection_foreshortens_them() {
        let start = Vec3::default();
        for target in [
            Vec3::new(0.3, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 0.3),
            Vec3::new(-0.2, -0.1, -0.2),
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 2.0),
        ] {
            let (elbow, wrist) = chain(start, target, Vec3::new(0.0, 1.0, 0.0), 0.16, 0.15);
            assert!((elbow.sub(start).length() - 0.16).abs() < 0.00001);
            assert!((wrist.sub(elbow).length() - 0.15).abs() < 0.00001);
        }
        let (elbow, wrist) = chain(
            start,
            Vec3::new(0.0, 0.0, 0.3),
            Vec3::new(0.0, 1.0, 0.0),
            0.16,
            0.15,
        );
        assert!(elbow.project().distance(wrist.project()) < 0.05);
        assert!(wrist.z > 0.29);
    }

    #[test]
    fn a_profile_changes_geometry_without_rotating_planted_feet() {
        let planar = Motion::new(Style::Flow).pose;
        let mut rig = Rig::new(&planar);
        let front = rig.pose;
        rig.pelvis.position = std::f32::consts::FRAC_PI_2;
        rig.torso.position = std::f32::consts::FRAC_PI_2;
        rig.head.position = std::f32::consts::FRAC_PI_2;
        rig.advance(&planar, Cue::default(), 0.0);
        let profile = rig.pose;
        assert_eq!(front.ankles, profile.ankles);
        assert!(front.project().shoulders[0].distance(front.project().shoulders[1]) > 0.149);
        assert!(profile.project().shoulders[0].distance(profile.project().shoulders[1]) < 0.001);
        assert!((profile.shoulders[0].z - profile.shoulders[1].z).abs() > 0.149);
        assert!(profile.face()[2].x > profile.head.x + 0.05);
        for i in 0..2 {
            assert!((profile.knees[i].sub(profile.hips[i]).length() - 0.22).abs() < 0.00001);
            assert!((profile.ankles[i].sub(profile.knees[i]).length() - 0.23).abs() < 0.00001);
        }
    }

    #[test]
    fn turning_retains_contacts_and_angles_across_a_loading_transition() {
        let planar = Motion::new(Style::Flow).pose;
        let mut rig = Rig::new(&planar);
        let feet = rig.pose.ankles;
        let mut turn = 0.0_f32;
        for frame in 0..3600 {
            let before = rig.pose;
            rig.advance(
                &planar,
                Cue {
                    energy: 0.8,
                    ..Cue::default()
                },
                1.0 / 60.0,
            );
            turn = turn.max(rig.pose.yaw.abs());
            assert_eq!(rig.pose.ankles, feet);
            assert!((rig.pose.yaw - before.yaw).abs() <= 0.9 / 60.0 + 0.00001);
            assert!(
                rig.pose.head.sub(before.head).length() < 0.02,
                "Frame {frame}"
            );
        }
        assert!(turn > 0.9);
        let before = rig.pose;
        rig.advance(&planar, Cue::default(), 1.0 / 60.0);
        assert!(rig.pose.head.sub(before.head).length() < 0.02);
        assert_eq!(rig.pose.ankles, feet);
    }
}
