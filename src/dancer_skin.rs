use super::{Point, Skeleton, Surface, Vec3};
use crate::dance_space::Capsule;

#[derive(Clone, Copy, Default, PartialEq)]
pub enum Skin {
    #[default]
    Detailed,
    Silhouette,
}

const OUTLINE: u8 = 3;
const SKIN: u8 = 4;
const HAIR: u8 = 7;
const JACKET: u8 = 10;
const PANTS: u8 = 13;
const CYAN: u8 = 16;
const SHIRT: u8 = 19;
const WHITE: u8 = 22;
const LIPS: u8 = 25;

pub(super) fn palette() -> Vec<[u8; 3]> {
    vec![
        [0, 0, 0],
        [25, 35, 55],
        [47, 42, 75],
        [16, 17, 27],
        [183, 116, 91],
        [238, 177, 137],
        [255, 217, 174],
        [50, 28, 28],
        [94, 49, 39],
        [145, 80, 58],
        [75, 36, 120],
        [133, 66, 195],
        [188, 115, 239],
        [27, 30, 44],
        [49, 53, 70],
        [78, 84, 105],
        [13, 105, 151],
        [22, 180, 221],
        [117, 232, 246],
        [18, 21, 32],
        [30, 34, 47],
        [55, 61, 78],
        [127, 153, 173],
        [202, 221, 231],
        [243, 248, 250],
        [128, 54, 62],
        [204, 100, 108],
        [244, 154, 151],
    ]
}

impl Surface {
    fn paint(&mut self, material: u8, part: Capsule) {
        self.material = material;
        self.capsule(part.start, part.end, part.radii, false);
    }
}

pub(super) fn render(
    pose: &Skeleton,
    width: usize,
    height: usize,
    origin: Point,
    scale: f32,
) -> Surface {
    let mut surface = Surface::new(width, height, origin, scale);
    let chest = pose.shoulders[0].lerp(pose.shoulders[1], 0.5);
    let down = pose.waist.sub(chest).unit();
    let front = Vec3::new(0.0, 0.0, 1.0).yaw(pose.yaw);
    let body = pose.body_capsules();

    for side in 0..2 {
        let (hip, knee, ankle, toe) = (
            pose.hips[side],
            pose.knees[side],
            pose.ankles[side],
            pose.toes[side],
        );
        surface.paint(PANTS, Capsule::new(hip, knee, [0.058, 0.041]));
        let cuff = knee.lerp(ankle, 0.82);
        surface.paint(PANTS, Capsule::new(knee, cuff, [0.041, 0.025]));
        surface.paint(SKIN, Capsule::new(cuff, ankle, [0.019, 0.017]));
        surface.paint(CYAN, Capsule::new(ankle, toe, [0.028, 0.025]));
        let sole = Vec3::new(0.0, 0.021, 0.0);
        surface.paint(
            WHITE,
            Capsule::new(ankle.add(sole), toe.add(sole), [0.010; 2]),
        );
        surface.paint(
            WHITE,
            Capsule::new(
                ankle.lerp(toe, 0.25).add(Vec3::new(0.0, -0.012, 0.019)),
                ankle.lerp(toe, 0.65).add(Vec3::new(0.0, -0.010, 0.019)),
                [0.007; 2],
            ),
        );
    }
    surface.paint(PANTS, body[7]);
    surface.paint(
        SKIN,
        Capsule::new(
            chest.lerp(pose.waist, 0.86).add(front.scale(0.02)),
            pose.waist,
            [0.03, 0.045],
        ),
    );
    surface.paint(
        JACKET,
        Capsule::new(
            chest.add(down.scale(0.055)),
            chest.lerp(pose.waist, 0.64),
            [body[6].radii[0] * 0.78, 0.063],
        ),
    );
    surface.paint(
        SHIRT,
        Capsule::new(
            chest.lerp(pose.waist, 0.16).add(front.scale(0.064)),
            chest.lerp(pose.waist, 0.63).add(front.scale(0.046)),
            [0.044, 0.035],
        ),
    );
    // The opening and seams rotate with the torso instead of facing the screen.
    for side in [-1.0, 1.0] {
        let offset = Vec3::new(side * 0.047, 0.0, 0.068).yaw(pose.yaw);
        surface.paint(
            JACKET,
            Capsule::new(
                chest.add(offset),
                chest.lerp(pose.waist, 0.65).add(offset.scale(0.7)),
                [0.009, 0.007],
            ),
        );
    }
    let pelvis_front = Vec3::new(0.0, 0.0, 0.061).yaw(pose.hip_yaw);
    surface.paint(
        SHIRT,
        Capsule::new(
            pose.hips[0].lerp(pose.waist, 0.72).add(pelvis_front),
            pose.hips[1].lerp(pose.waist, 0.72).add(pelvis_front),
            [0.009; 2],
        ),
    );

    surface.paint(SKIN, Capsule::new(pose.neck, pose.head, [0.018; 2]));
    surface.material = SKIN;
    surface.head(pose.head, pose.head_width(), pose.head_roll);
    surface.paint(SKIN, pose.nose());
    for pair in pose.tail.windows(2) {
        surface.paint(HAIR, Capsule::new(pair[0], pair[1], [0.030, 0.022]));
    }
    let head = |x, y, z| pose.head_point(Vec3::new(x, y, z));
    surface.paint(
        HAIR,
        Capsule::new(
            head(0.0, -0.018, -0.025),
            head(0.0, 0.024, -0.025),
            [0.049, 0.033],
        ),
    );
    surface.paint(
        HAIR,
        Capsule::new(
            head(-0.025, -0.035, -0.014),
            head(0.025, -0.035, -0.014),
            [0.037; 2],
        ),
    );
    surface.paint(
        HAIR,
        Capsule::new(
            head(-0.030, -0.043, 0.035),
            head(0.025, -0.026, 0.051),
            [0.018, 0.014],
        ),
    );
    surface.paint(
        HAIR,
        Capsule::new(
            head(0.038, -0.018, 0.018),
            head(0.042, 0.023, 0.012),
            [0.016, 0.010],
        ),
    );
    for (a, b) in [
        ([-0.052, -0.008], [-0.044, -0.057]),
        ([-0.044, -0.057], [0.0, -0.069]),
        ([0.0, -0.069], [0.044, -0.057]),
        ([0.044, -0.057], [0.052, -0.008]),
    ] {
        surface.paint(
            CYAN,
            Capsule::new(head(a[0], a[1], 0.002), head(b[0], b[1], 0.002), [0.007; 2]),
        );
    }
    for side in [-1.0, 1.0] {
        surface.paint(
            CYAN,
            Capsule::new(
                head(side * 0.051, -0.004, -0.005),
                head(side * 0.051, 0.012, 0.008),
                [0.016; 2],
            ),
        );
        surface.paint(
            OUTLINE,
            Capsule::new(
                head(side * 0.019 - 0.005, -0.010, 0.052),
                head(side * 0.019 + 0.004, -0.008, 0.055),
                [0.004; 2],
            ),
        );
    }
    surface.paint(
        LIPS,
        Capsule::new(
            head(-0.005, 0.023, 0.050),
            head(0.006, 0.023, 0.050),
            [0.003; 2],
        ),
    );
    for side in 0..2 {
        let mut parts = pose.arm_parts(side, scale);
        surface.paint(JACKET, parts.next().unwrap());
        parts.next();
        let cuff = pose.elbows[side].lerp(pose.wrists[side], 0.72);
        surface.paint(
            JACKET,
            Capsule::new(pose.elbows[side], cuff, [0.031, 0.023]),
        );
        surface.paint(SKIN, Capsule::new(cuff, pose.wrists[side], [0.019, 0.018]));
        for hand in parts {
            surface.paint(SKIN, hand);
        }
    }
    surface
}

pub(super) fn color(surface: &Surface, x: usize, y: usize) -> u8 {
    let dot = surface.dots[y * surface.width + x];
    if dot.material == OUTLINE {
        return OUTLINE;
    }
    let left = x
        .checked_sub(1)
        .map(|x| surface.dots[y * surface.width + x]);
    let up = y
        .checked_sub(1)
        .map(|y| surface.dots[y * surface.width + x]);
    // A fixed light keeps the palette stable while the 3D surface turns.
    let slope = |other: Option<super::SurfaceDot>| {
        other
            .filter(|other| other.depth.is_finite() && other.material == dot.material)
            .map_or(0.0, |other| (dot.depth - other.depth).clamp(-2.0, 2.0))
    };
    let light = slope(left) * 0.55 + slope(up) * 0.8;
    dot.material
        + if light > 0.35 {
            2
        } else if light < -0.4 {
            0
        } else {
            1
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dance_motion::{Motion, Style};

    fn turned(pose: Skeleton, yaw: f32) -> Skeleton {
        let center = pose.shoulders[0].lerp(pose.shoulders[1], 0.5);
        let turn = |point: Vec3| point.sub(center).yaw(yaw).add(center);
        Skeleton {
            head: turn(pose.head),
            neck: turn(pose.neck),
            waist: turn(pose.waist),
            shoulders: pose.shoulders.map(turn),
            elbows: pose.elbows.map(turn),
            wrists: pose.wrists.map(turn),
            hips: pose.hips.map(turn),
            knees: pose.knees.map(turn),
            ankles: pose.ankles.map(turn),
            toes: pose.toes.map(turn),
            tail: pose.tail.map(turn),
            yaw: pose.yaw + yaw,
            hip_yaw: pose.hip_yaw + yaw,
            head_yaw: pose.head_yaw + yaw,
            ..pose
        }
    }

    #[test]
    fn turning_the_torso_does_not_expose_skin_through_clothing() {
        let mut pose = Motion::new(Style::Flow).spatial.pose;
        // Keep bare wrists below the chest so they cannot mask a torso leak.
        for side in 0..2 {
            pose.elbows[side] = pose.shoulders[side].add(Vec3::new(0.0, 0.16, 0.0));
            pose.wrists[side] = pose.shoulders[side].add(Vec3::new(0.0, 0.32, 0.0));
        }
        let origin = Point::new(120.0, 50.0);
        for yaw in [
            -std::f32::consts::FRAC_PI_2,
            -1.0,
            0.0,
            1.0,
            std::f32::consts::FRAC_PI_2,
            std::f32::consts::PI,
        ] {
            let pose = turned(pose, yaw);
            let surface = render(&pose, 240, 240, origin, 140.0);
            let chest = pose.shoulders[0].lerp(pose.shoulders[1], 0.5);
            for y in ((origin.y + (chest.y + 0.025) * 140.0) as usize)
                ..((origin.y + (chest.y + 0.075) * 140.0) as usize)
            {
                let center = (origin.x + chest.x * 140.0) as usize;
                assert!(
                    matches!(
                        surface.dots[y * surface.width + center].material,
                        JACKET | SHIRT
                    ),
                    "Missing chest clothing at yaw {yaw}, row {y}"
                );
                for x in ((origin.x + (chest.x - 0.11) * 140.0) as usize)
                    ..((origin.x + (chest.x + 0.11) * 140.0) as usize)
                {
                    assert_ne!(
                        surface.dots[y * surface.width + x].material,
                        SKIN,
                        "Exposed chest skin at yaw {yaw}, pixel ({x}, {y})"
                    );
                }
            }
        }
    }

    #[test]
    fn the_collar_leaves_the_neck_visible_below_the_chin() {
        let pose = Motion::new(Style::Flow).spatial.pose;
        let origin = Point::new(120.0, 50.0);
        let surface = render(&pose, 240, 240, origin, 140.0);
        let chest = pose.shoulders[0].lerp(pose.shoulders[1], 0.5);
        for fraction in [0.65, 0.75] {
            let neck = pose.head.lerp(chest, fraction);
            let x = (origin.x + neck.x * 140.0) as usize;
            let y = (origin.y + neck.y * 140.0) as usize;
            assert_eq!(
                surface.dots[y * surface.width + x].material,
                SKIN,
                "The collar hides the neck at {fraction}"
            );
        }
    }

    #[test]
    fn turning_the_head_occludes_the_face_with_hair() {
        let mut pose = Motion::new(Style::Flow).spatial.pose;
        let origin = Point::new(120.0, 50.0);
        for (yaw, expected) in [(0.0, SKIN), (std::f32::consts::PI, HAIR)] {
            pose.head_yaw = yaw;
            let surface = render(&pose, 240, 240, origin, 140.0);
            let x = (origin.x + pose.head.x * 140.0) as usize;
            let y = (origin.y + pose.head.y * 140.0) as usize;
            assert_eq!(surface.dots[y * surface.width + x].material, expected);
        }
    }
}
